use std::ffi::{c_void, CString};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use libmpv2::Mpv;

use smithay_client_toolkit::shell::WaylandSurface;

use crate::error::MuraleError;
use crate::{egl, parse_mpv_options, workspace, EglCore, Murale, SHUTDOWN};

pub(crate) struct SendPtr(pub(crate) *mut libmpv2_sys::mpv_handle);
unsafe impl Send for SendPtr {}

pub(crate) type SharedMpvPtr = Arc<Mutex<SendPtr>>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MpvPath {
    NoHandle,
    Unavailable,
    Path(String),
}

pub(crate) fn mpv_get_path(shared: &SharedMpvPtr) -> MpvPath {
    let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    let handle = guard.0;
    if handle.is_null() {
        return MpvPath::NoHandle;
    }
    let name = match CString::new("path") {
        Ok(n) => n,
        Err(_) => return MpvPath::NoHandle,
    };
    let ptr = unsafe { libmpv2_sys::mpv_get_property_string(handle, name.as_ptr()) };
    if ptr.is_null() {
        return MpvPath::Unavailable;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { libmpv2_sys::mpv_free(ptr as *mut c_void) };
    MpvPath::Path(s)
}

pub(crate) fn mpv_loadfile(shared: &SharedMpvPtr, path: &str) -> bool {
    crate::mem::log_rss(&format!("loadfile-before path={path}"));
    let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    let handle = guard.0;
    if handle.is_null() {
        return false;
    }
    let Ok(cmd) = CString::new("loadfile") else {
        return false;
    };
    let Ok(path_c) = CString::new(path) else {
        return false;
    };
    let args = [cmd.as_ptr(), path_c.as_ptr(), std::ptr::null()];
    let result = unsafe { libmpv2_sys::mpv_command(handle, args.as_ptr() as *mut *const _) };
    crate::mem::log_rss("loadfile-after");
    result == 0
}

pub(crate) fn render_nonblocking(ctx: *mut libmpv2_sys::mpv_render_context, w: i32, h: i32) -> i32 {
    let mut fbo = libmpv2_sys::mpv_opengl_fbo {
        fbo: 0,
        w,
        h,
        internal_format: 0,
    };
    let mut flip: i32 = 1;
    let mut block: i32 = 0;
    #[cfg(feature = "skip-render")]
    let mut skip: i32 = 1;
    let mut params = [
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
            data: &mut fbo as *mut _ as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
            data: &mut flip as *mut _ as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
            data: &mut block as *mut _ as *mut c_void,
        },
        #[cfg(feature = "skip-render")]
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_SKIP_RENDERING,
            data: &mut skip as *mut _ as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    unsafe { libmpv2_sys::mpv_render_context_render(ctx, params.as_mut_ptr()) }
}

const PRE_INIT_OPTIONS: &[&str] = &["config", "config-dir"];

impl Murale {
    pub(crate) fn init_mpv(&mut self, load_path: Option<&str>) -> Result<()> {
        crate::mem::log_rss("init_mpv-start");
        let user_opts: Vec<(String, String)> = self
            .mpv_options
            .as_deref()
            .map(|s| {
                parse_mpv_options(s)
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mpv = Mpv::with_initializer(|init| {
            for (key, value) in &user_opts {
                if PRE_INIT_OPTIONS.contains(&key.as_str()) {
                    init.set_option(key, value.as_str())?;
                }
            }
            Ok(())
        })?;

        mpv.set_property("vo", "libmpv")?;
        mpv.set_property("hwdec", "auto")?;
        mpv.set_property("video-sync", "desync")?;
        mpv.set_property("loop-file", "inf")?;
        mpv.set_property("keepaspect", true)?;
        mpv.set_property("panscan", 1.0f64)?;
        mpv.set_property("background-color", "#00000000")?;
        mpv.set_property("terminal", "no")?;

        for (key, value) in &user_opts {
            if PRE_INIT_OPTIONS.contains(&key.as_str()) {
                continue;
            }
            if key == "profile" {
                mpv.command("apply-profile", &[value.as_str()]).map_err(|e| {
                    anyhow::anyhow!(
                        "failed to apply mpv profile '{value}': {e}; \
                         check the name, and for a user-defined profile pass \
                         config=yes (plus config-dir=... when non-default)"
                    )
                })?;
                continue;
            }
            if let Err(e) = mpv.set_property(key.as_str(), value.as_str()) {
                tracing::warn!("failed to set mpv option {key}={value}: {e}");
            }
        }

        mpv.set_property("vo", "libmpv")?;

        let init_params = OpenGLInitParams {
            get_proc_address: egl::mpv_get_proc_address,
            ctx: self.egl_instance.clone(),
        };

        let render_params = [
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(init_params),
            RenderParam::WaylandDisplay(self.wl_display_ptr),
        ];

        let mut render_ctx = mpv.create_render_context(render_params)?;

        let wakeup_fd_raw = self.wakeup_fd.as_raw_fd();
        render_ctx.set_update_callback(move || {
            let buf = 1u64.to_ne_bytes();
            let _ = nix::unistd::write(unsafe { BorrowedFd::borrow_raw(wakeup_fd_raw) }, &buf);
        });

        let play_path = load_path.unwrap_or(&self.video_path);
        mpv.command("loadfile", &[play_path])?;

        // SAFETY: RenderContext<'a> borrows Mpv, but we need both in the same struct.
        // This transmute erases the lifetime to 'static. It is sound because:
        //   1. render_ctx is declared BEFORE mpv in the struct
        //   2. Rust drops fields in declaration order (top to bottom)
        //   3. Therefore render_ctx always drops before mpv
        //   4. Murale is never moved after init_mpv (lives on the stack in run())
        //   5. RenderContext internally holds a raw *mut mpv_render_context (C pointer),
        //      not a Rust &Mpv reference — the PhantomData lifetime is purely a
        //      borrow-checker hint to enforce drop ordering, which we handle manually
        // DO NOT reorder the render_ctx/mpv fields without updating this invariant.
        let render_ctx: RenderContext<'static> = unsafe { std::mem::transmute(render_ctx) };

        self.render_ctx = Some(render_ctx);
        self.mpv = Some(mpv);
        self.last_mpv_refresh = Instant::now();

        tracing::info!("playing {play_path}");
        crate::mem::log_rss("init_mpv-done");
        Ok(())
    }

    /// Publish the raw mpv handle to the shared pointer so the workspace
    /// listener thread can issue commands. Caller is responsible for ensuring
    /// the correct video is loaded before publishing (R2-B3).
    pub(crate) fn publish_mpv_handle(&self) {
        if let Some(ref mpv) = self.mpv {
            let ptr = self.mpv_ptr.clone();
            ptr.lock().unwrap_or_else(|e| e.into_inner()).0 = mpv.ctx.as_ptr();
        }
    }

    fn teardown_egl(&mut self) -> Result<(), MuraleError> {
        // R2-B2: Take ALL handles before first fallible call.
        // After these takes, self.egl_state and self.egl_core are None —
        // no callbacks or shutdown can reach stale handles.
        let state = self.egl_state.take();
        let core = self.egl_core.take();

        let state = state.ok_or_else(|| MuraleError::Egl("no EGL state to tear down".into()))?;
        let core = core.ok_or_else(|| MuraleError::Egl("no EGL core to tear down".into()))?;

        state.destroy(&self.egl_instance, self.egl_display)?;
        self.egl_instance
            .destroy_context(self.egl_display, core.context)
            .map_err(|e| MuraleError::Egl(format!("destroy_context failed: {e}")))?;
        self.egl_instance
            .terminate(self.egl_display)
            .map_err(|e| MuraleError::Egl(format!("eglTerminate failed: {e}")))?;

        tracing::debug!("EGL teardown complete (display terminated)");
        Ok(())
    }

    fn resolve_desired_video(&self, saved_path: &str) -> String {
        if let Some((ref map, ref default)) = self.workspace_config {
            workspace::get_active_workspace()
                .and_then(|ws| map.get(&ws).cloned())
                .unwrap_or_else(|| default.clone())
        } else {
            saved_path.to_string()
        }
    }

    pub(crate) fn refresh_mpv(&mut self) {
        // Disconnect guard: only refresh when we have a live output + EGL.
        if self.layer.is_none() || self.egl_state.is_none() || self.egl_core.is_none() {
            tracing::info!("refresh deferred — output disconnected");
            self.refresh_pending = true;
            self.last_mpv_refresh = Instant::now();
            return;
        }

        // Phase A: Save state and quiesce.
        // Acquire mpv_ptr mutex, read path+time-pos atomically, then null the
        // pointer — all under the same lock guard.
        let (saved_path, saved_pos) = {
            let ptr = self.mpv_ptr.clone();
            let mut guard = ptr.lock().unwrap_or_else(|e| e.into_inner());
            let result = match &self.mpv {
                Some(mpv) => {
                    let path: String = mpv
                        .get_property("path")
                        .unwrap_or_else(|_| self.video_path.clone());
                    let pos: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
                    (path, pos)
                }
                None => return,
            };
            guard.0 = std::ptr::null_mut();
            result
        };

        tracing::info!("refreshing mpv context (full EGL teardown)");
        crate::mem::log_rss("refresh-start");
        crate::mem::log_extra("refresh-start");

        // Phase B: Tear down mpv (GL context must be current).
        if let Some(ref egl_state) = self.egl_state {
            if let Some(ref core) = self.egl_core {
                let _ = self.egl_instance.make_current(
                    self.egl_display,
                    Some(egl_state.surface),
                    Some(egl_state.surface),
                    Some(core.context),
                );
            }
        }

        self.render_ctx.take();
        if let Some(mpv) = self.mpv.take() {
            mpv.terminate_destroy();
        }
        crate::mem::log_rss("refresh-after-mpv-drop");

        // Phase C: Tear down EGL (fail-closed).
        if let Err(e) = self.teardown_egl() {
            tracing::error!("EGL teardown failed: {e} — shutting down");
            SHUTDOWN.store(true, Ordering::Relaxed);
            self.exit = true;
            return;
        }
        crate::mem::log_rss("refresh-after-egl-teardown");

        // Phase D: Rebuild EGL (reinit_display handles its own rollback).
        match egl::reinit_display(&self.egl_instance, self.egl_display) {
            Ok((config, context)) => {
                self.egl_core = Some(EglCore { config, context });
            }
            Err(e) => {
                tracing::error!("EGL reinit failed: {e} — shutting down");
                SHUTDOWN.store(true, Ordering::Relaxed);
                self.exit = true;
                return;
            }
        }
        crate::mem::log_rss("refresh-after-egl-reinit");

        // Phase E: Rebuild surface.
        let layer = match &self.layer {
            Some(l) => l,
            None => {
                tracing::error!("layer surface lost during refresh — shutting down");
                SHUTDOWN.store(true, Ordering::Relaxed);
                self.exit = true;
                return;
            }
        };
        let core = self.egl_core.as_ref().unwrap();
        let scaled_w = (self.width * self.scale) as i32;
        let scaled_h = (self.height * self.scale) as i32;
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
                tracing::error!("EGL surface recreation failed: {e} — shutting down");
                SHUTDOWN.store(true, Ordering::Relaxed);
                self.exit = true;
                return;
            }
        }
        crate::mem::log_rss("refresh-after-surface-create");

        // Phase F: Rebuild mpv.
        if let Err(e) = self.init_mpv(None) {
            tracing::error!("mpv refresh failed: {e} — shutting down");
            SHUTDOWN.store(true, Ordering::Relaxed);
            self.exit = true;
            return;
        }

        // Workspace intent replay + atomic handle publish (R2-B3 + amendment).
        // Hold the lock for the entire load+publish+stability-check sequence so
        // the workspace listener cannot interleave between load and publish.
        {
            let ptr = self.mpv_ptr.clone();
            let mut guard = ptr.lock().unwrap_or_else(|e| e.into_inner());

            let mut desired = self.resolve_desired_video(&saved_path);

            if let Some(mpv) = &self.mpv {
                if desired != self.video_path {
                    let _ = mpv.command("loadfile", &[&desired]);
                }
                if saved_pos > 1.0 && desired == saved_path {
                    let pos_str = format!("{saved_pos:.1}");
                    let _ = mpv.command("seek", &[&pos_str, "absolute"]);
                }
            }

            // Publish the handle
            if let Some(ref mpv) = self.mpv {
                guard.0 = mpv.ctx.as_ptr();
            }

            // Generation-stability loop: re-check workspace and re-apply if it
            // changed during the load. Bounded by human input rate.
            if self.workspace_config.is_some() {
                loop {
                    let current = self.resolve_desired_video(&saved_path);
                    if current == desired {
                        break;
                    }
                    if let Some(mpv) = &self.mpv {
                        let _ = mpv.command("loadfile", &[&current]);
                    }
                    desired = current;
                }
            }
            // Lock released here — workspace listener can resume
        }

        self.last_time_pos = 0.0;

        let trimmed = unsafe { libc::malloc_trim(0) };
        let elapsed = crate::mem::elapsed_secs();
        tracing::info!(
            "[mem] malloc_trim returned {trimmed} (1=freed, 0=no-op) elapsed={elapsed:.0}s"
        );
        crate::mem::log_rss("refresh-complete");
        crate::mem::log_extra("refresh-complete");

        // Frame callback preservation (B4): the wl_surface survived the refresh,
        // so any pending callback is still live. Mark redraw needed for the next
        // callback. If no callback is pending, kick-start rendering now.
        self.redraw_needed = true;
        if !self.frame_callback_pending {
            self.render_frame();
        }
    }
}
