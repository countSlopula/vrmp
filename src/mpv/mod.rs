//! Video playback via libmpv's OpenGL render API.
//!
//! mpv does the genuinely hard parts — demuxing, hardware decode, audio output,
//! A/V sync, seeking — and hands us a rendered frame in a framebuffer we own.
//! That framebuffer is an OpenXR swapchain image, so the decoded frame lands
//! directly in the texture the compositor will project, with no intermediate
//! copy of our own.
//!
//! # Threading
//!
//! Every method here must be called from the thread holding the GL context,
//! with one exception: mpv invokes the update callback from its own thread, so
//! that callback does nothing but set an atomic flag.

pub mod ffi;

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

/// Playback state mirrored from mpv's properties, refreshed by [`Player::poll`].
#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    pub duration_secs: f64,
    pub position_secs: f64,
    pub paused: bool,
    pub idle: bool,
    /// Coded size of the current video, needed to size swapchains.
    pub video_width: u32,
    pub video_height: u32,
    /// Seconds of media buffered ahead. Watching this is how we know whether
    /// the network share is keeping up.
    pub cache_secs: f64,
    pub dropped_frames: i64,
    /// Set once a file is loaded and the first frame is ready.
    pub file_loaded: bool,
    /// Which part of a multi-part title is playing, and how many there are.
    pub playlist_pos: i64,
    pub playlist_count: i64,
}

pub struct Player {
    handle: *mut ffi::mpv_handle,
    render: *mut ffi::mpv_render_context,
    /// Set by mpv's update callback, cleared when we render.
    redraw: Arc<AtomicBool>,
    state: PlaybackState,
    /// Seek deferred until the next file finishes loading.
    pending_seek: Option<(usize, f64)>,
}

/// Property ids used when observing, so `poll` can tell them apart.
const PROP_DURATION: u64 = 1;
const PROP_POSITION: u64 = 2;
const PROP_PAUSE: u64 = 3;
const PROP_WIDTH: u64 = 4;
const PROP_HEIGHT: u64 = 5;
const PROP_CACHE: u64 = 6;
const PROP_DROPPED: u64 = 7;
const PROP_IDLE: u64 = 8;
const PROP_PLAYLIST_POS: u64 = 9;
const PROP_PLAYLIST_COUNT: u64 = 10;

impl Player {
    /// Creates a player bound to the current GL context.
    ///
    /// `get_proc_address` resolves GL entry points; it is called only while this
    /// function runs. `cache_secs` and `cache_max_mib` size the read-ahead
    /// buffer, which is the main defence against network stalls.
    pub fn new(
        get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
        proc_ctx: *mut c_void,
        cache_secs: u32,
        cache_max_mib: u32,
    ) -> Result<Self> {
        let handle = unsafe { ffi::mpv_create() };
        if handle.is_null() {
            bail!("mpv_create failed");
        }

        // Options that must be set before initialize.
        let cache_bytes = (cache_max_mib as u64) * 1024 * 1024;
        let opts: Vec<(&str, String)> = vec![
            // We render it ourselves; mpv must not open a window.
            ("vo", "libmpv".into()),
            // Hardware decode where the GPU supports it. "auto-safe" falls back
            // to software automatically, which matters because some VR files
            // exceed the fixed-function decoder's maximum dimensions.
            ("hwdec", "auto-safe".into()),
            // Use every core for the software fallback path.
            ("vd-lavc-threads", "0".into()),
            // Generous read-ahead: the library lives on a network share, and a
            // stall mid-scene is the worst failure this player can have.
            ("cache", "yes".into()),
            ("cache-secs", cache_secs.to_string()),
            ("demuxer-max-bytes", cache_bytes.to_string()),
            ("demuxer-max-back-bytes", (cache_bytes / 4).to_string()),
            ("demuxer-readahead-secs", cache_secs.to_string()),
            // Keep decoding while seeking around rather than tearing down.
            ("keep-open", "yes".into()),
            // We drive presentation from the headset, so let mpv render frames
            // on demand rather than to a display clock it cannot see.
            ("video-timing-offset", "0".into()),
            // Filling the swapchain exactly; letterboxing would waste pixels and
            // misalign the equirect projection.
            ("keepaspect", "no".into()),
            ("terminal", "no".into()),
        ];
        for (name, value) in &opts {
            set_option(handle, name, value)
                .with_context(|| format!("setting mpv option {name}={value}"))?;
        }

        check(handle, unsafe { ffi::mpv_initialize(handle) }, "mpv_initialize")?;

        // Warnings and errors go to our log; anything more is far too chatty.
        let level = CString::new("warn")?;
        unsafe { ffi::mpv_request_log_messages(handle, level.as_ptr()) };

        // Build the OpenGL render context against the current GL context.
        let mut init = ffi::mpv_opengl_init_params {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: proc_ctx,
        };
        let api = CString::new("opengl")?;
        let mut advanced: c_int = 1;
        let mut params = [
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_API_TYPE,
                data: api.as_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut _ as *mut c_void,
            },
            // Lets mpv schedule its own work rather than assuming we render on
            // a fixed display clock.
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: &mut advanced as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];

        let mut render: *mut ffi::mpv_render_context = std::ptr::null_mut();
        let rc = unsafe { ffi::mpv_render_context_create(&mut render, handle, params.as_mut_ptr()) };
        if rc < 0 {
            unsafe { ffi::mpv_terminate_destroy(handle) };
            bail!("mpv_render_context_create failed: {}", ffi::error_string(rc));
        }

        let redraw = Arc::new(AtomicBool::new(true));
        unsafe {
            ffi::mpv_render_context_set_update_callback(
                render,
                Some(on_mpv_update),
                Arc::as_ptr(&redraw) as *mut c_void,
            );
        }

        let mut player = Player {
            handle,
            render,
            redraw,
            state: PlaybackState::default(),
            pending_seek: None,
        };
        player.observe_properties()?;
        Ok(player)
    }

    fn observe_properties(&mut self) -> Result<()> {
        let to_watch: &[(u64, &str, c_int)] = &[
            (PROP_DURATION, "duration", ffi::MPV_FORMAT_DOUBLE),
            (PROP_POSITION, "time-pos", ffi::MPV_FORMAT_DOUBLE),
            (PROP_PAUSE, "pause", ffi::MPV_FORMAT_FLAG),
            (PROP_WIDTH, "width", ffi::MPV_FORMAT_INT64),
            (PROP_HEIGHT, "height", ffi::MPV_FORMAT_INT64),
            (PROP_CACHE, "demuxer-cache-duration", ffi::MPV_FORMAT_DOUBLE),
            (PROP_DROPPED, "frame-drop-count", ffi::MPV_FORMAT_INT64),
            (PROP_IDLE, "idle-active", ffi::MPV_FORMAT_FLAG),
            (PROP_PLAYLIST_POS, "playlist-pos", ffi::MPV_FORMAT_INT64),
            (PROP_PLAYLIST_COUNT, "playlist-count", ffi::MPV_FORMAT_INT64),
        ];
        for (id, name, format) in to_watch {
            let cname = CString::new(*name)?;
            let rc = unsafe { ffi::mpv_observe_property(self.handle, *id, cname.as_ptr(), *format) };
            check(self.handle, rc, "mpv_observe_property")?;
        }
        Ok(())
    }

    pub fn state(&self) -> &PlaybackState {
        &self.state
    }

    /// Starts playing a file, replacing anything already loaded.
    pub fn load(&mut self, path: &Path) -> Result<()> {
        self.pending_seek = None;
        self.load_playlist(std::slice::from_ref(&path.to_path_buf()), 0)
    }

    /// Queues a seek to apply once `part` has finished loading.
    ///
    /// Seeking immediately after `loadfile` is unreliable: mpv has not opened
    /// the file yet, so the command can be dropped and playback silently starts
    /// from zero. Deferring until the file is actually loaded is what makes
    /// resuming a half-watched title work.
    ///
    /// The part is recorded alongside the offset because loading a playlist
    /// starts on its first entry before switching to the requested one, so more
    /// than one file may load before the intended target does. Without the
    /// check, resuming disc C at forty minutes would seek forty minutes into
    /// disc A.
    pub fn seek_once_loaded(&mut self, part: usize, position: f64) {
        self.pending_seek = Some((part, position));
    }

    /// Reads mpv's current playlist position directly.
    ///
    /// Queried rather than taken from the observed copy, because property
    /// notifications can lag the `FILE_LOADED` event that needs this answer.
    fn current_playlist_pos(&self) -> i64 {
        let Ok(name) = CString::new("playlist-pos") else {
            return -1;
        };
        let mut value: i64 = -1;
        let rc = unsafe {
            ffi::mpv_get_property(
                self.handle,
                name.as_ptr(),
                ffi::MPV_FORMAT_INT64,
                &mut value as *mut _ as *mut c_void,
            )
        };
        if rc < 0 {
            -1
        } else {
            value
        }
    }

    /// Loads every part of a title as one playlist and starts at `start_index`.
    ///
    /// Titles are often split across several files, arriving as discs `_A`,
    /// `_B`, `_C`. Handing them to mpv as a
    /// playlist means it advances between them itself, so a part ending runs
    /// straight into the next one with the decoder already warm, and seeking
    /// between parts is just a playlist position change.
    pub fn load_playlist(&mut self, paths: &[std::path::PathBuf], start_index: usize) -> Result<()> {
        if paths.is_empty() {
            bail!("cannot load an empty playlist");
        }

        for (i, path) in paths.iter().enumerate() {
            let path_c = CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|_| anyhow!("path contains a NUL byte: {}", path.display()))?;
            let cmd = CString::new("loadfile")?;
            // The first entry replaces whatever was playing; the rest queue up
            // behind it.
            let mode = CString::new(if i == 0 { "replace" } else { "append" })?;
            let args = [cmd.as_ptr(), path_c.as_ptr(), mode.as_ptr(), std::ptr::null()];
            let rc = unsafe { ffi::mpv_command(self.handle, args.as_ptr()) };
            check(self.handle, rc, "loadfile")?;
        }

        self.state.file_loaded = false;
        self.state.playlist_count = paths.len() as i64;
        if start_index > 0 {
            self.set_playlist_pos(start_index)?;
        }
        Ok(())
    }

    /// Jumps to a specific part of the loaded title.
    pub fn set_playlist_pos(&mut self, index: usize) -> Result<()> {
        let mut value = index as i64;
        let name = CString::new("playlist-pos")?;
        let rc = unsafe {
            ffi::mpv_set_property(
                self.handle,
                name.as_ptr(),
                ffi::MPV_FORMAT_INT64,
                &mut value as *mut _ as *mut c_void,
            )
        };
        check(self.handle, rc, "set playlist-pos")
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        let mut flag: c_int = if paused { 1 } else { 0 };
        let name = CString::new("pause")?;
        let rc = unsafe {
            ffi::mpv_set_property(
                self.handle,
                name.as_ptr(),
                ffi::MPV_FORMAT_FLAG,
                &mut flag as *mut _ as *mut c_void,
            )
        };
        check(self.handle, rc, "set pause")
    }

    pub fn toggle_pause(&mut self) -> Result<()> {
        let paused = self.state.paused;
        self.set_paused(!paused)
    }

    /// Seeks by `delta` seconds, clamped by mpv to the file's bounds.
    pub fn seek_relative(&mut self, delta: f64) -> Result<()> {
        self.command(&["seek", &format!("{delta}"), "relative"])
    }

    pub fn seek_absolute(&mut self, position: f64) -> Result<()> {
        self.command(&["seek", &format!("{position}"), "absolute"])
    }

    pub fn stop(&mut self) -> Result<()> {
        self.command(&["stop"])
    }

    /// Sets the audio volume as a percentage, where 100 is unattenuated.
    pub fn set_volume(&mut self, percent: f64) -> Result<()> {
        let value = CString::new(format!("{:.1}", percent.clamp(0.0, 150.0)))?;
        let name = CString::new("volume")?;
        let rc = unsafe { ffi::mpv_set_property_string(self.handle, name.as_ptr(), value.as_ptr()) };
        check(self.handle, rc, "set volume")
    }

    fn command(&mut self, args: &[&str]) -> Result<()> {
        let owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a))
            .collect::<std::result::Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { ffi::mpv_command(self.handle, ptrs.as_ptr()) };
        check(self.handle, rc, args[0])
    }

    /// True when mpv has a new frame waiting to be drawn.
    pub fn needs_redraw(&self) -> bool {
        // The flag catches the callback; asking mpv directly catches frames
        // that became ready without one, which `ADVANCED_CONTROL` permits.
        if self.redraw.load(Ordering::Acquire) {
            return true;
        }
        let flags = unsafe { ffi::mpv_render_context_update(self.render) };
        flags & ffi::MPV_RENDER_UPDATE_FRAME != 0
    }

    /// Draws the current frame into an OpenGL framebuffer.
    ///
    /// `fbo` is typically an OpenXR swapchain image wrapped in a framebuffer, so
    /// the decoded frame is written straight into the texture the compositor
    /// projects. `flip_y` is set because GL's origin is bottom-left while
    /// swapchain images are top-left.
    pub fn render_to_fbo(&mut self, fbo: u32, width: u32, height: u32) -> Result<()> {
        self.redraw.store(false, Ordering::Release);

        let mut gl_fbo = ffi::mpv_opengl_fbo {
            fbo: fbo as c_int,
            w: width as c_int,
            h: height as c_int,
            internal_format: 0,
        };
        let mut flip_y: c_int = 1;
        let mut params = [
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut gl_fbo as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let rc = unsafe { ffi::mpv_render_context_render(self.render, params.as_mut_ptr()) };
        if rc < 0 {
            bail!("mpv render failed: {}", ffi::error_string(rc));
        }
        Ok(())
    }

    /// Tells mpv the frame reached the display, which it uses for timing.
    pub fn report_swap(&mut self) {
        unsafe { ffi::mpv_render_context_report_swap(self.render) };
    }

    /// Drains pending events and refreshes [`PlaybackState`].
    ///
    /// Returns false once mpv has shut down, at which point the player must not
    /// be used again.
    pub fn poll(&mut self) -> bool {
        loop {
            // Zero timeout: never block the render loop on mpv.
            let event = unsafe { ffi::mpv_wait_event(self.handle, 0.0) };
            if event.is_null() {
                return true;
            }
            let event = unsafe { &*event };
            match event.event_id {
                ffi::MPV_EVENT_NONE => return true,
                ffi::MPV_EVENT_SHUTDOWN => return false,
                ffi::MPV_EVENT_FILE_LOADED | ffi::MPV_EVENT_PLAYBACK_RESTART => {
                    self.state.file_loaded = true;
                    self.state.idle = false;
                    // A file is open now. Apply a queued resume only if it is
                    // the part the offset belongs to, and clear it either way
                    // once that part is reached, so later parts start from
                    // their beginning.
                    if let Some((part, position)) = self.pending_seek {
                        if self.current_playlist_pos() == part as i64 {
                            self.pending_seek = None;
                            if let Err(e) = self.seek_absolute(position) {
                                eprintln!("warning: could not resume at {position:.0}s: {e}");
                            }
                        }
                    }
                }
                ffi::MPV_EVENT_START_FILE => {
                    self.state.file_loaded = false;
                }
                ffi::MPV_EVENT_END_FILE => {
                    self.state.file_loaded = false;
                }
                ffi::MPV_EVENT_LOG_MESSAGE => {
                    let msg = unsafe { &*(event.data as *const ffi::mpv_event_log_message) };
                    let text = unsafe { CStr::from_ptr(msg.text) }.to_string_lossy();
                    let prefix = unsafe { CStr::from_ptr(msg.prefix) }.to_string_lossy();
                    eprintln!("[mpv/{prefix}] {}", text.trim_end());
                }
                ffi::MPV_EVENT_PROPERTY_CHANGE => {
                    let prop = unsafe { &*(event.data as *const ffi::mpv_event_property) };
                    self.apply_property(event.reply_userdata, prop);
                }
                _ => {}
            }
        }
    }

    fn apply_property(&mut self, id: u64, prop: &ffi::mpv_event_property) {
        if prop.data.is_null() {
            return;
        }
        // Safety: mpv guarantees `data` points to a value of the format that was
        // requested when the property was observed.
        unsafe {
            match (id, prop.format) {
                (PROP_DURATION, ffi::MPV_FORMAT_DOUBLE) => {
                    self.state.duration_secs = *(prop.data as *const f64)
                }
                (PROP_POSITION, ffi::MPV_FORMAT_DOUBLE) => {
                    self.state.position_secs = *(prop.data as *const f64)
                }
                (PROP_CACHE, ffi::MPV_FORMAT_DOUBLE) => {
                    self.state.cache_secs = *(prop.data as *const f64)
                }
                (PROP_PAUSE, ffi::MPV_FORMAT_FLAG) => {
                    self.state.paused = *(prop.data as *const c_int) != 0
                }
                (PROP_IDLE, ffi::MPV_FORMAT_FLAG) => {
                    self.state.idle = *(prop.data as *const c_int) != 0
                }
                (PROP_WIDTH, ffi::MPV_FORMAT_INT64) => {
                    self.state.video_width = (*(prop.data as *const i64)).max(0) as u32
                }
                (PROP_HEIGHT, ffi::MPV_FORMAT_INT64) => {
                    self.state.video_height = (*(prop.data as *const i64)).max(0) as u32
                }
                (PROP_DROPPED, ffi::MPV_FORMAT_INT64) => {
                    self.state.dropped_frames = *(prop.data as *const i64)
                }
                (PROP_PLAYLIST_POS, ffi::MPV_FORMAT_INT64) => {
                    self.state.playlist_pos = *(prop.data as *const i64)
                }
                (PROP_PLAYLIST_COUNT, ffi::MPV_FORMAT_INT64) => {
                    self.state.playlist_count = *(prop.data as *const i64)
                }
                _ => {}
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The render context must go first: it holds GL resources and borrows
        // the client handle.
        unsafe {
            ffi::mpv_render_context_free(self.render);
            ffi::mpv_terminate_destroy(self.handle);
        }
    }
}

/// Called by mpv from its own thread when a new frame is ready.
///
/// Must do essentially nothing — no GL, no locking — so it only raises a flag
/// the render loop reads.
unsafe extern "C" fn on_mpv_update(ctx: *mut c_void) {
    let flag = &*(ctx as *const AtomicBool);
    flag.store(true, Ordering::Release);
}

fn set_option(handle: *mut ffi::mpv_handle, name: &str, value: &str) -> Result<()> {
    let name_c = CString::new(name)?;
    let value_c = CString::new(value)?;
    let rc = unsafe { ffi::mpv_set_option_string(handle, name_c.as_ptr(), value_c.as_ptr()) };
    check(handle, rc, name)
}

fn check(_handle: *mut ffi::mpv_handle, rc: c_int, what: &str) -> Result<()> {
    if rc < 0 {
        bail!("{what}: {}", ffi::error_string(rc));
    }
    Ok(())
}
