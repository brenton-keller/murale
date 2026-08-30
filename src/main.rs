#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod egl;
mod error;
mod mem;
mod mpv;
mod stats;
mod workspace;

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use khronos_egl as kegl;
use libmpv2::render::RenderContext;
use libmpv2::Mpv;
use nix::poll::{PollFd, PollFlags, PollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};

use mpv::{render_nonblocking, SendPtr, SharedMpvPtr};
use stats::FrameStats;
use workspace::spawn_workspace_listener;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);
static COMMIT_HELD: AtomicBool = AtomicBool::new(false);
static RELEASE_SIGNAL: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

extern "C" fn handle_refresh_signal(_sig: libc::c_int) {
    REFRESH_REQUESTED.store(true, Ordering::Relaxed);
}

extern "C" fn handle_release_signal(_sig: libc::c_int) {
    RELEASE_SIGNAL.store(true, Ordering::Relaxed);
}

const MPV_REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 3600);

// --- mpv options parsing ---

fn parse_mpv_options(opts: &str) -> Vec<(&str, &str)> {
    opts.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|opt| {
            if let Some((key, value)) = opt.split_once('=') {
                (key.trim(), value.trim())
            } else if opt == "loop" {
                ("loop-file", "inf")
            } else {
                (opt, "yes")
            }
        })
        .collect()
}

// --- CLI ---

#[derive(Parser)]
#[command(
    about = "Lean, memory-safe video wallpaper player for Wayland compositors",
    after_help = "\
EXAMPLES:
    murale ~/Videos/wallpaper.mp4
    murale ~/Videos/wallpaper.mp4 --output DP-2
    murale ~/Videos/wallpaper.mp4 --mpv-options \"input-ipc-server=/tmp/murale.sock\"
    murale --workspace 1=~/Videos/forest.mp4 --workspace 2=~/Videos/ocean.mp4 ~/Videos/default.mp4
    murale ~/Videos/wallpaper.mp4 --stats"
)]
struct Cli {
    /// Path to video file (any format mpv supports). Used as the default/fallback
    /// when no --workspace mapping matches.
    video: String,

    /// Target a specific display output by name (e.g. DP-1, HDMI-A-1).
    /// Run without this flag to see available outputs listed in the log.
    #[arg(short, long)]
    output: Option<String>,

    /// Map a Hyprland workspace to a video. Can be repeated.
    /// When switching to a mapped workspace, the corresponding video loads.
    /// Unmapped workspaces use the default video.
    #[arg(short, long, value_name = "N=PATH")]
    workspace: Vec<String>,

    /// Pass options to mpv as comma-separated key=value pairs. Bare flags are
    /// set to "yes" (loop is special-cased to loop-file=inf).
    /// Example: --mpv-options "input-ipc-server=/tmp/murale.sock,hwdec=auto"
    #[arg(long, value_name = "OPTIONS")]
    mpv_options: Option<String>,

    /// Log frame timing and mpv playback stats every 5 seconds
    #[arg(long)]
    stats: bool,

    /// Log wp_presentation scanout timestamps (per-frame, capped at 60s)
    #[arg(long)]
    scanout: bool,

    /// Suppress internal periodic refresh (supervisor manages lifecycle)
    #[arg(long)]
    supervised: bool,
}

// --- Presentation timing (scanout instrumentation) ---

struct PresentationMeta {
    frame_count: u64,
    media_pos: f64,
    swap_mono_ns: u64,
    is_held_release: bool,
}

// --- EGL lifecycle wrapper ---

pub(crate) struct EglCore {
    pub(crate) config: kegl::Config,
    pub(crate) context: kegl::Context,
}

// --- Main state ---

struct Murale {
    // SCTK state
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    qh: QueueHandle<Self>,

    // Layer surface — None when output is disconnected
    layer: Option<LayerSurface>,
    target_output_name: Option<String>,

    // EGL display handle survives refreshes (same handle, re-initialized).
    // EGL config/context are rebuilt on each refresh (wrapped in EglCore).
    // Only the per-surface EglState is recreated on output reconnect.
    egl_instance: Arc<egl::EglInstance>,
    egl_state: Option<egl::EglState>,
    egl_display: kegl::Display,
    egl_core: Option<EglCore>,

    // SAFETY: render_ctx MUST be declared before mpv — see transmute comment in init_mpv().
    render_ctx: Option<RenderContext<'static>>,
    mpv: Option<Mpv>,

    // Shared mpv handle pointer — workspace thread locks this for the
    // duration of each mpv_command call. refresh_mpv locks it to swap
    // the pointer, providing mutual exclusion.
    mpv_ptr: SharedMpvPtr,

    // mpv wakeup eventfd
    wakeup_fd: EventFd,

    // Frame state
    initialized: bool,
    first_configure: bool,
    width: u32,
    height: u32,
    scale: u32,
    frame_callback_pending: bool,
    redraw_needed: bool,

    // Two-phase readiness for supervisor handoff
    first_swap_done: bool,
    ready_signaled: bool,

    // Hidden sync: hold commit until released via SIGUSR2
    hold_ready_signaled: bool,
    hold_render_gen: u64,
    target_render_armed: bool,

    // Periodic mpv refresh to counter residual GPU memory leak
    last_mpv_refresh: Instant,
    refresh_pending: bool,

    // Instrumentation
    stats_enabled: bool,
    scanout_enabled: bool,
    scanout_start: Option<Instant>,
    scanout_count: u64,
    frame_stats: FrameStats,
    frame_count: u64,
    last_mem_log: Instant,

    // Loop restart detection via time-pos backward jump
    loop_count: u64,
    last_time_pos: f64,

    // Control
    supervised: bool,
    exit: bool,
    video_path: String,
    mpv_options: Option<String>,

    // Workspace config — kept for intent replay during refresh
    workspace_config: Option<(HashMap<String, String>, String)>,

    // Scanout instrumentation (wp_presentation)
    presentation: Option<wp_presentation::WpPresentation>,

    // Raw wl_display pointer for mpv
    wl_display_ptr: *mut c_void,
}

impl Murale {
    fn create_layer_surface(&mut self, output: Option<&wl_output::WlOutput>) {
        let surface = self.compositor_state.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Background,
            Some("murale"),
            output,
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        self.layer = Some(layer);
        self.first_configure = true;
        self.frame_callback_pending = false;
        self.redraw_needed = false;
    }

    fn teardown_surface(&mut self) {
        if let Some(state) = self.egl_state.take() {
            if let Err(e) = state.destroy(&self.egl_instance, self.egl_display) {
                tracing::warn!("EGL surface teardown error: {e}");
            }
        }
        self.layer.take();
        self.frame_callback_pending = false;
        self.redraw_needed = false;
        if let Some(mpv) = &self.mpv {
            let _ = mpv.set_property("pause", true);
        }
        tracing::info!("surface torn down, playback paused");
    }

    fn render_frame(&mut self) {
        let egl_state = match &self.egl_state {
            Some(s) => s,
            None => return,
        };
        let core = match &self.egl_core {
            Some(c) => c,
            None => return,
        };
        let render_ctx = match &self.render_ctx {
            Some(ctx) => ctx,
            None => return,
        };
        let layer = match &self.layer {
            Some(l) => l,
            None => return,
        };

        let w = (self.width * self.scale) as i32;
        let h = (self.height * self.scale) as i32;

        if let Err(e) = self.egl_instance.make_current(
            self.egl_display,
            Some(egl_state.surface),
            Some(egl_state.surface),
            Some(core.context),
        ) {
            tracing::error!("eglMakeCurrent failed: {e:?}");
            self.redraw_needed = true;
            return;
        }

        unsafe {
            gl::DrawBuffer(gl::BACK);
            gl::Viewport(0, 0, w, h);
        }

        let err = render_nonblocking(render_ctx.ctx, w, h);
        if err < 0 {
            tracing::error!("mpv render failed: {err}");
            self.redraw_needed = true;
            return;
        }

        if COMMIT_HELD.load(Ordering::Relaxed) {
            self.hold_render_gen += 1;
            self.frame_count += 1;
            self.target_render_armed = false;

            if !self.hold_ready_signaled {
                self.signal_hold_ready();
            }
            self.write_target_rendered();
            return;
        }

        if self.stats_enabled {
            self.frame_stats.record(Instant::now());
            if self.frame_stats.should_report() {
                let (avg, min, max, jitter) = self.frame_stats.take_stats();
                self.log_combined_stats(avg, min, max, jitter);
            }
        }

        let media_pos = if self.scanout_enabled && self.presentation.is_some() {
            self.mpv
                .as_ref()
                .and_then(|m| m.get_property::<f64>("time-pos").ok())
                .unwrap_or(-1.0)
        } else {
            -1.0
        };

        layer
            .wl_surface()
            .frame(&self.qh, layer.wl_surface().clone());
        self.frame_callback_pending = true;
        self.redraw_needed = false;

        if let Err(e) = self
            .egl_instance
            .swap_buffers(self.egl_display, egl_state.surface)
        {
            tracing::error!("eglSwapBuffers failed: {e:?}");
            self.frame_callback_pending = false;
            self.redraw_needed = true;
            return;
        }

        unsafe {
            libmpv2_sys::mpv_render_context_report_swap(render_ctx.ctx);
        }

        if !self.first_swap_done {
            self.first_swap_done = true;
        }

        self.frame_count += 1;
        if self.stats_enabled && self.frame_count.is_multiple_of(1000) {
            mem::log_rss(&format!("frame#{}", self.frame_count));
        }
        if self.scanout_enabled {
            self.request_presentation_feedback(media_pos, false);
        }
    }

    fn log_combined_stats(&self, avg: f64, min: f64, max: f64, jitter: f64) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };
        let fps: f64 = mpv.get_property("estimated-vf-fps").unwrap_or(0.0);
        let delayed: i64 = mpv.get_property("vo-delayed-frame-count").unwrap_or(-1);
        let vo_drops: i64 = mpv.get_property("frame-drop-count").unwrap_or(-1);
        let dec_drops: i64 = mpv.get_property("decoder-frame-drop-count").unwrap_or(-1);
        let vsync_ratio: f64 = mpv.get_property("vsync-ratio").unwrap_or(0.0);
        tracing::info!(
            "[stats] fps={fps:.1} delayed={delayed} vo_drops={vo_drops} dec_drops={dec_drops} vsync_ratio={vsync_ratio:.2} | interval avg={avg:.2}ms min={min:.2}ms max={max:.2}ms jitter={jitter:.2}ms"
        );

        mem::log_rss("stats-interval");

        let cache_dur: f64 = mpv.get_property("demuxer-cache-duration").unwrap_or(-1.0);
        let vp_w: i64 = mpv.get_property("video-out-params/w").unwrap_or(-1);
        let vp_h: i64 = mpv.get_property("video-out-params/h").unwrap_or(-1);
        let vp_fmt: String = mpv
            .get_property("video-out-params/pixelformat")
            .unwrap_or_default();
        let elapsed = mem::elapsed_secs();
        tracing::info!(
            "[mpv-props] cache_dur={cache_dur:.1}s vout={vp_w}x{vp_h} pixfmt={vp_fmt} elapsed={elapsed:.0}s"
        );
    }

    fn handle_mpv_wakeup(&mut self) {
        let _ = self.wakeup_fd.read();

        let render_ctx = match &self.render_ctx {
            Some(ctx) => ctx,
            None => return,
        };

        let flags = match render_ctx.update() {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("mpv render update failed: {e}");
                return;
            }
        };

        if flags & libmpv2::render::mpv_render_update::Frame != 0 {
            if COMMIT_HELD.load(Ordering::Relaxed) {
                if !self.target_render_armed {
                    if self.hold_render_gen == 0 {
                        self.target_render_armed = true;
                    } else {
                        self.check_arm_file();
                    }
                }
                if self.target_render_armed {
                    self.render_frame();
                }
                return;
            }
            if self.frame_callback_pending {
                self.redraw_needed = true;
            } else {
                self.render_frame();
            }
        }

        // Detect loop restarts via time-pos backward jump.
        // loop-file=inf seeks to start without firing END_FILE.
        if let Some(mpv) = &self.mpv {
            if let Ok(pos) = mpv.get_property::<f64>("time-pos") {
                if self.last_time_pos > 5.0 && pos < self.last_time_pos * 0.2 {
                    self.loop_count += 1;
                    let (rss, swap) = mem::read_rss_raw();
                    let total = rss + swap;
                    let elapsed = mem::elapsed_secs();
                    tracing::info!(
                        "[mem] loop#{} rss={rss:.1}MB swap={swap:.1}MB total={total:.1}MB elapsed={elapsed:.0}s",
                        self.loop_count
                    );
                }
                self.last_time_pos = pos;
            }
        }
    }

    fn signal_hold_ready(&mut self) {
        if let Ok(path) = std::env::var("MURALE_HOLD_READY_FILE") {
            let gen = std::env::var("MURALE_GENERATION").unwrap_or_default();
            let content = format!("HOLD_READY:{}:{}", std::process::id(), gen);
            match std::fs::write(&path, &content) {
                Ok(()) => {
                    tracing::info!("hold readiness signaled: {path}");
                    self.hold_ready_signaled = true;
                }
                Err(e) => tracing::error!("failed to write hold ready: {e}"),
            }
        } else {
            self.hold_ready_signaled = true;
        }
    }

    fn write_target_rendered(&self) {
        if let Ok(path) = std::env::var("MURALE_TARGET_RENDERED_FILE") {
            let pos = self
                .mpv
                .as_ref()
                .and_then(|m| m.get_property::<f64>("time-pos").ok())
                .unwrap_or(-1.0);
            let content = format!("{}:{:.6}", self.hold_render_gen, pos);
            if let Err(e) = std::fs::write(&path, &content) {
                tracing::error!("failed to write target rendered: {e}");
            }
        }
    }

    fn check_arm_file(&mut self) {
        let Ok(path) = std::env::var("MURALE_ARM_RENDER_FILE") else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some((target_str, _seq)) = content.trim().split_once(':') else {
            return;
        };
        let Ok(expected) = target_str.parse::<f64>() else {
            return;
        };
        let Some(mpv) = &self.mpv else { return };
        let Ok(pos) = mpv.get_property::<f64>("time-pos") else {
            return;
        };
        let frame_time = 1.0 / 240.0;
        if (pos - expected).abs() < frame_time {
            let _ = std::fs::remove_file(&path);
            self.target_render_armed = true;
        }
    }

    fn present_held_frame(&mut self) {
        let egl_state = match &self.egl_state {
            Some(s) => s,
            None => return,
        };
        let render_ctx = match &self.render_ctx {
            Some(ctx) => ctx,
            None => return,
        };
        let layer = match &self.layer {
            Some(l) => l,
            None => return,
        };
        let core = match &self.egl_core {
            Some(c) => c,
            None => return,
        };

        if let Err(e) = self.egl_instance.make_current(
            self.egl_display,
            Some(egl_state.surface),
            Some(egl_state.surface),
            Some(core.context),
        ) {
            tracing::error!("held make_current failed: {e:?}");
            return;
        }

        layer
            .wl_surface()
            .frame(&self.qh, layer.wl_surface().clone());
        self.frame_callback_pending = true;

        if let Err(e) = self
            .egl_instance
            .swap_buffers(self.egl_display, egl_state.surface)
        {
            tracing::error!("held swap failed: {e:?}");
            self.frame_callback_pending = false;
            return;
        }

        unsafe {
            libmpv2_sys::mpv_render_context_report_swap(render_ctx.ctx);
        }

        if !self.first_swap_done {
            self.first_swap_done = true;
        }
        self.frame_count += 1;

        if self.scanout_enabled {
            let held_pos = self
                .mpv
                .as_ref()
                .and_then(|m| m.get_property::<f64>("time-pos").ok())
                .unwrap_or(-1.0);
            self.request_presentation_feedback(held_pos, true);
        }

        if let Ok(path) = std::env::var("MURALE_SWAP_DONE_FILE") {
            let gen = std::env::var("MURALE_GENERATION").unwrap_or_default();
            let content = format!("{}:{}", std::process::id(), gen);
            let _ = std::fs::write(&path, &content);
        }

        tracing::info!("held frame presented (swap done)");
    }

    fn request_presentation_feedback(&mut self, media_pos: f64, is_held_release: bool) {
        if !self.scanout_enabled {
            return;
        }
        let start = *self.scanout_start.get_or_insert_with(Instant::now);
        if start.elapsed() >= Duration::from_secs(60) {
            tracing::info!(
                "[scanout] capped after 60s ({} frames logged), stopping",
                self.scanout_count
            );
            self.scanout_enabled = false;
            return;
        }
        self.scanout_count += 1;
        let presentation = match &self.presentation {
            Some(p) => p,
            None => return,
        };
        let layer = match &self.layer {
            Some(l) => l,
            None => return,
        };
        let swap_mono_ns = {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
            ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
        };
        let meta = PresentationMeta {
            frame_count: self.frame_count,
            media_pos,
            swap_mono_ns,
            is_held_release,
        };
        let _ = presentation.feedback(layer.wl_surface(), &self.qh, meta);
    }

    fn find_target_output(&self, name: &str) -> Option<wl_output::WlOutput> {
        for output in self.output_state.outputs() {
            if let Some(info) = self.output_state.info(&output) {
                if info.name.as_deref() == Some(name) {
                    return Some(output);
                }
            }
        }
        None
    }
}

// --- SCTK handler implementations ---

impl CompositorHandler for Murale {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor as u32;
        tracing::debug!("scale factor changed to {new_factor}");
        if let Some(egl_state) = &self.egl_state {
            let scaled_w = (self.width * self.scale) as i32;
            let scaled_h = (self.height * self.scale) as i32;
            egl_state.egl_window.resize(scaled_w, scaled_h, 0, 0);
            self.redraw_needed = true;
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.frame_callback_pending = false;

        // VSync-aligned release: send SIGUSR2 to B from A's frame callback.
        // One-shot — read PID from file, signal, delete file.
        if let Ok(path) = std::env::var("MURALE_RELEASE_TARGET_FILE") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(pid) = content.trim().parse::<i32>() {
                    unsafe { libc::kill(pid, libc::SIGUSR2) };
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        if self.first_swap_done && !self.ready_signaled {
            if let Ok(path) = std::env::var("MURALE_READY_FILE") {
                let content = format!(
                    "{}:{}",
                    std::process::id(),
                    std::env::var("MURALE_GENERATION").unwrap_or_default()
                );
                match std::fs::write(&path, &content) {
                    Ok(()) => {
                        tracing::info!("readiness signaled: {path}");
                        self.ready_signaled = true;
                    }
                    Err(e) => tracing::error!("failed to write ready file: {e}"),
                }
            } else {
                self.ready_signaled = true;
            }
        }

        if self.redraw_needed {
            self.render_frame();
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        let name = self
            .output_state
            .info(output)
            .and_then(|i| i.name.clone())
            .unwrap_or_else(|| "(unknown)".into());
        tracing::info!("surface entered output {name}");
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        let name = self
            .output_state
            .info(output)
            .and_then(|i| i.name.clone())
            .unwrap_or_else(|| "(unknown)".into());
        tracing::warn!("surface left output {name}");
    }
}

impl OutputHandler for Murale {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let target = match &self.target_output_name {
            Some(name) => name.clone(),
            None => return,
        };

        let name = self.output_state.info(&output).and_then(|i| i.name.clone());

        if name.as_deref() == Some(&target) && self.initialized && self.layer.is_none() {
            tracing::info!("output {target} reconnected, recreating surface");
            self.create_layer_surface(Some(&output));
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let name = self
            .output_state
            .info(&output)
            .and_then(|i| i.name.clone())
            .unwrap_or_else(|| "(unknown)".into());
        tracing::debug!("output updated: {name}");
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let target = match &self.target_output_name {
            Some(name) => name,
            None => return,
        };

        let name = self.output_state.info(&output).and_then(|i| i.name.clone());

        if name.as_deref() == Some(target.as_str()) {
            tracing::info!("output {target} disconnected");
            self.teardown_surface();
        }
    }
}

impl LayerShellHandler for Murale {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        tracing::warn!("layer surface closed by compositor");
        self.teardown_surface();
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let w = configure.new_size.0;
        let h = configure.new_size.1;

        if w == 0 || h == 0 {
            return;
        }

        self.width = w;
        self.height = h;
        tracing::debug!("configure: {w}x{h} scale={}", self.scale);

        if self.first_configure {
            self.first_configure = false;

            let scaled_w = (w * self.scale) as i32;
            let scaled_h = (h * self.scale) as i32;

            let layer = match &self.layer {
                Some(l) => l,
                None => return,
            };

            let core = match &self.egl_core {
                Some(c) => c,
                None => {
                    tracing::error!("no EGL core during configure");
                    self.exit = true;
                    return;
                }
            };

            match egl::create_surface(
                &self.egl_instance,
                self.egl_display,
                core.config,
                core.context,
                layer.wl_surface(),
                scaled_w,
                scaled_h,
            ) {
                Ok(state) => self.egl_state = Some(state),
                Err(e) => {
                    tracing::error!("failed to create EGL surface: {e}");
                    self.exit = true;
                    return;
                }
            }

            if self.mpv.is_none() {
                if let Err(e) = self.init_mpv() {
                    tracing::error!("failed to initialize mpv: {e}");
                    self.exit = true;
                    return;
                }
                self.publish_mpv_handle();
            } else {
                if let Some(mpv) = &self.mpv {
                    let _ = mpv.set_property("pause", false);
                }
                tracing::info!("surface recreated, resuming playback");
            }

            self.initialized = true;

            // R2-B4: if a deferred refresh is pending, run it now and return.
            // refresh_mpv handles its own kick-start rendering, so we must NOT
            // fall through to the render_frame() call below.
            if self.refresh_pending {
                self.refresh_pending = false;
                self.refresh_mpv();
                return;
            }

            self.render_frame();
        } else if let Some(egl_state) = &self.egl_state {
            let scaled_w = (w * self.scale) as i32;
            let scaled_h = (h * self.scale) as i32;
            egl_state.egl_window.resize(scaled_w, scaled_h, 0, 0);
        }
    }
}

delegate_compositor!(Murale);
delegate_output!(Murale);
delegate_layer!(Murale);
delegate_registry!(Murale);

impl Dispatch<wp_presentation::WpPresentation, ()> for Murale {
    fn event(
        _state: &mut Self,
        _proxy: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            tracing::debug!("wp_presentation clock_id: {clk_id}");
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, PresentationMeta> for Murale {
    fn event(
        _state: &mut Self,
        _proxy: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &PresentationMeta,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh: _,
                seq_hi,
                seq_lo,
                flags: _,
            } => {
                let scanout_ns =
                    ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64) * 1_000_000_000 + tv_nsec as u64;
                let seq = (seq_hi as u64) << 32 | seq_lo as u64;
                let submit_us = scanout_ns.saturating_sub(data.swap_mono_ns) / 1000;
                tracing::info!(
                    "[scanout] frame={} pos={:.6} scanout_ns={} submit_to_scanout={}us seq={} held={}",
                    data.frame_count, data.media_pos, scanout_ns, submit_us, seq, data.is_held_release
                );
            }
            wp_presentation_feedback::Event::Discarded => {
                tracing::warn!(
                    "[scanout] DISCARDED frame={} pos={:.6} held={}",
                    data.frame_count,
                    data.media_pos,
                    data.is_held_release
                );
            }
            _ => {}
        }
    }
}

impl ProvidesRegistryState for Murale {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    mem::init();
    mem::log_rss("startup");

    let workspace_map: HashMap<String, String> = cli
        .workspace
        .iter()
        .filter_map(|s| {
            let (k, v) = s.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();

    let workspace_config = if workspace_map.is_empty() {
        None
    } else {
        Some((workspace_map, cli.video.clone()))
    };

    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGUSR1,
            handle_refresh_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGUSR2,
            handle_release_signal as *const () as libc::sighandler_t,
        );
    }

    if std::env::var("MURALE_HOLD_COMMIT").is_ok() {
        COMMIT_HELD.store(true, Ordering::Relaxed);
        tracing::info!("hold-commit mode active");
    }

    let conn = Connection::connect_to_env()
        .context("could not connect to Wayland compositor (is a Wayland session running?)")?;
    let (globals, mut event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry init failed: {e}"))?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)
        .context("compositor does not support wl_compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("compositor does not support wlr-layer-shell (required for wallpaper surfaces)")?;
    let output_state = OutputState::new(&globals, &qh);

    let presentation: Option<wp_presentation::WpPresentation> = if cli.scanout {
        globals
            .bind::<wp_presentation::WpPresentation, _, _>(&qh, 1..=1, ())
            .map_err(|e| tracing::debug!("wp_presentation not available: {e}"))
            .ok()
    } else {
        None
    };

    let wl_display_ptr = conn.backend().display_ptr() as *mut c_void;

    let egl_instance = egl::load_egl()?;
    let (egl_display, egl_config, egl_context) = egl::init_display(&egl_instance, wl_display_ptr)?;

    let wakeup_fd = EventFd::from_flags(
        EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_SEMAPHORE,
    )?;

    let mpv_ptr: SharedMpvPtr = Arc::new(Mutex::new(SendPtr(std::ptr::null_mut())));

    let mut state = Murale {
        registry_state: RegistryState::new(&globals),
        output_state,
        compositor_state,
        layer_shell,
        qh: qh.clone(),
        layer: None,
        target_output_name: cli.output.clone(),
        egl_instance,
        egl_state: None,
        egl_display,
        egl_core: Some(EglCore {
            config: egl_config,
            context: egl_context,
        }),
        render_ctx: None,
        mpv: None,
        mpv_ptr: mpv_ptr.clone(),
        wakeup_fd,
        initialized: false,
        first_configure: true,
        width: 0,
        height: 0,
        scale: 1,
        frame_callback_pending: false,
        redraw_needed: false,
        first_swap_done: false,
        ready_signaled: false,
        hold_ready_signaled: false,
        hold_render_gen: 0,
        target_render_armed: false,
        last_mpv_refresh: Instant::now(),
        refresh_pending: false,
        stats_enabled: cli.stats,
        scanout_enabled: cli.scanout,
        scanout_start: None,
        scanout_count: 0,
        frame_stats: FrameStats::new(),
        frame_count: 0,
        loop_count: 0,
        last_time_pos: 0.0,
        last_mem_log: Instant::now(),
        supervised: cli.supervised,
        exit: false,
        video_path: cli.video.clone(),
        mpv_options: cli.mpv_options,
        workspace_config,
        presentation,
        wl_display_ptr,
    };

    event_queue.roundtrip(&mut state)?;

    let target_output = cli
        .output
        .as_deref()
        .and_then(|name| state.find_target_output(name));

    if let Some(name) = &cli.output {
        if target_output.is_none() {
            tracing::warn!("output '{name}' not found, available:");
            for out in state.output_state.outputs() {
                if let Some(info) = state.output_state.info(&out) {
                    tracing::warn!("  {:?}", info.name.as_deref().unwrap_or("(unnamed)"));
                }
            }
        } else {
            tracing::info!("output: {name}");
        }
    }

    state.create_layer_surface(target_output.as_ref());

    let wl_fd = conn.as_fd();
    tracing::debug!("entering main loop");

    let mut workspace_join_handle: Option<std::thread::JoinHandle<()>> = None;

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            tracing::info!("received shutdown signal");
            break;
        }

        // SIGUSR2: release held frame (swap without re-render)
        if RELEASE_SIGNAL.swap(false, Ordering::Relaxed) && COMMIT_HELD.load(Ordering::Relaxed) {
            COMMIT_HELD.store(false, Ordering::Relaxed);
            tracing::info!("hold released");
            state.present_held_frame();
        }

        // SIGUSR1: trigger immediate refresh (coalesced, shutdown wins)
        if REFRESH_REQUESTED.swap(false, Ordering::Relaxed) && state.mpv.is_some() {
            state.refresh_mpv();
        }

        if state.mpv.is_some()
            && !state.supervised
            && state.last_mpv_refresh.elapsed() >= MPV_REFRESH_INTERVAL
        {
            state.refresh_mpv();
        }

        let mut mpv_ready = false;

        {
            let read_guard = event_queue.prepare_read();
            conn.flush()?;

            if let Some(guard) = read_guard {
                let mut pollfds = [
                    PollFd::new(wl_fd, PollFlags::POLLIN),
                    PollFd::new(state.wakeup_fd.as_fd(), PollFlags::POLLIN),
                ];

                match nix::poll::poll(&mut pollfds, PollTimeout::from(16u16)) {
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(e) => return Err(anyhow::anyhow!("poll failed: {e}")),
                }

                if pollfds[0].any().unwrap_or(false) {
                    guard.read()?;
                }

                mpv_ready = pollfds[1].any().unwrap_or(false);
            }
        }

        if !mpv_ready && state.wakeup_fd.read().is_ok() {
            mpv_ready = true;
        }

        event_queue.dispatch_pending(&mut state)?;

        if state.last_mem_log.elapsed().as_secs() >= 30 {
            mem::log_rss("periodic");
            state.last_mem_log = Instant::now();
        }

        if mpv_ready {
            state.handle_mpv_wakeup();
        }

        if workspace_join_handle.is_none() {
            if let (Some(_), Some((ref map, ref default_video))) =
                (&state.mpv, &state.workspace_config)
            {
                workspace_join_handle =
                    spawn_workspace_listener(mpv_ptr.clone(), map.clone(), default_video.clone());
            }
        }

        if state.exit {
            break;
        }
    }

    tracing::info!("shutting down");
    mem::log_rss("shutdown-start");
    mem::log_extra("shutdown-start");
    if let Some(jh) = workspace_join_handle {
        let _ = jh.join();
    }
    // Attach null buffer so compositor sees empty surface before destruction —
    // prevents tear/flash when B is rendered on top during overlap handoff.
    if let Some(ref layer) = state.layer {
        layer.wl_surface().attach(None, 0, 0);
        layer.wl_surface().commit();
        let _ = conn.flush();
        std::thread::sleep(Duration::from_millis(20));
    }

    // GL context must be current for mpv's render teardown (glDelete* calls)
    if let Some(ref egl_state) = state.egl_state {
        if let Some(ref core) = state.egl_core {
            let _ = state.egl_instance.make_current(
                state.egl_display,
                Some(egl_state.surface),
                Some(egl_state.surface),
                Some(core.context),
            );
        }
    }
    mem::log_rss("shutdown-before-render_ctx-drop");
    state.render_ctx.take();
    mem::log_rss("shutdown-after-render_ctx-drop");
    if let Some(mpv) = state.mpv.take() {
        mpv.terminate_destroy();
    }
    mem::log_rss("shutdown-after-mpv-drop");
    if let Some(egl_state) = state.egl_state.take() {
        let _ = egl_state.destroy(&state.egl_instance, state.egl_display);
    }
    if let Some(core) = state.egl_core.take() {
        let _ = state
            .egl_instance
            .destroy_context(state.egl_display, core.context);
    }
    let _ = state.egl_instance.terminate(state.egl_display);

    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "murale=info".parse().unwrap()),
        )
        .init();

    if let Err(e) = run() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}
