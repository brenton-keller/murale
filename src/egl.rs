use std::ffi::c_void;
use std::sync::Arc;

use khronos_egl as egl;
use wayland_client::Proxy;
use wayland_egl::WlEglSurface;

use crate::error::MuraleError;

pub type EglInstance = egl::DynamicInstance<egl::EGL1_5>;

// R2-B1: egl::Int (i32), not egl::Attrib (usize) — choose_first_config takes &[Int]
pub const CONFIG_ATTRIBS: [egl::Int; 13] = [
    egl::SURFACE_TYPE,
    egl::WINDOW_BIT,
    egl::RENDERABLE_TYPE,
    egl::OPENGL_BIT,
    egl::RED_SIZE,
    8,
    egl::GREEN_SIZE,
    8,
    egl::BLUE_SIZE,
    8,
    egl::ALPHA_SIZE,
    8,
    egl::NONE,
];

pub const GL_VERSIONS: [(egl::Int, egl::Int); 8] = [
    (4, 6),
    (4, 5),
    (4, 4),
    (4, 3),
    (4, 2),
    (4, 1),
    (4, 0),
    (3, 3),
];

pub struct EglState {
    pub surface: egl::Surface,
    pub egl_window: WlEglSurface,
}

pub fn load_egl() -> Result<Arc<EglInstance>, MuraleError> {
    let lib = unsafe { libloading::Library::new("libEGL.so.1") }
        .map_err(|e| MuraleError::Egl(format!("failed to load libEGL.so.1: {e}")))?;
    let egl = unsafe { EglInstance::load_required_from(lib) }
        .map_err(|e| MuraleError::Egl(format!("failed to load EGL functions: {e}")))?;
    Ok(Arc::new(egl))
}

pub fn init_display(
    egl: &Arc<EglInstance>,
    wl_display_ptr: *mut c_void,
) -> Result<(egl::Display, egl::Config, egl::Context), MuraleError> {
    let display = unsafe { egl.get_display(wl_display_ptr as egl::NativeDisplayType) }
        .ok_or_else(|| MuraleError::Egl("eglGetDisplay returned EGL_NO_DISPLAY".into()))?;

    egl.initialize(display)?;
    egl.bind_api(egl::OPENGL_API)?;

    let config = egl
        .choose_first_config(display, &CONFIG_ATTRIBS)?
        .ok_or_else(|| MuraleError::Egl("no suitable EGL config found".into()))?;

    let context = GL_VERSIONS
        .iter()
        .find_map(|&(major, minor)| {
            let ctx_attribs = [
                egl::CONTEXT_MAJOR_VERSION,
                major,
                egl::CONTEXT_MINOR_VERSION,
                minor,
                egl::NONE,
            ];
            egl.create_context(display, config, None, &ctx_attribs).ok()
        })
        .ok_or_else(|| {
            MuraleError::Egl("failed to create OpenGL context (tried 4.6 down to 3.3)".into())
        })?;

    egl.make_current(display, None, None, Some(context))?;

    gl::load_with(|s| {
        egl.get_proc_address(s)
            .map(|f| f as *const c_void)
            .unwrap_or(std::ptr::null())
    });

    tracing::debug!("EGL initialized");

    Ok((display, config, context))
}

/// Re-initialize an EGL display after `eglTerminate`. EGL 1.5 spec §3.2
/// guarantees the same display handle is valid for `eglInitialize` after
/// termination. On any failure, partially-created state is rolled back
/// (new context destroyed, display re-terminated) before returning.
pub fn reinit_display(
    egl: &Arc<EglInstance>,
    display: egl::Display,
) -> Result<(egl::Config, egl::Context), MuraleError> {
    egl.initialize(display)
        .map_err(|e| MuraleError::Egl(format!("eglInitialize failed after terminate: {e}")))?;

    egl.bind_api(egl::OPENGL_API).map_err(|e| {
        let _ = egl.terminate(display);
        MuraleError::Egl(format!("bind_api failed after reinit: {e}"))
    })?;

    let config = egl
        .choose_first_config(display, &CONFIG_ATTRIBS)
        .map_err(|e| {
            let _ = egl.terminate(display);
            MuraleError::Egl(format!("choose_config failed: {e}"))
        })?
        .ok_or_else(|| {
            let _ = egl.terminate(display);
            MuraleError::Egl("no suitable EGL config found".into())
        })?;

    let context = GL_VERSIONS
        .iter()
        .find_map(|&(major, minor)| {
            let ctx_attribs = [
                egl::CONTEXT_MAJOR_VERSION,
                major,
                egl::CONTEXT_MINOR_VERSION,
                minor,
                egl::NONE,
            ];
            egl.create_context(display, config, None, &ctx_attribs).ok()
        })
        .ok_or_else(|| {
            let _ = egl.terminate(display);
            MuraleError::Egl("failed to create OpenGL context (tried 4.6 down to 3.3)".into())
        })?;

    if let Err(e) = egl.make_current(display, None, None, Some(context)) {
        let _ = egl.destroy_context(display, context);
        let _ = egl.terminate(display);
        return Err(MuraleError::Egl(format!(
            "make_current failed after reinit: {e}"
        )));
    }

    gl::load_with(|s| {
        egl.get_proc_address(s)
            .map(|f| f as *const c_void)
            .unwrap_or(std::ptr::null())
    });

    tracing::debug!("EGL re-initialized after terminate");
    Ok((config, context))
}

pub fn create_surface(
    egl: &Arc<EglInstance>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    wl_surface: &wayland_client::protocol::wl_surface::WlSurface,
    width: i32,
    height: i32,
) -> Result<EglState, MuraleError> {
    let egl_window = WlEglSurface::new(wl_surface.id(), width, height)
        .map_err(|_| MuraleError::Egl("failed to create wl_egl_window".into()))?;

    let surface =
        unsafe { egl.create_window_surface(display, config, egl_window.ptr() as _, None) }?;

    egl.make_current(display, Some(surface), Some(surface), Some(context))?;
    egl.swap_interval(display, 0)?;

    unsafe {
        gl::DrawBuffer(gl::BACK);
        gl::ClearColor(0.0, 0.0, 0.0, 0.0);
    }

    tracing::debug!("EGL surface created ({width}x{height})");

    Ok(EglState {
        surface,
        egl_window,
    })
}

pub fn mpv_get_proc_address(egl: &Arc<EglInstance>, name: &str) -> *mut c_void {
    egl.get_proc_address(name)
        .map(|f| f as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

impl EglState {
    pub fn destroy(self, egl: &EglInstance, display: egl::Display) -> Result<(), MuraleError> {
        tracing::debug!("destroying EGL surface");
        egl.make_current(display, None, None, None)
            .map_err(|e| MuraleError::Egl(format!("unbind failed: {e}")))?;
        egl.destroy_surface(display, self.surface)
            .map_err(|e| MuraleError::Egl(format!("destroy_surface failed: {e}")))?;
        Ok(())
    }
}
