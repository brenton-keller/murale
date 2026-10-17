#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod ctrl;
mod egl;
mod error;
mod mem;
mod mpv;
mod stats;
mod supervisor;
mod workspace;

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::AsFd;
use std::os::unix::net::UnixDatagram;
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

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

extern "C" fn handle_refresh_signal(_sig: libc::c_int) {
    REFRESH_REQUESTED.store(true, Ordering::Relaxed);
}

const MPV_REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 3600);

// --- scale mismatch detection ---

fn check_scale_mismatch(
    logical_w: u32,
    logical_h: u32,
    scale: u32,
    mode_w: u32,
    mode_h: u32,
) -> bool {
    let buf_w = logical_w * scale;
    let buf_h = logical_h * scale;
    buf_w != mode_w || buf_h != mode_h
}

#[derive(Debug)]
struct ScaleMismatchState {
    pending: bool,
    warned: bool,
}

impl ScaleMismatchState {
    fn new() -> Self {
        Self {
            pending: false,
            warned: false,
        }
    }

    fn tick(&mut self, mismatched: bool) -> ScaleMismatchAction {
        if mismatched {
            if self.pending {
                if !self.warned {
                    self.warned = true;
                    return ScaleMismatchAction::Warn;
                }
                ScaleMismatchAction::None
            } else {
                self.pending = true;
                ScaleMismatchAction::None
            }
        } else {
            let was_warned = self.warned;
            self.pending = false;
            self.warned = false;
            if was_warned {
                ScaleMismatchAction::Cleared
            } else {
                ScaleMismatchAction::None
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum ScaleMismatchAction {
    None,
    Warn,
    Cleared,
}

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

fn normalize_video_path(path: &str) -> String {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        }
    } else {
        path.to_string()
    };
    std::path::absolute(&expanded)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(expanded)
}

fn scaled_buffer_size(logical_w: u32, logical_h: u32, scale: u32) -> (i32, i32) {
    ((logical_w * scale) as i32, (logical_h * scale) as i32)
}

#[cfg(test)]
fn should_defer_surface(target_output: &Option<String>, found: bool) -> bool {
    target_output.is_some() && !found
}

fn desired_load(
    active_ws: &str,
    map: &HashMap<String, String>,
    default: &str,
    playing: &str,
) -> Option<String> {
    let desired = map.get(active_ws).map(|v| v.as_str()).unwrap_or(default);
    if desired != playing {
        Some(desired.to_string())
    } else {
        None
    }
}

#[cfg(test)]
fn desired_load_connected(
    active_ws: &str,
    map: &HashMap<String, String>,
    default: &str,
    playing: &str,
    connected: bool,
) -> Option<String> {
    if !connected {
        return None;
    }
    desired_load(active_ws, map, default, playing)
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
pub struct Cli {
    /// Path to video file (any format mpv supports). Used as the default/fallback
    /// when no --workspace mapping matches.
    pub video: String,

    /// Target a specific display output by name (e.g. DP-1, HDMI-A-1).
    /// Use --list-outputs to see available names.
    #[arg(short, long)]
    pub output: Option<String>,

    /// Map a Hyprland workspace to a video. Can be repeated.
    /// When switching to a mapped workspace, the corresponding video loads.
    /// Unmapped workspaces use the default video.
    #[arg(short, long, value_name = "N=PATH")]
    pub workspace: Vec<String>,

    /// Pass options to mpv as comma-separated key=value pairs. Bare flags are
    /// set to "yes" (loop is special-cased to loop-file=inf).
    /// Example: --mpv-options "input-ipc-server=/tmp/murale.sock,hwdec=auto"
    #[arg(long, value_name = "OPTIONS")]
    pub mpv_options: Option<String>,

    /// Log frame timing and mpv playback stats every 5 seconds
    #[arg(long)]
    pub stats: bool,

    /// Log wp_presentation scanout timestamps (per-frame, capped at 60s)
    #[arg(long)]
    pub scanout: bool,

    /// List available display outputs and exit
    #[arg(long)]
    pub list_outputs: bool,

    /// Run without automatic memory recycling (single process, may leak on NVIDIA)
    #[arg(long)]
    pub no_recycle: bool,

    /// Internal: control channel fd (used by supervisor)
    #[arg(long, hide = true)]
    pub control_fd: Option<i32>,

    /// Internal: initial video path for handoff (overrides positional for first load only)
    #[arg(long, hide = true)]
    pub initial_path: Option<String>,
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

    // Hidden sync: hold mode (from Hello handshake in renderer mode)
    hold_mode: bool,
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
    scale_mismatch: ScaleMismatchState,

    // Loop restart detection via time-pos backward jump
    loop_count: u64,
    last_time_pos: f64,

    // Control channel (renderer mode only, None in standalone)
    ctrl: Option<ctrl::ChildCtrlChannel>,
    #[allow(dead_code)]
    generation: u64,
    #[allow(dead_code)]
    gen_token: String,

    // Release mechanism (VSync-aligned handoff)
    release_recv: Option<UnixDatagram>,
    release_send: Option<UnixDatagram>,
    release_seq: u64,
    release_token: [u8; 16],
    release_armed: bool,

    exit: bool,
    handoff_pending: bool,
    initial_path: Option<String>,
    video_path: String,
    mpv_options: Option<String>,

    // Delayed workspace re-query after monitor reconnect (Fix 2: DPMS phantom events)
    reconnect_requery: Option<Instant>,
    workspace_available: bool,

    // Workspace config — kept for intent replay during refresh
    workspace_config: Option<(HashMap<String, String>, String)>,

    // Hold-mode workspace deferral (shared with workspace listener thread)
    output_connected: Arc<AtomicBool>,
    hold_active: Arc<AtomicBool>,
    deferred_video: Arc<Mutex<Option<String>>>,
    ws_mismatch_secs: Arc<std::sync::atomic::AtomicI64>,

    // Scanout instrumentation (wp_presentation)
    presentation: Option<wp_presentation::WpPresentation>,

    // Raw wl_display pointer for mpv
    wl_display_ptr: *mut c_void,
}

impl Murale {
    fn output_mode_str(&self) -> String {
        let out_name = self.target_output_name.as_deref().unwrap_or("(any)");
        if let Some(name) = &self.target_output_name {
            if let Some(output) = self.find_target_output(name) {
                if let Some(info) = self.output_state.info(&output) {
                    if let Some(mode) = info.modes.iter().find(|m| m.current) {
                        return format!(
                            "output={out_name} (mode={}x{})",
                            mode.dimensions.0, mode.dimensions.1
                        );
                    }
                }
            }
        }
        format!("output={out_name}")
    }

    fn log_surface_state(&self, event: &str) {
        let buf_w = self.width * self.scale;
        let buf_h = self.height * self.scale;
        let mode = self.output_mode_str();
        tracing::info!(
            "surface: {event} logical={}x{} scale={} buffer={buf_w}x{buf_h} {mode}",
            self.width,
            self.height,
            self.scale,
        );
    }

    fn create_layer_surface(&mut self, output: Option<&wl_output::WlOutput>) {
        debug_assert!(
            self.layer.is_none(),
            "create_layer_surface called with existing surface"
        );
        if self.layer.is_some() {
            tracing::warn!("BUG: create_layer_surface called with existing surface — skipping");
            return;
        }
        self.scale = 1;
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
        layer.wl_surface().set_buffer_scale(1);
        layer.commit();
        self.layer = Some(layer);
        self.first_configure = true;
        self.frame_callback_pending = false;
        self.redraw_needed = false;
        tracing::info!("surface: created {}", self.output_mode_str());
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

        let (w, h) = scaled_buffer_size(self.width, self.height, self.scale);

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

        if self.hold_mode {
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
            if self.hold_mode {
                if !self.target_render_armed && self.hold_render_gen == 0 {
                    self.target_render_armed = true;
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
        if let Some(ref mut ctrl) = self.ctrl {
            match ctrl.send_event("hold_ready") {
                Ok(()) => {
                    tracing::info!("hold readiness signaled (ctrl)");
                    self.hold_ready_signaled = true;
                }
                Err(e) => tracing::error!("failed to send hold_ready: {e}"),
            }
        } else {
            self.hold_ready_signaled = true;
        }
    }

    fn write_target_rendered(&mut self) {
        let pos = self
            .mpv
            .as_ref()
            .and_then(|m| m.get_property::<f64>("time-pos").ok())
            .unwrap_or(-1.0);
        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_target_rendered(self.hold_render_gen, pos);
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

        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_swap_done(self.release_seq);
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

    // --- Control channel command handling ---

    fn drain_ctrl_commands(&mut self) {
        let ctrl = match &mut self.ctrl {
            Some(c) => c,
            None => return,
        };
        let mut cmds = Vec::new();
        for _ in 0..4 {
            match ctrl.try_recv_cmd() {
                Ok(Some(cmd)) => cmds.push(cmd),
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("ctrl recv error: {e}");
                    self.exit = true;
                    return;
                }
            }
        }
        // Prioritize shutdown
        if cmds.iter().any(|c| c.is_shutdown()) {
            cmds.retain(|c| c.is_shutdown());
        }
        for cmd in cmds {
            self.handle_ctrl_cmd(cmd);
        }
    }

    fn handle_ctrl_cmd(&mut self, cmd: ctrl::SupervisorCmd) {
        use ctrl::SupervisorCmd;
        match cmd {
            SupervisorCmd::Hello(_) => {
                tracing::warn!("unexpected Hello after handshake");
            }
            SupervisorCmd::GetSnapshot { id } => self.handle_get_snapshot(id),
            SupervisorCmd::Seek { id, pos } => self.handle_seek(id, pos),
            SupervisorCmd::ArmRender {
                id,
                target,
                seq,
                release_token,
            } => self.handle_arm_render(id, target, seq, release_token),
            SupervisorCmd::Release {
                id,
                seq,
                release_token,
            } => self.handle_release_cmd(id, seq, release_token),
            SupervisorCmd::ArmRelease {
                id,
                seq,
                abstract_addr,
                release_token,
            } => self.handle_arm_release(id, seq, abstract_addr, release_token),
            SupervisorCmd::PrepareRelease {
                id,
                seq,
                release_token,
            } => self.handle_prepare_release(id, seq, release_token),
            SupervisorCmd::Unpause { id } => self.handle_unpause(id),
            SupervisorCmd::HandoffComplete { id } => self.handle_handoff_complete(id),
            SupervisorCmd::Shutdown { id } => self.handle_shutdown(id),
        }
    }

    fn handle_get_snapshot(&mut self, id: u64) {
        let snap = self.build_snapshot();
        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_snapshot(id, &snap);
        }
    }

    fn build_snapshot(&self) -> ctrl::Snapshot {
        let mpv = self.mpv.as_ref();
        ctrl::Snapshot {
            path: mpv
                .and_then(|m| m.get_property("path").ok())
                .unwrap_or_else(|| self.video_path.clone()),
            pos: mpv
                .and_then(|m| m.get_property("time-pos").ok())
                .unwrap_or(0.0),
            dur: mpv
                .and_then(|m| m.get_property("duration").ok())
                .unwrap_or(0.0),
            fps: mpv
                .and_then(|m| m.get_property("container-fps").ok())
                .unwrap_or(0.0),
            codec: mpv
                .and_then(|m| m.get_property("video-codec").ok())
                .unwrap_or_default(),
            hwdec: mpv
                .and_then(|m| m.get_property("hwdec-current").ok())
                .unwrap_or_default(),
            width: mpv
                .and_then(|m| m.get_property::<i64>("video-out-params/w").ok())
                .unwrap_or(0) as u32,
            height: mpv
                .and_then(|m| m.get_property::<i64>("video-out-params/h").ok())
                .unwrap_or(0) as u32,
            output_connected: self.output_connected.load(Ordering::Relaxed),
        }
    }

    fn handle_seek(&mut self, id: u64, pos: f64) {
        if let Some(mpv) = &self.mpv {
            let pos_str = format!("{pos:.6}");
            match mpv.command("seek", &[&pos_str, "absolute"]) {
                Ok(()) => {
                    if let Some(ref mut ctrl) = self.ctrl {
                        let _ = ctrl.send_ok(id);
                    }
                }
                Err(e) => {
                    if let Some(ref mut ctrl) = self.ctrl {
                        let _ = ctrl.send_cmd_error(id, &format!("seek failed: {e}"));
                    }
                }
            }
        }
    }

    fn handle_arm_render(
        &mut self,
        id: u64,
        _target: f64,
        seq: Option<u64>,
        release_token: Option<String>,
    ) {
        if let (Some(seq), Some(ref token)) = (seq, &release_token) {
            self.release_seq = seq;
            if let Some(decoded) = ctrl::hex_decode(token) {
                if decoded.len() == 16 {
                    self.release_token.copy_from_slice(&decoded);
                }
            }
        }
        self.target_render_armed = true;
        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_ok(id);
        }
    }

    fn handle_release_cmd(&mut self, _id: u64, seq: u64, token: String) {
        self.release_seq = seq;
        if let Some(decoded) = ctrl::hex_decode(&token) {
            if decoded.len() == 16 {
                self.release_token.copy_from_slice(&decoded);
            }
        }
        self.hold_mode = false;
        tracing::info!("release command received (fallback, seq={seq})");
        self.present_held_frame();
        self.apply_deferred_workspace();
    }

    fn handle_arm_release(&mut self, id: u64, seq: u64, addr: String, token: String) {
        match ctrl::connect_abstract_dgram(&addr) {
            Ok(sock) => {
                self.release_send = Some(sock);
                self.release_seq = seq;
                if let Some(decoded) = ctrl::hex_decode(&token) {
                    if decoded.len() == 16 {
                        self.release_token.copy_from_slice(&decoded);
                    }
                }
                self.release_armed = true;
                if let Some(ref mut ctrl) = self.ctrl {
                    let _ = ctrl.send_release_armed(seq);
                }
            }
            Err(e) => {
                tracing::error!("arm_release: connect failed: {e}");
                if let Some(ref mut ctrl) = self.ctrl {
                    let _ = ctrl.send_cmd_error(id, &format!("connect failed: {e}"));
                }
            }
        }
    }

    fn handle_prepare_release(&mut self, id: u64, seq: u64, token: String) {
        self.release_seq = seq;
        if let Some(decoded) = ctrl::hex_decode(&token) {
            if decoded.len() == 16 {
                self.release_token.copy_from_slice(&decoded);
            }
        }
        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_ok(id);
        }
    }

    fn handle_unpause(&mut self, id: u64) {
        if let Some(mpv) = &self.mpv {
            let _ = mpv.set_property("pause", false);
        }
        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_ok(id);
        }
    }

    fn handle_handoff_complete(&mut self, id: u64) {
        tracing::info!("handoff complete — workspace listener unlocked");
        self.handoff_pending = false;
        if self.workspace_available && self.output_connected.load(Ordering::Relaxed) {
            self.sync_workspace_video("unlock");
        }
        if let Some(ref mut ctrl) = self.ctrl {
            let _ = ctrl.send_ok(id);
        }
    }

    fn handle_shutdown(&mut self, _id: u64) {
        tracing::info!("shutdown command received from supervisor");
        self.exit = true;
    }

    fn handle_release_datagram(&mut self) {
        let recv = match &self.release_recv {
            Some(r) => r,
            None => return,
        };
        let mut buf = [0u8; ctrl::RELEASE_DATAGRAM_SIZE];
        match recv.recv(&mut buf) {
            Ok(n) if n == ctrl::RELEASE_DATAGRAM_SIZE => {
                let (seq, token) = ctrl::decode_release_datagram(&buf);
                if seq == self.release_seq && token == self.release_token {
                    self.hold_mode = false;
                    tracing::info!("release datagram received (seq={seq})");
                    self.present_held_frame();
                    self.apply_deferred_workspace();
                } else {
                    tracing::warn!(
                        "release datagram mismatch: seq={seq} expected={}",
                        self.release_seq
                    );
                }
            }
            Ok(n) => tracing::warn!("release datagram wrong size: {n}"),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => tracing::error!("release datagram recv error: {e}"),
        }
    }

    fn apply_deferred_workspace(&mut self) {
        self.hold_active.store(false, Ordering::Relaxed);
        let video = {
            let mut def = self.deferred_video.lock().unwrap_or_else(|e| e.into_inner());
            def.take()
        };
        if let Some(video) = video {
            if let Some(mpv) = &self.mpv {
                tracing::info!("applying deferred workspace video: {video}");
                let _ = mpv.command("loadfile", &[&video]);
            }
        }
    }

    fn sync_workspace_video(&self, context: &str) {
        let (map, default_video) = match &self.workspace_config {
            Some(cfg) => cfg,
            None => return,
        };
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };
        let ws = match workspace::get_active_workspace() {
            Some(ws) => ws,
            None => return,
        };
        let current_path: String = mpv.get_property("path").unwrap_or_default();
        if let Some(desired) = desired_load(&ws, map, default_video, &current_path) {
            tracing::info!("{context}: active workspace = {ws} (was playing {current_path})");
            let _ = mpv.command("loadfile", &[&desired]);
        } else {
            tracing::info!("{context}: active workspace = {ws} (already correct)");
        }
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
        self.log_surface_state("scale-changed");
        if let Some(ref layer) = self.layer {
            layer.wl_surface().set_buffer_scale(new_factor);
        }
        if let Some(egl_state) = &self.egl_state {
            let (scaled_w, scaled_h) = scaled_buffer_size(self.width, self.height, self.scale);
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

        // VSync-aligned release: A sends release datagram to B from its frame callback.
        if self.release_armed {
            if let Some(ref sock) = self.release_send {
                let dgram = ctrl::encode_release_datagram(self.release_seq, &self.release_token);
                match sock.send(&dgram) {
                    Ok(_) => {
                        self.release_armed = false;
                        if let Some(ref mut ctrl) = self.ctrl {
                            let _ = ctrl.send_release_sent(self.release_seq);
                        }
                    }
                    Err(e) => {
                        tracing::error!("release datagram send failed: {e}");
                        self.release_armed = false;
                    }
                }
            }
        }

        // Signal display readiness to supervisor after first compositor-acknowledged frame.
        if self.first_swap_done && !self.ready_signaled {
            if let Some(ref mut ctrl) = self.ctrl {
                match ctrl.send_event("display_ready") {
                    Ok(()) => {
                        tracing::info!("display ready signaled (ctrl)");
                        self.ready_signaled = true;
                    }
                    Err(e) => tracing::error!("failed to send display_ready: {e}"),
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

        if name.as_deref() == Some(&target) && self.layer.is_none() {
            if self.initialized {
                tracing::info!("output {target} reconnected, recreating surface");
                self.output_connected.store(true, Ordering::Relaxed);
                self.create_layer_surface(Some(&output));
                if self.workspace_available {
                    self.sync_workspace_video("reconnect");
                    self.reconnect_requery = Some(Instant::now() + Duration::from_secs(1));
                }
                if let Some(ref mut ctrl) = self.ctrl {
                    let _ = ctrl.send_event("output_reconnected");
                }
            } else {
                tracing::info!("output {target} appeared, creating deferred surface");
                self.create_layer_surface(Some(&output));
            }
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
            self.output_connected.store(false, Ordering::Relaxed);
            self.teardown_surface();
            if let Some(ref mut ctrl) = self.ctrl {
                let _ = ctrl.send_event("output_disconnected");
            }
        }
    }
}

impl LayerShellHandler for Murale {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        tracing::warn!("layer surface closed by compositor");
        self.output_connected.store(false, Ordering::Relaxed);
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
        self.log_surface_state("configure");

        if self.first_configure {
            self.first_configure = false;

            let (scaled_w, scaled_h) = scaled_buffer_size(w, h, self.scale);

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
                let load = self.initial_path.take();
                if let Err(e) = self.init_mpv(load.as_deref()) {
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
            let (scaled_w, scaled_h) = scaled_buffer_size(w, h, self.scale);
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

// --- Hello handshake (blocking, before Wayland init) ---

fn wait_for_hello(ctrl: &mut ctrl::ChildCtrlChannel) -> Result<ctrl::Hello> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ctrl.try_recv_cmd() {
            Ok(Some(ctrl::SupervisorCmd::Hello(hello))) => return Ok(hello),
            Ok(Some(other)) => anyhow::bail!("expected Hello, got: {other:?}"),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    anyhow::bail!("Hello handshake timeout (5s)");
                }
                let mut fds = [PollFd::new(ctrl.as_fd(), PollFlags::POLLIN)];
                let ms = remaining.as_millis().min(5000) as u16;
                match nix::poll::poll(&mut fds, PollTimeout::from(ms)) {
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(e) => anyhow::bail!("poll failed during Hello: {e}"),
                }
            }
            Err(e) => anyhow::bail!("ctrl recv during Hello: {e}"),
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    mem::init();
    mem::log_rss("startup");

    // --- Control channel setup (renderer mode) ---
    let mut ctrl_channel: Option<ctrl::ChildCtrlChannel> = None;
    let mut hold_mode = false;
    let mut generation: u64 = 0;
    let mut gen_token = String::new();
    let mut release_recv: Option<UnixDatagram> = None;

    if let Some(fd_num) = cli.control_fd {
        let fd = ctrl::validate_socket_fd(fd_num)
            .context(format!("--control-fd={fd_num} is not a valid socket"))?;
        let mut ctrl = ctrl::ChildCtrlChannel::from_fd(fd)?;

        let hello = wait_for_hello(&mut ctrl)?;
        anyhow::ensure!(
            hello.version == ctrl::PROTOCOL_VERSION,
            "protocol version mismatch: expected {}, got {}",
            ctrl::PROTOCOL_VERSION,
            hello.version
        );

        hold_mode = hello.mode == ctrl::SpawnMode::Hold;
        generation = hello.gen;
        gen_token = hello.token;

        ctrl.send_hello_ack()?;

        if hold_mode {
            let release_fd = ctrl::validate_socket_fd(ctrl::RELEASE_FD)
                .context("hold mode requires release fd (fd 4)")?;
            let dgram: UnixDatagram = release_fd.into();
            dgram.set_nonblocking(true)?;
            release_recv = Some(dgram);
        }

        tracing::info!(
            "renderer mode: gen={generation} hold={hold_mode} token={}...",
            &gen_token[..gen_token.len().min(8)]
        );
        ctrl_channel = Some(ctrl);
    }

    let is_supervised = ctrl_channel.is_some();

    let video = normalize_video_path(&cli.video);
    let initial_path = cli.initial_path.as_deref().map(normalize_video_path);

    let workspace_map: HashMap<String, String> = cli
        .workspace
        .iter()
        .filter_map(|s| {
            let (k, v) = s.split_once('=')?;
            Some((k.trim().to_string(), normalize_video_path(v.trim())))
        })
        .collect();

    let workspace_config = if workspace_map.is_empty() {
        None
    } else {
        Some((workspace_map, video.clone()))
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
        hold_mode,
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
        scale_mismatch: ScaleMismatchState::new(),
        ctrl: ctrl_channel,
        generation,
        gen_token,
        release_recv,
        release_send: None,
        release_seq: 0,
        release_token: [0u8; 16],
        release_armed: false,
        exit: false,
        handoff_pending: hold_mode,
        initial_path,
        video_path: video.clone(),
        mpv_options: cli.mpv_options,
        workspace_config,
        reconnect_requery: None,
        workspace_available: false,
        output_connected: Arc::new(AtomicBool::new(true)),
        hold_active: Arc::new(AtomicBool::new(hold_mode)),
        deferred_video: Arc::new(Mutex::new(None)),
        ws_mismatch_secs: Arc::new(std::sync::atomic::AtomicI64::new(-1)),
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
            let available: Vec<String> = state
                .output_state
                .outputs()
                .filter_map(|out| {
                    state
                        .output_state
                        .info(&out)
                        .and_then(|i| i.name.clone())
                })
                .collect();
            let n_outputs = state.output_state.outputs().count();
            let since_connect = mem::elapsed_secs();
            if available.is_empty() {
                // Inferred: output info may not have arrived after roundtrips
                // in a freshly-spawned process (Wayland protocol race, unverified).
                tracing::info!(
                    "output '{name}' not found ({n_outputs} outputs known, 0 with info, {since_connect:.1}s after connect) — deferring surface until it appears"
                );
            } else {
                tracing::info!(
                    "output '{name}' not found, available: {} ({since_connect:.1}s after connect) — deferring surface until it appears",
                    available.join(", ")
                );
            }
            if is_supervised {
                if let Some(ref mut ctrl) = state.ctrl {
                    let _ = ctrl.send_waiting_for_output(&available);
                }
            }
        } else {
            tracing::info!("output: {name}");
        }
    }

    if state.layer.is_none() && (target_output.is_some() || cli.output.is_none()) {
        state.create_layer_surface(target_output.as_ref());
    }

    let wl_fd = conn.as_fd();
    tracing::debug!("entering main loop");

    let mut workspace_join_handle: Option<std::thread::JoinHandle<()>> = None;
    state.workspace_available = if state.workspace_config.is_some() {
        match std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
            Ok(_) => true,
            Err(_) => {
                tracing::warn!(
                    "workspace switching disabled: HYPRLAND_INSTANCE_SIGNATURE not set"
                );
                false
            }
        }
    } else {
        false
    };

    let mut last_output_warn = Instant::now() - Duration::from_secs(600);

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            tracing::info!("received shutdown signal");
            break;
        }

        // SIGUSR1: trigger immediate refresh (standalone mode only)
        if REFRESH_REQUESTED.swap(false, Ordering::Relaxed) && !is_supervised && state.mpv.is_some()
        {
            state.refresh_mpv();
        }

        // Periodic internal refresh (standalone mode only)
        if !is_supervised
            && state.mpv.is_some()
            && state.last_mpv_refresh.elapsed() >= MPV_REFRESH_INTERVAL
        {
            state.refresh_mpv();
        }

        let mut mpv_ready = false;
        let mut ctrl_readable = false;
        let mut release_readable = false;

        {
            let read_guard = event_queue.prepare_read();
            conn.flush()?;

            if let Some(guard) = read_guard {
                let ctrl_fd_ref = state.ctrl.as_ref().map(|c| c.as_fd());
                let release_fd_ref = state.release_recv.as_ref().map(|r| r.as_fd());

                let mut pollfds = vec![
                    PollFd::new(wl_fd, PollFlags::POLLIN),
                    PollFd::new(state.wakeup_fd.as_fd(), PollFlags::POLLIN),
                ];
                let ctrl_idx = ctrl_fd_ref.map(|fd| {
                    let i = pollfds.len();
                    pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
                    i
                });
                let release_idx = release_fd_ref.map(|fd| {
                    let i = pollfds.len();
                    pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
                    i
                });

                match nix::poll::poll(&mut pollfds, PollTimeout::from(16u16)) {
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(e) => return Err(anyhow::anyhow!("poll failed: {e}")),
                }

                if pollfds[0].any().unwrap_or(false) {
                    guard.read()?;
                }

                mpv_ready = pollfds[1].any().unwrap_or(false);

                if let Some(idx) = ctrl_idx {
                    let revents = pollfds[idx].revents().unwrap_or(PollFlags::empty());
                    if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
                        tracing::info!("ctrl channel closed (parent died), exiting");
                        break;
                    }
                    ctrl_readable = revents.contains(PollFlags::POLLIN);
                }

                if let Some(idx) = release_idx {
                    release_readable = pollfds[idx].any().unwrap_or(false);
                }
            }
        }

        if !mpv_ready && state.wakeup_fd.read().is_ok() {
            mpv_ready = true;
        }

        event_queue.dispatch_pending(&mut state)?;

        // Handle release datagram first (highest priority — VSync-aligned)
        if release_readable {
            state.handle_release_datagram();
        }

        // Drain ctrl commands (bounded at 4, shutdown prioritized)
        if ctrl_readable {
            state.drain_ctrl_commands();
        }

        if state.last_mem_log.elapsed().as_secs() >= 30 {
            mem::log_rss("periodic");
            state.last_mem_log = Instant::now();

            if is_supervised && state.layer.is_none() && state.target_output_name.is_some() {
                if let Some(ref mut ctrl) = state.ctrl {
                    let available: Vec<String> = state
                        .output_state
                        .outputs()
                        .filter_map(|out| {
                            state.output_state.info(&out).and_then(|i| i.name.clone())
                        })
                        .collect();
                    let _ = ctrl.send_waiting_for_output(&available);
                    if last_output_warn.elapsed() >= Duration::from_secs(600) {
                        tracing::warn!(
                            "still waiting for target output (available: [{}])",
                            available.join(", ")
                        );
                        last_output_warn = Instant::now();
                    }
                }
            }

            if workspace_join_handle
                .as_ref()
                .is_some_and(|h| h.is_finished())
            {
                tracing::warn!("[ws] listener exited — respawning");
                workspace_join_handle = None;
            }

            // Scale mismatch detector (two-tick debounce)
            if state.layer.is_some() && state.width > 0 && state.scale > 0 {
                if let Some(name) = &state.target_output_name {
                    if let Some(output) = state.find_target_output(name) {
                        if let Some(info) = state.output_state.info(&output) {
                            if let Some(mode) = info.modes.iter().find(|m| m.current) {
                                let (mode_w, mode_h) =
                                    (mode.dimensions.0 as u32, mode.dimensions.1 as u32);
                                let mismatched = check_scale_mismatch(
                                    state.width, state.height, state.scale, mode_w, mode_h,
                                );
                                match state.scale_mismatch.tick(mismatched) {
                                    ScaleMismatchAction::Warn => {
                                        let buf_w = state.width * state.scale;
                                        let buf_h = state.height * state.scale;
                                        tracing::warn!(
                                            "scale mismatch: buffer={buf_w}x{buf_h} (logical={}x{} scale={}) != mode={mode_w}x{mode_h}",
                                            state.width, state.height, state.scale,
                                        );
                                    }
                                    ScaleMismatchAction::Cleared => {
                                        let buf_w = state.width * state.scale;
                                        let buf_h = state.height * state.scale;
                                        tracing::info!("scale mismatch cleared: buffer={buf_w}x{buf_h} matches mode");
                                    }
                                    ScaleMismatchAction::None => {}
                                }
                            }
                        }
                    }
                }
            }
        }

        if mpv_ready {
            state.handle_mpv_wakeup();
        }

        if let Some(deadline) = state.reconnect_requery {
            if Instant::now() >= deadline {
                state.reconnect_requery = None;
                state.sync_workspace_video("reconnect");
            }
        }

        if state.workspace_available && !state.hold_mode && !state.handoff_pending && workspace_join_handle.is_none() {
            if let (Some(_), Some((ref map, ref default_video))) =
                (&state.mpv, &state.workspace_config)
            {
                workspace_join_handle = spawn_workspace_listener(
                    mpv_ptr.clone(),
                    map.clone(),
                    default_video.clone(),
                    state.hold_active.clone(),
                    state.deferred_video.clone(),
                    state.output_connected.clone(),
                    state.target_output_name.clone(),
                    state.ws_mismatch_secs.clone(),
                );
                if !state.handoff_pending && state.output_connected.load(Ordering::Relaxed) {
                    state.sync_workspace_video("startup");
                }
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

struct OutputQuery {
    registry_state: RegistryState,
    output_state: OutputState,
}

impl OutputHandler for OutputQuery {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl ProvidesRegistryState for OutputQuery {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_output!(OutputQuery);
delegate_registry!(OutputQuery);

fn list_outputs() -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("could not connect to Wayland compositor (is a Wayland session running?)")?;
    let (globals, mut event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry init failed: {e}"))?;
    let qh = event_queue.handle();
    let output_state = OutputState::new(&globals, &qh);
    let mut state = OutputQuery {
        registry_state: RegistryState::new(&globals),
        output_state,
    };
    event_queue.roundtrip(&mut state)?;
    for output in state.output_state.outputs() {
        if let Some(info) = state.output_state.info(&output) {
            if let Some(name) = &info.name {
                println!("{name}");
            }
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|s| s == "--list-outputs") {
        if let Err(e) = list_outputs() {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let is_renderer = args
        .get(1)
        .map(|s| s == "__internal-renderer")
        .unwrap_or(false);
    let is_supervisor = !is_renderer && !args.iter().any(|s| s == "--no-recycle");

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "murale=info".parse().unwrap());

    if is_supervisor {
        let cli = Cli::parse();

        if let Some(ref name) = cli.output {
            if name.is_empty() || name.len() > 100 {
                eprintln!("error: invalid --output value: must be 1-100 bytes");
                std::process::exit(1);
            }
        }

        let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let ns = supervisor::output_namespace(cli.output.as_deref());

        let state_dir = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
            format!(
                "{}/.local/state",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
            )
        });
        let persistent_log_dir =
            std::path::PathBuf::from(&state_dir).join("murale").join(&ns);
        let _ = std::fs::create_dir_all(&persistent_log_dir);
        let _ = std::fs::create_dir_all(persistent_log_dir.join("failed"));

        let tmpfs_log_dir =
            std::path::PathBuf::from(&xdg).join("murale").join(&ns).join("logs");
        let _ = std::fs::create_dir_all(&tmpfs_log_dir);

        let sup_log_path = persistent_log_dir.join("supervisor.log");
        supervisor::rotate_supervisor_log_on_open(&sup_log_path);

        let (sup_log_dir, _fallback) = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sup_log_path)
        {
            Ok(_) => (persistent_log_dir.clone(), false),
            Err(_) => {
                eprintln!(
                    "warning: persistent log path failed, using tmpfs: {}",
                    tmpfs_log_dir.display()
                );
                (tmpfs_log_dir.clone(), true)
            }
        };

        let sup_log_file_path = sup_log_dir.join("supervisor.log");
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sup_log_file_path)
        {
            Ok(file) => {
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_ansi(false)
                    .init();
            }
            Err(_) => {
                tracing_subscriber::fmt().with_env_filter(filter).init();
            }
        }

        let result = if cli.no_recycle {
            run(cli)
        } else {
            supervisor::run_supervisor(cli)
        };
        if let Err(e) = result {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(!is_renderer)
        .init();

    let result = if is_renderer {
        unsafe {
            let name = std::ffi::CString::new("murale-render").unwrap();
            libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0usize, 0usize, 0usize);
        }
        let filtered: Vec<&str> = std::iter::once(args[0].as_str())
            .chain(args[2..].iter().map(|s| s.as_str()))
            .collect();
        let cli = Cli::parse_from(filtered);
        if cli.control_fd.is_none() {
            tracing::error!("__internal-renderer requires --control-fd");
            std::process::exit(1);
        }
        run(cli)
    } else {
        let cli = Cli::parse();
        run(cli)
    };

    if let Err(e) = result {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn default_to_mapped() {
        let map = make_map(&[("2", "/videos/ocean.mp4")]);
        let result = desired_load("2", &map, "/videos/default.mp4", "/videos/default.mp4");
        assert_eq!(result, Some("/videos/ocean.mp4".into()));
    }

    #[test]
    fn mapped_to_default() {
        let map = make_map(&[("2", "/videos/ocean.mp4")]);
        let result = desired_load("1", &map, "/videos/default.mp4", "/videos/ocean.mp4");
        assert_eq!(result, Some("/videos/default.mp4".into()));
    }

    #[test]
    fn unmapped_falls_back_to_default() {
        let map = make_map(&[("2", "/videos/ocean.mp4")]);
        let result = desired_load("99", &map, "/videos/default.mp4", "/videos/ocean.mp4");
        assert_eq!(result, Some("/videos/default.mp4".into()));
    }

    #[test]
    fn same_video_returns_none() {
        let map = make_map(&[("1", "/videos/forest.mp4")]);
        let result = desired_load("1", &map, "/videos/default.mp4", "/videos/forest.mp4");
        assert_eq!(result, None);
    }

    #[test]
    fn unnormalized_path_does_not_match() {
        let map = make_map(&[("1", "/home/user/Videos/a.mp4")]);
        let result = desired_load("1", &map, "/videos/default.mp4", "~/Videos/a.mp4");
        assert_eq!(result, Some("/home/user/Videos/a.mp4".into()));
    }

    #[test]
    fn normalize_expands_tilde() {
        let result = normalize_video_path("~/Videos/a.mp4");
        assert!(!result.starts_with("~/"), "tilde should be expanded: {result}");
        assert!(result.ends_with("/Videos/a.mp4"));
    }

    #[test]
    fn normalize_makes_absolute() {
        let result = normalize_video_path("relative/path.mp4");
        assert!(
            result.starts_with('/'),
            "should be absolute: {result}"
        );
    }

    #[test]
    fn normalize_preserves_absolute() {
        let result = normalize_video_path("/absolute/path.mp4");
        assert_eq!(result, "/absolute/path.mp4");
    }

    #[test]
    fn disconnected_skips_load() {
        let map = make_map(&[("2", "/videos/ocean.mp4")]);
        let result =
            desired_load_connected("2", &map, "/videos/default.mp4", "/videos/default.mp4", false);
        assert_eq!(result, None);
    }

    #[test]
    fn connected_allows_load() {
        let map = make_map(&[("2", "/videos/ocean.mp4")]);
        let result =
            desired_load_connected("2", &map, "/videos/default.mp4", "/videos/default.mp4", true);
        assert_eq!(result, Some("/videos/ocean.mp4".into()));
    }

    #[test]
    fn buffer_size_scale_1() {
        assert_eq!(scaled_buffer_size(7680, 2160, 1), (7680, 2160));
    }

    #[test]
    fn buffer_size_scale_2() {
        assert_eq!(scaled_buffer_size(7680, 2160, 2), (15360, 4320));
    }

    #[test]
    fn buffer_size_zero() {
        assert_eq!(scaled_buffer_size(0, 0, 2), (0, 0));
    }

    #[test]
    fn defer_when_target_absent() {
        assert!(should_defer_surface(&Some("DP-4".into()), false));
    }

    #[test]
    fn no_defer_when_target_found() {
        assert!(!should_defer_surface(&Some("DP-4".into()), true));
    }

    #[test]
    fn no_defer_when_no_target() {
        assert!(!should_defer_surface(&None, false));
    }

    #[test]
    fn unlock_while_disconnected_skips_load() {
        let map = make_map(&[("5", "/videos/a010.mp4")]);
        let result =
            desired_load_connected("5", &map, "/videos/default.mp4", "/videos/default.mp4", false);
        assert_eq!(result, None);
    }

    #[test]
    fn reconnect_loads_active_workspace() {
        let map = make_map(&[("1", "/videos/a007.mp4"), ("5", "/videos/a010.mp4")]);
        let result =
            desired_load_connected("1", &map, "/videos/default.mp4", "/videos/a010.mp4", true);
        assert_eq!(result, Some("/videos/a007.mp4".into()));
    }

    #[test]
    fn scale_mismatch_scale_1_match() {
        assert!(!check_scale_mismatch(1920, 1080, 1, 1920, 1080));
    }

    #[test]
    fn scale_mismatch_scale_2_match() {
        assert!(!check_scale_mismatch(1920, 1080, 2, 3840, 2160));
    }

    #[test]
    fn scale_mismatch_scale_1_wrong_mode() {
        assert!(check_scale_mismatch(1920, 1080, 1, 3840, 2160));
    }

    #[test]
    fn scale_mismatch_scale_2_wrong_buffer() {
        assert!(check_scale_mismatch(1920, 1080, 2, 1920, 1080));
    }

    #[test]
    fn scale_debounce_requires_two_ticks() {
        let mut s = ScaleMismatchState::new();
        assert_eq!(s.tick(true), ScaleMismatchAction::None);
        assert_eq!(s.tick(true), ScaleMismatchAction::Warn);
    }

    #[test]
    fn scale_debounce_transient_no_warn() {
        let mut s = ScaleMismatchState::new();
        assert_eq!(s.tick(true), ScaleMismatchAction::None);
        assert_eq!(s.tick(false), ScaleMismatchAction::None);
        assert_eq!(s.tick(true), ScaleMismatchAction::None);
        assert_eq!(s.tick(false), ScaleMismatchAction::None);
    }

    #[test]
    fn scale_debounce_warn_then_clear() {
        let mut s = ScaleMismatchState::new();
        s.tick(true);
        assert_eq!(s.tick(true), ScaleMismatchAction::Warn);
        assert_eq!(s.tick(true), ScaleMismatchAction::None);
        assert_eq!(s.tick(false), ScaleMismatchAction::Cleared);
    }

    #[test]
    fn scale_debounce_no_double_warn() {
        let mut s = ScaleMismatchState::new();
        s.tick(true);
        assert_eq!(s.tick(true), ScaleMismatchAction::Warn);
        assert_eq!(s.tick(true), ScaleMismatchAction::None);
        assert_eq!(s.tick(true), ScaleMismatchAction::None);
    }

    #[test]
    fn unmapped_uses_original_default_not_handoff_path() {
        let map = make_map(&[("3", "/videos/light01.mp4")]);
        let original_default = "/videos/a007.mp4";
        let result = desired_load("11", &map, original_default, "/videos/light01.mp4");
        assert_eq!(result, Some("/videos/a007.mp4".into()));
    }

    #[test]
    fn unmapped_does_not_use_corrupted_default() {
        let map = make_map(&[("3", "/videos/light01.mp4")]);
        let corrupted_default = "/videos/light01.mp4";
        let result = desired_load("11", &map, corrupted_default, "/videos/light01.mp4");
        assert_eq!(result, None);
    }
}
