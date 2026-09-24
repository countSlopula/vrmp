//! Creates the OpenGL context that both OpenXR and libmpv render through.
//!
//! The OpenXR OpenGL binding on Linux is the Xlib/GLX one, so this creates a
//! real GLX context and hands OpenXR its X11 handles. The session runs under
//! Wayland, but XWayland provides the X display, and the context is only ever
//! used for offscreen rendering into swapchain images — nothing is presented to
//! the X window, which exists solely because GLX requires a drawable.
//!
//! Monado-based runtimes also offer `XR_MNDX_egl_enable`, which would avoid X
//! entirely, but the `openxr` crate has no binding for it; GLX is the portable
//! path.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int, c_ulong};
use std::ptr;

use anyhow::{bail, Context, Result};
use x11_dl::glx::{Glx, GLXContext, GLXFBConfig};
use x11_dl::xlib::{self, Xlib};

// GLX framebuffer-config attributes.
const GLX_X_RENDERABLE: c_int = 0x8012;
const GLX_DRAWABLE_TYPE: c_int = 0x8010;
const GLX_WINDOW_BIT: c_int = 0x0000_0001;
const GLX_RENDER_TYPE: c_int = 0x8011;
const GLX_RGBA_BIT: c_int = 0x0000_0001;
const GLX_X_VISUAL_TYPE: c_int = 0x22;
const GLX_TRUE_COLOR: c_int = 0x8002;
const GLX_RED_SIZE: c_int = 8;
const GLX_GREEN_SIZE: c_int = 9;
const GLX_BLUE_SIZE: c_int = 10;
const GLX_ALPHA_SIZE: c_int = 11;
const GLX_DEPTH_SIZE: c_int = 12;
const GLX_DOUBLEBUFFER: c_int = 5;
const GLX_VISUAL_ID: c_int = 0x800B;

// Attributes for glXCreateContextAttribsARB.
const GLX_CONTEXT_MAJOR_VERSION_ARB: c_int = 0x2091;
const GLX_CONTEXT_MINOR_VERSION_ARB: c_int = 0x2092;
const GLX_CONTEXT_PROFILE_MASK_ARB: c_int = 0x9126;
const GLX_CONTEXT_CORE_PROFILE_BIT_ARB: c_int = 0x0000_0001;

type CreateContextAttribsArb = unsafe extern "C" fn(
    dpy: *mut xlib::Display,
    config: GLXFBConfig,
    share: GLXContext,
    direct: c_int,
    attribs: *const c_int,
) -> GLXContext;

/// An OpenGL context plus the X11 handles OpenXR needs to adopt it.
pub struct GlContext {
    pub xlib: Xlib,
    pub glx: Glx,
    pub display: *mut xlib::Display,
    pub window: c_ulong,
    pub context: GLXContext,
    pub fb_config: GLXFBConfig,
    pub visual_id: u32,
}

// The context is created on, and used from, a single thread; it is only marked
// Send so it can be stored alongside other session state.
unsafe impl Send for GlContext {}

impl GlContext {
    /// Opens the X display and creates a core-profile context.
    ///
    /// The GL version is negotiated downwards from 4.6, because mpv's renderer
    /// wants a modern core context but the exact version available depends on
    /// the driver.
    pub fn create() -> Result<Self> {
        let xlib = Xlib::open().context("loading libX11 (is XWayland running?)")?;
        let glx = Glx::open().context("loading libGL/GLX")?;

        let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
        if display.is_null() {
            bail!("cannot open X display — set DISPLAY, or start XWayland");
        }
        let screen = unsafe { (xlib.XDefaultScreen)(display) };

        // Ask for a plain double-buffered RGBA config. Depth is requested so the
        // same context can render the UI with depth testing if that is ever
        // wanted; the video path does not use it.
        let attribs: [c_int; 21] = [
            GLX_X_RENDERABLE, 1,
            GLX_DRAWABLE_TYPE, GLX_WINDOW_BIT,
            GLX_RENDER_TYPE, GLX_RGBA_BIT,
            GLX_X_VISUAL_TYPE, GLX_TRUE_COLOR,
            GLX_RED_SIZE, 8,
            GLX_GREEN_SIZE, 8,
            GLX_BLUE_SIZE, 8,
            GLX_ALPHA_SIZE, 8,
            GLX_DEPTH_SIZE, 24,
            GLX_DOUBLEBUFFER, 1,
            0,
        ];

        let mut count: c_int = 0;
        let configs =
            unsafe { (glx.glXChooseFBConfig)(display, screen, attribs.as_ptr(), &mut count) };
        if configs.is_null() || count == 0 {
            bail!("no suitable GLX framebuffer config");
        }
        let fb_config = unsafe { *configs };
        unsafe { (xlib.XFree)(configs as *mut c_void) };

        let mut visual_id: c_int = 0;
        unsafe {
            (glx.glXGetFBConfigAttrib)(display, fb_config, GLX_VISUAL_ID, &mut visual_id);
        }

        // Resolve the context-creation extension; the legacy glXCreateContext
        // cannot request a core profile.
        let create_attribs: CreateContextAttribsArb = unsafe {
            let name = CString::new("glXCreateContextAttribsARB")?;
            let sym = (glx.glXGetProcAddress)(name.as_ptr() as *const u8);
            match sym {
                Some(f) => std::mem::transmute::<
                    unsafe extern "C" fn(),
                    CreateContextAttribsArb,
                >(f),
                None => bail!("GLX_ARB_create_context is unavailable"),
            }
        };

        let mut context: GLXContext = ptr::null_mut();
        for (major, minor) in [(4, 6), (4, 5), (4, 3), (3, 3)] {
            let ctx_attribs: [c_int; 7] = [
                GLX_CONTEXT_MAJOR_VERSION_ARB, major,
                GLX_CONTEXT_MINOR_VERSION_ARB, minor,
                GLX_CONTEXT_PROFILE_MASK_ARB, GLX_CONTEXT_CORE_PROFILE_BIT_ARB,
                0,
            ];
            let candidate = unsafe {
                (create_attribs)(display, fb_config, ptr::null_mut(), 1, ctx_attribs.as_ptr())
            };
            if !candidate.is_null() {
                context = candidate;
                break;
            }
        }
        if context.is_null() {
            bail!("could not create an OpenGL core context (3.3 or newer required)");
        }

        // GLX needs a drawable to make a context current. The window is never
        // mapped: all rendering goes to swapchain images and framebuffers.
        let visual_info = unsafe { (glx.glXGetVisualFromFBConfig)(display, fb_config) };
        if visual_info.is_null() {
            bail!("GLX returned no visual for the chosen config");
        }
        let root = unsafe { (xlib.XRootWindow)(display, screen) };
        let colormap = unsafe {
            (xlib.XCreateColormap)(display, root, (*visual_info).visual, xlib::AllocNone)
        };
        let mut swa: xlib::XSetWindowAttributes = unsafe { std::mem::zeroed() };
        swa.colormap = colormap;
        let window = unsafe {
            (xlib.XCreateWindow)(
                display,
                root,
                0,
                0,
                16,
                16,
                0,
                (*visual_info).depth,
                xlib::InputOutput as u32,
                (*visual_info).visual,
                xlib::CWColormap,
                &mut swa,
            )
        };
        unsafe { (xlib.XFree)(visual_info as *mut c_void) };

        if unsafe { (glx.glXMakeCurrent)(display, window, context) } == 0 {
            bail!("glXMakeCurrent failed");
        }

        Ok(GlContext { xlib, glx, display, window, context, fb_config, visual_id: visual_id as u32 })
    }

    /// Resolves a GL entry point, for `glow` and for libmpv.
    pub fn proc_address(&self, name: &str) -> *const c_void {
        let Ok(cname) = CString::new(name) else {
            return ptr::null();
        };
        match unsafe { (self.glx.glXGetProcAddress)(cname.as_ptr() as *const u8) } {
            Some(f) => f as *const c_void,
            None => ptr::null(),
        }
    }

    /// Builds a `glow` context for our own drawing (framebuffers, blits, UI).
    pub fn glow(&self) -> glow::Context {
        unsafe { glow::Context::from_loader_function(|name| self.proc_address(name)) }
    }

    pub fn make_current(&self) -> Result<()> {
        if unsafe { (self.glx.glXMakeCurrent)(self.display, self.window, self.context) } == 0 {
            bail!("glXMakeCurrent failed");
        }
        Ok(())
    }
}

impl Drop for GlContext {
    fn drop(&mut self) {
        unsafe {
            (self.glx.glXMakeCurrent)(self.display, 0, ptr::null_mut());
            (self.glx.glXDestroyContext)(self.display, self.context);
            (self.xlib.XDestroyWindow)(self.display, self.window);
            (self.xlib.XCloseDisplay)(self.display);
        }
    }
}

/// C callback handed to libmpv so it can resolve GL entry points.
///
/// # Safety
/// `ctx` must be a valid `*const GlContext` that outlives the mpv render
/// context, and `name` a NUL-terminated string.
pub unsafe extern "C" fn mpv_get_proc_address(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    if ctx.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    let gl = &*(ctx as *const GlContext);
    let Ok(name) = CStr::from_ptr(name).to_str() else {
        return ptr::null_mut();
    };
    gl.proc_address(name) as *mut c_void
}

impl GlContext {
    /// Resizes the window and makes it visible at a useful size.
    ///
    /// It starts tiny because it exists only as a GLX drawable and a keyboard
    /// target; giving it real dimensions is what turns it into a usable mirror.
    pub fn resize_window(&self, width: u32, height: u32) {
        unsafe {
            (self.xlib.XResizeWindow)(self.display, self.window, width.max(1), height.max(1));
            (self.xlib.XFlush)(self.display);
        }
    }

    /// Copies part of a framebuffer into the desktop window.
    ///
    /// Reads from `src_fbo` while it is still acquired — a swapchain image must
    /// not be sampled after release — and scales to fill the window, so one eye
    /// of a side-by-side frame arrives at the right shape rather than squashed.
    ///
    /// # Safety
    /// `src_fbo` must be a complete framebuffer in this GL context.
    pub unsafe fn mirror_framebuffer(
        &self,
        gl: &glow::Context,
        src_fbo: u32,
        src: (i32, i32, i32, i32),
        dst_rect: (i32, i32, i32, i32),
        clear: bool,
    ) {
        use glow::HasContext as _;

        let (sx, sy, sw, sh) = src;
        let (dx, dy, dw, dh) = dst_rect;
        if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 {
            return;
        }

        // Letterboxed within the destination rectangle rather than the whole
        // window, so a control bar composited over video keeps its own shape.
        let (ox, oy, fit_w, fit_h) = letterbox((sw, sh), (dw, dh));
        let (ox, oy) = (dx + ox, dy + oy);

        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
        gl.disable(glow::SCISSOR_TEST);
        if clear {
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        gl.bind_framebuffer(
            glow::READ_FRAMEBUFFER,
            std::num::NonZeroU32::new(src_fbo).map(glow::NativeFramebuffer),
        );
        // Copied straight across, with no vertical flip.
        //
        // Both framebuffers use GL's bottom-left origin, and mpv already renders
        // with `flip_y` set so the swapchain image suits the runtime — so the
        // content is in window orientation by the time it gets here. Adding a
        // flip on top of that inverts it, which is what an earlier version did.
        gl.blit_framebuffer(
            sx,
            sy,
            sx + sw,
            sy + sh,
            ox,
            oy,
            ox + fit_w,
            oy + fit_h,
            glow::COLOR_BUFFER_BIT,
            glow::LINEAR,
        );
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
    }

    /// Shows whatever has been drawn into the window.
    ///
    /// Separate from the blit so several sources can be composited — video with
    /// a control bar over it — before anything is presented, instead of the
    /// window flickering between them.
    pub fn present(&self) {
        unsafe { (self.glx.glXSwapBuffers)(self.display, self.window) };
    }
}

/// Largest centred rectangle of `src`'s shape that fits inside `dst`.
///
/// Letterboxing rather than stretching, because one eye of a side-by-side 180
/// frame is square while the interface is 16:9 and the window a fixed shape —
/// filling it would distort whichever does not match, which is exactly what a
/// view meant for checking things must not do.
///
/// Shared by the blit and by the inverse mapping that turns a mouse position
/// back into surface coordinates, so the two cannot disagree about where the
/// picture actually is.
pub fn letterbox(src: (i32, i32), dst: (i32, i32)) -> (i32, i32, i32, i32) {
    let (sw, sh) = src;
    let (dw, dh) = dst;
    if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 {
        return (0, 0, 0, 0);
    }
    let src_aspect = sw as f32 / sh as f32;
    let dst_aspect = dw as f32 / dh as f32;
    let (fit_w, fit_h) = if src_aspect > dst_aspect {
        (dw, (dw as f32 / src_aspect).round() as i32)
    } else {
        ((dh as f32 * src_aspect).round() as i32, dh)
    };
    ((dw - fit_w) / 2, (dh - fit_h) / 2, fit_w, fit_h)
}

/// Maps a window position onto the surface being mirrored there.
///
/// Returns `None` outside the letterboxed picture, so clicking the black bars
/// does nothing rather than landing on the nearest edge of the interface.
///
/// The vertical axes need no correction despite window coordinates running down
/// and GL's running up: the picture is centred, so its offset is the same
/// measured from either edge.
pub fn window_to_surface(
    window_pos: (f32, f32),
    surface: (i32, i32),
    window: (i32, i32),
) -> Option<(f32, f32)> {
    let (ox, oy, fit_w, fit_h) = letterbox(surface, window);
    if fit_w <= 0 || fit_h <= 0 {
        return None;
    }
    let u = (window_pos.0 - ox as f32) / fit_w as f32;
    let v = (window_pos.1 - oy as f32) / fit_h as f32;
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return None;
    }
    Some((u * surface.0 as f32, v * surface.1 as f32))
}

#[cfg(test)]
mod mirror_tests {
    use super::*;

    /// A square source in a wide window is pillarboxed, not stretched.
    #[test]
    fn a_square_source_is_pillarboxed() {
        let (ox, oy, w, h) = letterbox((1000, 1000), (960, 540));
        assert_eq!((w, h), (540, 540), "should fit to the window height");
        assert_eq!(oy, 0, "no vertical bars when height is the limit");
        assert_eq!(ox, (960 - 540) / 2, "centred horizontally");
    }

    /// A source wider than the window is letterboxed instead.
    #[test]
    fn a_wide_source_is_letterboxed() {
        let (ox, oy, w, h) = letterbox((2048, 420), (960, 540));
        assert_eq!(w, 960, "should fit to the window width");
        assert_eq!(ox, 0);
        assert!(h < 540 && oy > 0, "bars above and below: {h}, {oy}");
    }

    /// The centre of the window maps to the centre of the surface, whatever the
    /// shapes involved — the basic sanity check for clicking where you look.
    #[test]
    fn the_window_centre_maps_to_the_surface_centre() {
        for surface in [(2048, 1152), (2048, 420), (1000, 1000)] {
            let hit = window_to_surface((480.0, 270.0), surface, (960, 540))
                .expect("the centre is always inside the picture");
            let (ex, ey) = (surface.0 as f32 / 2.0, surface.1 as f32 / 2.0);
            assert!(
                (hit.0 - ex).abs() < 2.0 && (hit.1 - ey).abs() < 2.0,
                "surface {surface:?} mapped centre to {hit:?}, expected ({ex}, {ey})"
            );
        }
    }

    /// Corners map to corners rather than drifting, so edge controls stay
    /// reachable.
    #[test]
    fn corners_map_to_corners() {
        // A 16:9 surface exactly fills a 16:9 window, so the mapping is direct.
        let top_left = window_to_surface((0.0, 0.0), (1920, 1080), (960, 540)).unwrap();
        assert!(top_left.0 < 1.0 && top_left.1 < 1.0, "got {top_left:?}");

        let bottom_right =
            window_to_surface((960.0, 540.0), (1920, 1080), (960, 540)).unwrap();
        assert!(
            (bottom_right.0 - 1920.0).abs() < 1.0 && (bottom_right.1 - 1080.0).abs() < 1.0,
            "got {bottom_right:?}"
        );
    }

    /// Clicking a letterbox bar hits nothing, rather than the nearest edge.
    ///
    /// Clamping instead would make the black margin act as a thin strip of
    /// whatever control happens to lie along that edge.
    #[test]
    fn the_bars_are_not_clickable() {
        // A square surface in a wide window leaves bars at the sides.
        let (ox, ..) = letterbox((1000, 1000), (960, 540));
        assert!(ox > 0, "this case should pillarbox");
        assert!(
            window_to_surface((1.0, 270.0), (1000, 1000), (960, 540)).is_none(),
            "a point in the left bar should miss"
        );
        assert!(
            window_to_surface((959.0, 270.0), (1000, 1000), (960, 540)).is_none(),
            "a point in the right bar should miss"
        );
    }
}
