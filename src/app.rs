//! The running player: owns the OpenXR session, the decoder, and the UI, and
//! drives the frame loop that connects them.


use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use openxr as xr;

use crate::config::{Config, Paths};
use crate::library::index::{Overrides, ProbeCache};
use crate::library::{Layout, Projection, Stereo, Title};
use crate::mpv::Player;
use crate::ui::pointer::CylinderPanel;
use crate::ui::{
    Action, Ui, View, UI_CONTROLS_HEIGHT, UI_CONTROLS_WIDTH, UI_HEIGHT, UI_WIDTH,
};
use crate::xr::{gl_context::GlContext, EquirectGeometry, Eye, RenderTarget, Xr};

/// Assumed eye height when the runtime reports a floor-relative space. Content
/// is placed at this height so it sits in front of the viewer rather than at
/// their feet.
const EYE_HEIGHT_M: f32 = 1.6;

/// What is currently loaded. Which part is playing is not tracked here:
/// mpv owns the playlist, so `playlist-pos` is the single source of truth and
/// stays correct when mpv advances between parts on its own.
struct Playing {
    title_index: usize,
    layout: Layout,
    /// Yaw the content is anchored to, captured when playback began so the
    /// video faces wherever the viewer was looking.
    yaw: f32,
    /// Recline the content is anchored to, so titles meant to be watched lying
    /// down can be tipped to match the viewer.
    pitch: f32,
    /// Swapchain sized to this video; rebuilt when the resolution changes.
    target: Option<RenderTarget>,
}

pub struct App {
    paths: Paths,
    config: Config,
    titles: Vec<Title>,
    overrides: Overrides,

    // Declaration order below is load-bearing. Struct fields drop in the order
    // they are declared, and everything here that owns GPU objects — the mpv
    // render context, egui's painter, the swapchain framebuffers — makes OpenGL
    // calls as it is destroyed. The GL context must therefore outlive all of
    // them, so it is declared last and dropped last. Moving it earlier turns a
    // clean shutdown into a segfault on a destroyed context.
    playing: Option<Playing>,
    ui_target: RenderTarget,
    /// Separate, correctly-shaped target for the playback transport bar.
    controls_target: RenderTarget,
    ui: Ui,
    player: Player,
    input: crate::xr::input::Input,
    xr: Xr,
    glow: Arc<glow::Context>,
    /// Boxed so its address is stable: libmpv is handed a pointer to this for
    /// resolving GL entry points, and moving the struct must not invalidate it.
    /// Kept alive for the whole session even though nothing reads it again.
    #[allow(dead_code)]
    gl: Box<GlContext>,

    /// Progress of background cover generation, shown to the viewer so a slow
    /// first run has a visible explanation.
    cover_progress: Arc<crate::library::thumbs::Progress>,
    /// Desktop mirror size in pixels. Zero when mirroring is disabled, in
    /// which case the window stays at its minimal input-only size.
    mirror_size: (i32, i32),
    /// Latest mouse position in window pixels, and when the mouse was last
    /// used. Whichever of mouse or controller moved most recently owns the
    /// pointer, so picking one up takes over without a mode switch.
    mouse_window_pos: Option<(f32, f32)>,
    mouse_down: bool,
    mouse_active_at: f64,
    vr_active_at: f64,
    ui_visible: bool,
    /// Heading and height the panel was placed at, fixed while it is on screen
    /// so it does not drift with the viewer's gaze. Cleared when hidden, so it
    /// reappears in front of them next time.
    panel_anchor: Option<Anchor>,
    /// Reads text input from X11; OpenXR provides no keyboard.
    keyboard: crate::xr::desktop::DesktopInput,
    /// Time after which the panel hides itself during playback.
    controls_until: f64,
    started: Instant,
}

/// Where content is placed: the viewer's heading, how far they are reclined,
/// and where their head is. Captured when a panel appears or the view is
/// recentred, then held so nothing drifts with their gaze.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    yaw: f32,
    /// Recline, so content meant to be watched lying down can be tipped to
    /// match the viewer. Small head tilts are flattened to zero, so ordinary
    /// seated use never accumulates a lean.
    pitch: f32,
    head: xr::Vector3f,
}

/// How long the playback controls stay up after the last controller input.
const CONTROLS_LINGER_S: f64 = 8.0;

/// How far below the eye line the playback controls sit, in degrees.
const CONTROLS_PITCH_DEG: f32 = 14.0;

impl App {
    pub fn new(paths: Paths, config: Config) -> Result<Self> {
        // Scan before touching the headset, so library problems surface as
        // plain terminal errors rather than inside VR.
        let mut cache = ProbeCache::load(&paths.probe_cache);
        let report = crate::library::scan::scan(&config.roots, &mut cache)?;
        cache.save(&paths.probe_cache)?;
        let mut overrides = Overrides::load(&paths.overrides);
        let mut titles = report.titles;
        crate::library::index::apply_overrides(&mut titles, &overrides);
        eprintln!("library: {} titles", titles.len());

        // Prune resume points for titles that no longer exist, so the file does
        // not accumulate entries forever.
        let known: std::collections::HashSet<&str> =
            titles.iter().map(|t| t.id.as_str()).collect();
        overrides.resume.retain(|id, _| known.contains(id.as_str()));
        overrides.resume_part.retain(|id, _| known.contains(id.as_str()));

        // Cover art is a cache, so anything missing is rebuilt in the
        // background rather than being a separate step someone has to remember.
        // Started before the headset is touched so it overlaps with session
        // setup instead of adding to it.
        let cover_progress =
            crate::library::thumbs::spawn_generator(
                &paths.covers_dir,
                &paths.cover_log,
                &titles,
            );

        let gl = Box::new(GlContext::create().context("creating the OpenGL context")?);
        let glow = Arc::new(gl.glow());

        let xr = Xr::new(&gl).context("starting OpenXR")?;
        let input = crate::xr::input::Input::new(&xr.instance, &xr.session)?;

        let player = Player::new(
            crate::xr::gl_context::mpv_get_proc_address,
            &*gl as *const GlContext as *mut std::ffi::c_void,
            config.cache_secs,
            config.cache_max_mib,
        )
        .context("starting libmpv")?;

        // Maps the GLX window so it can take focus and receive key events. This
        // is the only route text can reach the app.
        let keyboard =
            crate::xr::desktop::DesktopInput::new(&gl.xlib, gl.display, gl.window);
        eprintln!(
            "keyboard: focus the \"vrmp keyboard input\" window (id 0x{:x}) to type",
            keyboard.window()
        );

        // Give the input window a useful size when it is doubling as a mirror,
        // and leave it minimal otherwise so it stays out of the way.
        let mirror_size = if config.mirror_window {
            let w = config.mirror_width.clamp(160, 3840) as i32;
            let h = (w as f32 * 9.0 / 16.0).round() as i32;
            gl.resize_window(w as u32, h as u32);
            (w, h)
        } else {
            (0, 0)
        };

        let ui = Ui::new(&glow)?;
        let ui_target = RenderTarget::new(&xr.session, &glow, UI_WIDTH, UI_HEIGHT)
            .context("creating the UI swapchain")?;
        let controls_target = RenderTarget::new(
            &xr.session,
            &glow,
            UI_CONTROLS_WIDTH,
            UI_CONTROLS_HEIGHT,
        )
        .context("creating the playback controls swapchain")?;

        Ok(App {
            paths,
            config,
            titles,
            overrides,
            gl,
            glow,
            xr,
            input,
            player,
            ui,
            ui_target,
            controls_target,
            playing: None,
            cover_progress,
            mirror_size,
            mouse_window_pos: None,
            mouse_down: false,
            mouse_active_at: 0.0,
            vr_active_at: 0.0,
            ui_visible: true,
            panel_anchor: None,
            keyboard,
            controls_until: 0.0,
            started: Instant::now(),
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let mut event_buffer = xr::EventDataBuffer::new();

        loop {
            if !self.xr.poll_events(&mut event_buffer)? {
                break;
            }
            if !self.player.poll() {
                eprintln!("mpv shut down");
                break;
            }
            if !self.xr.is_running() {
                // Session is idle; avoid spinning the CPU while the headset is
                // asleep or the runtime is not ready.
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }

            if !self.frame()? {
                break;
            }
        }

        self.save_resume_point();
        self.overrides.save(&self.paths.overrides)?;
        Ok(())
    }

    /// Renders and submits one frame. Returns false to quit.
    fn frame(&mut self) -> Result<bool> {
        let frame_state = self.xr.frame_wait.wait()?;
        self.xr.frame_stream.begin()?;

        if !frame_state.should_render {
            // The runtime does not want content this frame (headset off the
            // head, for instance); submit nothing rather than wasting work.
            self.xr.frame_stream.end(
                frame_state.predicted_display_time,
                self.xr.environment_blend_mode,
                &[],
            )?;
            return Ok(true);
        }

        let time = frame_state.predicted_display_time;
        let input = self.input.sync(&self.xr.session, &self.xr.stage, time)?;

        // Any deliberate controller activity counts as "still using it", which
        // keeps the controls up while they are being operated.
        let hand_now = input.active_hand.index();
        let active = input.select[hand_now]
            || input.stick[hand_now].0.abs() > 0.3
            || input.stick[hand_now].1.abs() > 0.3;
        if active {
            self.controls_until = self.started.elapsed().as_secs_f64() + CONTROLS_LINGER_S;
        }

        // Recentre everything on the viewer: the panel and, if something is
        // playing, the video itself. Needed because both are anchored where the
        // viewer was when they appeared, and people move — turning in a chair is
        // enough to leave the controls behind you.
        if input.recenter_pressed {
            let anchor = self.measure_anchor(time);
            self.panel_anchor = Some(anchor);
            if let Some(playing) = &mut self.playing {
                playing.yaw = anchor.yaw;
                playing.pitch = anchor.pitch;
            }
            // Bring the controls back up, since recentring is usually the
            // reaction to not being able to find them.
            self.ui_visible = true;
            self.controls_until = self.started.elapsed().as_secs_f64() + CONTROLS_LINGER_S;
            eprintln!("recentred on the viewer");
        }

        // The menu button switches between the library and what is playing,
        // rather than showing and hiding the panel. Hiding is handled by the
        // controls fading on their own, so the button is better spent on the
        // one navigation step that otherwise needs a small on-screen target.
        if input.menu_pressed {
            match self.ui.view {
                View::Playing => {
                    self.ui.view = View::Library;
                    // The library is the thing being looked at, so it appears
                    // straight away and re-anchors to the viewer.
                    self.ui_visible = false;
                    self.set_ui_visible(true, time);
                }
                // Only worth leaving the library if there is something to go
                // back to.
                View::Library if self.playing.is_some() => {
                    self.ui.view = View::Playing;
                    // Returning to playback means wanting to watch, so the video
                    // is left unobstructed rather than covered by the transport
                    // bar the viewer did not ask for.
                    self.set_ui_visible(false, time);
                }
                View::Library => {}
            }
        }

        // Show or hide the controls without leaving playback. The library never
        // hides, since doing so would leave nothing on screen and no obvious way
        // back to it.
        if input.toggle_controls_pressed && self.ui.view == View::Playing {
            let showing = !self.ui_visible;
            self.set_ui_visible(showing, time);
            if showing {
                // Asked for explicitly, so give the full linger rather than
                // whatever remained of a previous one.
                self.controls_until =
                    self.started.elapsed().as_secs_f64() + CONTROLS_LINGER_S;
            }
        }

        // The library must always be reachable, so it forces the panel visible.
        if self.ui.view == View::Library {
            self.set_ui_visible(true, time);
        } else if self.ui_visible && self.started.elapsed().as_secs_f64() > self.controls_until {
            // During playback the panel would otherwise sit over the video
            // permanently, so it fades out once it stops being used. The menu
            // button brings it straight back.
            self.set_ui_visible(false, time);
        }

        // The panel is placed once, when it appears, and then stays put.
        //
        // Recomputing this every frame would re-centre it on the viewer's gaze
        // continuously, so it would drift away from wherever the controller was
        // aimed and make anything on it impossible to click.
        let anchor = match self.panel_anchor {
            Some(a) => a,
            None => {
                let anchor = self.measure_anchor(time);
                self.panel_anchor = Some(anchor);
                anchor
            }
        };
        // The library and the playback controls want different shapes, so each
        // gets its own panel geometry over the same swapchain. The controls are
        // a short wide strip sitting below the eye line; the library is a tall
        // panel straight ahead.
        // Each view draws into its own swapchain, sized to its content, so the
        // layer always shows the whole image and never has to address a
        // sub-rect of a differently shaped one.
        let playing_view = self.ui.view == View::Playing;
        let surface = if playing_view {
            (UI_CONTROLS_WIDTH, UI_CONTROLS_HEIGHT)
        } else {
            (UI_WIDTH, UI_HEIGHT)
        };
        let aspect = surface.0 as f32 / surface.1 as f32;
        let panel = CylinderPanel::facing_with_pitch(
            anchor.yaw,
            anchor.head,
            self.config.ui_distance_m,
            aspect,
            self.config.ui_angle_deg,
            // Resting gaze sits below horizontal, so the transport bar goes
            // there — glanceable without covering the video. The library sits
            // level, since it is the thing being looked at.
            anchor.pitch,
            if playing_view { CONTROLS_PITCH_DEG } else { 0.0 },
        );

        // Point with whichever hand is in use.
        let hand = input.active_hand.index();
        let ray_pointer = if self.ui_visible {
            input.aim[hand]
                .and_then(|aim| panel.hit(aim, (surface.0 as f32, surface.1 as f32)))
        } else {
            None
        };
        if ray_pointer.is_some() || input.select[hand] {
            self.vr_active_at = self.started.elapsed().as_secs_f64();
        }

        // Whichever input was used most recently owns the pointer. Picking up a
        // controller or touching the mouse simply takes over, with no mode to
        // switch and nothing to remember — and because it is driven by actual
        // activity, an idle device never fights the one in use.
        // Mapped through wherever the interface actually sits in the window —
        // the bottom bar during playback, the whole window otherwise — so a
        // click lands on the control under the cursor rather than being scaled
        // from the wrong rectangle.
        let band = self.mirror_band(playing_view);
        let mouse_pointer = self.mouse_window_pos.and_then(|(mx, my)| {
            // X11 reports y downwards; the band is expressed in GL's upward
            // coordinates, so flip before testing against it.
            let flipped_y = self.mirror_size.1 as f32 - my;
            let local = (mx - band.0 as f32, flipped_y - band.1 as f32);
            if local.0 < 0.0
                || local.1 < 0.0
                || local.0 > band.2 as f32
                || local.1 > band.3 as f32
            {
                return None;
            }
            crate::xr::gl_context::window_to_surface(
                // Back to y-down within the band, which is what the surface
                // mapping expects.
                (local.0, band.3 as f32 - local.1),
                (surface.0 as i32, surface.1 as i32),
                (band.2, band.3),
            )
            .map(|(x, y)| egui::pos2(x, y))
        });
        let mouse_owns = self.config.mirror_window
            && self.mouse_active_at > self.vr_active_at
            && mouse_pointer.is_some();

        let pointer = if mouse_owns { mouse_pointer } else { ray_pointer };
        // The held state, not the rising edge: egui needs the button to stay
        // down across frames for a drag to happen at all.
        //
        // Only the controller half is gated on the panel being visible. The
        // window keeps its controls up permanently, so gating the mouse on the
        // *headset's* panel made clicks stop working there as soon as the VR
        // controls faded — the two displays share an interface but not its
        // visibility.
        let held = if mouse_owns {
            self.mouse_down
        } else {
            self.ui_visible && input.select[hand]
        };

        // Thumbstick scrolls the library and scrubs during playback.
        let stick_y = input.stick[hand].1;
        // Pushing the stick up moves the grid up, revealing titles below —
        // the direction the content moves, matching a touchscreen rather than
        // a scroll wheel.
        let scroll = if self.ui.view == View::Library {
            stick_y * 40.0
        } else {
            0.0
        };
        if self.ui.view == View::Playing {
            let stick_x = input.stick[hand].0;
            if stick_x.abs() > 0.6 {
                // Nudge rather than continuous scrub, so a resting thumb does
                // not run away with the position.
                self.player.seek_relative(stick_x.signum() as f64 * 5.0).ok();
            }
        }

        // Keyboard and mouse arrive over X11; OpenXR provides neither.
        let mut keys = Vec::new();
        for event in self.keyboard.poll(&self.gl.xlib) {
            match event {
                crate::xr::desktop::DesktopEvent::Key(key) => keys.push(key),
                crate::xr::desktop::DesktopEvent::MouseMoved { x, y } => {
                    self.mouse_window_pos = Some((x, y));
                    self.mouse_active_at = self.started.elapsed().as_secs_f64();
                }
                crate::xr::desktop::DesktopEvent::MouseButton { pressed } => {
                    self.mouse_down = pressed;
                    self.mouse_active_at = self.started.elapsed().as_secs_f64();
                }
            }
        }

        // Reported, not acted on. The viewer is told that generation is running
        // and what it may cost, so a sluggish first run has a visible cause
        // instead of the worker silently throttling itself. Failures keep the
        // status alive after the work ends, so the log is still pointed at.
        let failures = self.cover_progress.failures();
        let running = self.cover_progress.in_progress();
        let cover_status = (running || failures > 0).then(|| crate::ui::CoverStatus {
            running,
            done: self.cover_progress.done(),
            total: self.cover_progress.total,
            failures,
            log_path: self.cover_progress.log_path().display().to_string(),
        });

        let now = self.started.elapsed().as_secs_f64();

        // --- draw the video into its swapchain -----------------------------
        // Whether the window got a fresh video frame this pass. The mirror is
        // double-buffered, so presenting a pass that only drew the control bar
        // would swap in a buffer with no picture behind it.
        let mut mirrored_video = false;
        let mut video_layer_data = None;
        if let Some(playing) = &mut self.playing {
            let state = self.player.state().clone();
            if state.video_width > 0 && state.video_height > 0 {
                let needs_new_target = playing
                    .target
                    .as_ref()
                    .map(|t| t.width != state.video_width || t.height != state.video_height)
                    .unwrap_or(true);
                if needs_new_target {
                    eprintln!(
                        "video swapchain {}x{}",
                        state.video_width, state.video_height
                    );
                    playing.target = Some(
                        RenderTarget::new(
                            &self.xr.session,
                            &self.glow,
                            state.video_width,
                            state.video_height,
                        )
                        .context("creating the video swapchain")?,
                    );
                }

                if let Some(target) = &mut playing.target {
                    // Only draw when mpv actually has a new frame. Skipping this
                    // leaves the last released image in place, which the
                    // compositor keeps reprojecting at full rate.
                    if self.player.needs_redraw() {
                        let fbo = target.acquire()?;
                        self.player
                            .render_to_fbo(fbo, target.width, target.height)?;

                        // Blit the left eye's half of the source frame to the
                        // desktop before releasing, since a swapchain image must
                        // not be read once handed back. This is the flat source
                        // rectangle, not a projected eye view — the compositor
                        // projects it only after we are done here. The control
                        // bar is composited over this afterwards.
                        if self.config.mirror_window {
                            let rect = target.eye_rect(playing.layout.stereo, Eye::Left);
                            let (mw, mh) = self.mirror_size;
                            unsafe {
                                self.gl.mirror_framebuffer(
                                    &self.glow,
                                    fbo,
                                    (
                                        rect.offset.x,
                                        rect.offset.y,
                                        rect.extent.width,
                                        rect.extent.height,
                                    ),
                                    (0, 0, mw, mh),
                                    true,
                                );
                            }
                        }
                            mirrored_video = true;

                        target.release()?;
                        self.player.report_swap();
                    }
                    // A layer may only name a swapchain that has had an image
                    // released. A target created this frame has none yet, so it
                    // stays out of the frame until the decoder fills it — which
                    // happens on the next frame with new video.
                    if target.is_presentable() {
                        video_layer_data = Some((playing.layout, playing.yaw, playing.pitch));
                    }
                }
            }
        }

        // --- draw the UI ---------------------------------------------------
        //
        // Drawn whenever it is wanted in *either* place. The desktop window
        // keeps the controls up permanently — hiding them there would be pure
        // friction, since the whole point of that window is operating the
        // player from the desk — while in the headset they still fade, because
        // there they sit over the video.
        let draw_ui = self.ui_visible || self.config.mirror_window;
        if draw_ui {
            let playback = self.player.state().clone();
            let target = if playing_view {
                &mut self.controls_target
            } else {
                &mut self.ui_target
            };
            let fbo = target.acquire()?;
            let actions = self.ui.run(
                &self.titles,
                &self.paths.covers_dir,
                &playback,
                pointer,
                held,
                scroll,
                &keys,
                now,
                fbo,
                surface,
                cover_status.as_ref(),
            )?;
            let target = if playing_view {
                &mut self.controls_target
            } else {
                &mut self.ui_target
            };

            // Composited over whatever the video path already drew, rather than
            // replacing it: during playback the window shows the picture with a
            // control bar along the bottom, like an ordinary player. Browsing
            // the library, there is no video, so it fills the window.
            //
            // Done before release, like the video path — a swapchain image must
            // not be read once handed back.
            if self.config.mirror_window {
                unsafe {
                    self.gl.mirror_framebuffer(
                        &self.glow,
                        fbo,
                        (0, 0, target.width as i32, target.height as i32),
                        band,
                        // The video underneath has already cleared and filled
                        // the window; clearing again would erase it.
                        !playing_view,
                    );
                }
            }

            target.release()?;

            for action in actions {
                if !self.handle(action, time)? {
                    // Still have to close the frame we opened.
                    self.xr.frame_stream.end(
                        time,
                        self.xr.environment_blend_mode,
                        &[],
                    )?;
                    return Ok(false);
                }
            }
        }

        // Show the window once everything that belongs in it has been drawn.
        //
        // Skipped on passes with no fresh video, because the window is
        // double-buffered: swapping after drawing only the control bar would
        // present a buffer with nothing behind it. Video arrives often enough
        // that the bar still tracks it closely.
        if self.config.mirror_window && (mirrored_video || self.playing.is_none()) {
            self.gl.present();
        }

        // --- submit layers -------------------------------------------------
        // Built here because each borrows the swapchains, and OpenXR wants them
        // as one slice at submission time.
        let video_layers =
            build_video_layers(&self.xr.stage, self.playing.as_ref(), video_layer_data);
        let flat_layers =
            build_flat_layers(&self.xr.stage, self.playing.as_ref(), video_layer_data);
        let ui_layer =
            build_ui_layer(
                &self.xr.stage,
                if playing_view { &self.controls_target } else { &self.ui_target },
                &panel,
                self.xr.has_cylinder,
                self.ui_visible,
            );

        // Exactly one of these is ever non-empty: a title is either projected
        // onto the sphere or shown on a screen.
        let mut layers: Vec<&xr::CompositionLayerBase<xr::OpenGL>> = Vec::new();
        for layer in &video_layers {
            layers.push(layer);
        }
        for layer in &flat_layers {
            layers.push(layer);
        }
        if let Some(layer) = &ui_layer {
            layers.push(layer);
        }

        self.xr
            .frame_stream
            .end(time, self.xr.environment_blend_mode, &layers)?;
        Ok(true)
    }

    /// Shows or hides the panel, re-anchoring it to the viewer each time it
    /// appears so it is always in front of them when summoned.
    fn set_ui_visible(&mut self, visible: bool, time: xr::Time) {
        if visible && !self.ui_visible {
            self.panel_anchor = Some(self.measure_anchor(time));
            self.controls_until = self.started.elapsed().as_secs_f64() + CONTROLS_LINGER_S;
        }
        if !visible {
            self.panel_anchor = None;
        }
        self.ui_visible = visible;
    }

    /// Applies a UI action. Returns false to quit.
    fn handle(&mut self, action: Action, time: xr::Time) -> Result<bool> {
        match action {
            Action::Quit => return Ok(false),

            Action::Play { title, part } => {
                self.save_resume_point();

                let Some(t) = self.titles.get(title) else {
                    return Ok(true);
                };
                let Some(p) = t.parts.get(part) else {
                    return Ok(true);
                };
                let media = p.best();
                eprintln!(
                    "playing {} — part {}/{} — {} ({}x{} {})",
                    t.name,
                    part + 1,
                    t.parts.len(),
                    media.file_name(),
                    media.width,
                    media.height,
                    media.codec
                );

                let layout = media.layout;
                // Queue every part, not just the chosen one, so a title that is
                // split across discs plays through to the end without going back
                // to the library between them.
                let playlist: Vec<std::path::PathBuf> =
                    t.parts.iter().map(|p| p.best().path.clone()).collect();
                // A stored resume point overrides the part the browser asked
                // for, so picking a half-watched release continues where it was
                // left rather than restarting from disc A.
                let resume = self.overrides.resume.get(&t.id).copied();
                let resume_part = self
                    .overrides
                    .resume_part
                    .get(&t.id)
                    .copied()
                    .filter(|i| *i < t.parts.len());
                let start_part = resume_part.unwrap_or(part);

                self.player.load_playlist(&playlist, start_part)?;
                if let Some(position) = resume {
                    // Only worth seeking if it is meaningfully into the file.
                    // Deferred until mpv has actually opened it, since a seek
                    // issued before then is dropped and playback would silently
                    // restart from the beginning.
                    if position > 30.0 {
                        self.player.seek_once_loaded(start_part, position);
                    }
                }

                let start_anchor = self.measure_anchor(time);
                self.playing = Some(Playing {
                    title_index: title,
                    layout,
                    // Anchor the video to where the viewer is facing, and to how
                    // far they are reclined, so a title started lying down is
                    // already oriented correctly.
                    yaw: start_anchor.yaw,
                    pitch: start_anchor.pitch,
                    target: None,
                });
                self.ui.playing = Some(title);
                self.ui.view = View::Playing;
                // Keep the controls up briefly so they are visibly there, then
                // let them fade rather than sitting over the video. Re-anchored
                // to the viewer's new heading, which playback has just set.
                self.ui_visible = false;
                self.set_ui_visible(true, time);
            }

            Action::PlayPart(index) => {
                // Only meaningful while something is loaded; the whole title is
                // already queued, so this is just a playlist position change —
                // no reload, and the decoder stays warm.
                if self.playing.is_some() {
                    self.player.set_playlist_pos(index)?;
                    self.controls_until =
                        self.started.elapsed().as_secs_f64() + CONTROLS_LINGER_S;
                }
            }

            Action::TogglePause => self.player.toggle_pause()?,
            Action::SeekTo(position) => self.player.seek_absolute(position)?,
            Action::SeekBy(delta) => self.player.seek_relative(delta)?,

            Action::StopPlayback => {
                self.save_resume_point();
                self.player.stop()?;
                self.playing = None;
                self.ui.view = View::Library;
                self.ui_visible = true;
            }

            // Correcting the title that is currently on screen.
            Action::SetLayout(layout) => {
                if let Some(index) = self.playing.as_ref().map(|p| p.title_index) {
                    self.set_layout(index, layout);
                }
            }

            // Correcting any title from the library, without playing it.
            Action::SetLayoutFor { title, layout } => self.set_layout(title, layout),
        }
        Ok(true)
    }

    /// Applies a layout correction to a title and remembers it.
    ///
    /// Corrections must outlive both the session and any future rescan, since
    /// detection would otherwise overwrite them with the same wrong guess. The
    /// swapchain does not depend on layout — only the per-eye rectangles do, and
    /// those are recomputed every frame — so a change takes effect immediately
    /// even mid-playback.
    fn set_layout(&mut self, title_index: usize, layout: Layout) {
        let Some(title) = self.titles.get_mut(title_index) else { return };
        self.overrides.layouts.insert(title.id.clone(), layout);
        for part in &mut title.parts {
            for variant in &mut part.variants {
                variant.layout = layout;
            }
        }
        if let Some(playing) = &mut self.playing {
            if playing.title_index == title_index {
                playing.layout = layout;
            }
        }
        if let Err(e) = self.overrides.save(&self.paths.overrides) {
            eprintln!("warning: could not save layout override: {e}");
        }
    }

    /// Records where playback got to, so a long title can be resumed.
    ///
    /// For a multi-part title the part matters as much as the offset: resuming a
    /// three-disc release thirty minutes into disc B is not the same as thirty
    /// minutes into disc A.
    fn save_resume_point(&mut self) {
        let Some(playing) = &self.playing else { return };
        let Some(title) = self.titles.get(playing.title_index) else { return };
        let state = self.player.state();
        let part = state.playlist_pos.max(0) as usize;

        // Ignore positions at the very start or effectively at the end — unless
        // there are later parts still to watch, in which case finishing one part
        // should resume at the next rather than forgetting the title.
        let near_end = state.duration_secs > 0.0
            && state.position_secs >= state.duration_secs - 30.0;
        let has_more_parts = part + 1 < title.parts.len();

        if state.position_secs > 30.0 && !near_end {
            self.overrides
                .resume
                .insert(title.id.clone(), state.position_secs);
            self.overrides.resume_part.insert(title.id.clone(), part);
        } else if near_end && has_more_parts {
            self.overrides.resume.insert(title.id.clone(), 0.0);
            self.overrides
                .resume_part
                .insert(title.id.clone(), part + 1);
        } else {
            self.overrides.resume.remove(&title.id);
            self.overrides.resume_part.remove(&title.id);
        }
    }
}


/// Entry point for the `play` command.
pub fn run(paths: Paths, config: Config) -> Result<()> {
    if config.roots.is_empty() {
        anyhow::bail!("no library roots configured — run: vrmp add-root <DIR>");
    }
    let mut app = App::new(paths, config)?;
    app.run()
}

/// One equirect layer per eye, both reading from the same swapchain image.
///
/// Free-standing rather than a method so that it borrows only the reference
/// space and the video swapchain. As a method it would borrow all of `App`,
/// which would conflict with borrowing the frame stream mutably to submit.
///
/// Stereo needs no extra work here: a side-by-side frame is one image, and the
/// eyes differ only by `eye_visibility` and the half of it each one reads.
fn build_video_layers<'a>(
    stage: &'a xr::Space,
    playing: Option<&'a Playing>,
    data: Option<(Layout, f32, f32)>,
) -> Vec<xr::CompositionLayerEquirect2KHR<'a, xr::OpenGL>> {
    let Some((layout, yaw, pitch)) = data else {
        return Vec::new();
    };
    let Some(playing) = playing else {
        return Vec::new();
    };
    let Some(target) = &playing.target else {
        return Vec::new();
    };
    let Some(geometry) = EquirectGeometry::for_projection(layout.projection) else {
        // Flat and fisheye content cannot be described as an equirect layer;
        // both are left for a future quad/mesh path.
        return Vec::new();
    };

    let pose = crate::xr::pose_facing(yaw, pitch, xr::Vector3f { x: 0.0, y: EYE_HEIGHT_M, z: 0.0 });

    [Eye::Left, Eye::Right]
        .into_iter()
        .map(|eye| {
            xr::CompositionLayerEquirect2KHR::new()
                .space(stage)
                .eye_visibility(eye.visibility())
                // Radius zero means an infinite sphere, which is what video
                // wants: the image must not shift as the viewer moves.
                .radius(0.0)
                .central_horizontal_angle(geometry.central_horizontal_angle)
                .upper_vertical_angle(geometry.upper_vertical_angle)
                .lower_vertical_angle(geometry.lower_vertical_angle)
                .pose(pose)
                .sub_image(
                    xr::SwapchainSubImage::new()
                        .swapchain(&target.swapchain)
                        .image_array_index(0)
                        .image_rect(target.eye_rect(layout.stereo, eye)),
                )
        })
        .collect()
}

/// Flat video shown on a quad floating in front of the viewer.
///
/// The compositor cannot describe flat content as an equirect layer, so ordinary
/// 2D video gets a quad instead — a virtual screen. Stereo is handled the same
/// way as for equirect: one quad per eye over halves of the same image, or a
/// single quad visible to both eyes for mono.
fn build_flat_layers<'a>(
    stage: &'a xr::Space,
    playing: Option<&'a Playing>,
    data: Option<(Layout, f32, f32)>,
) -> Vec<xr::CompositionLayerQuad<'a, xr::OpenGL>> {
    let Some((layout, yaw, pitch)) = data else {
        return Vec::new();
    };
    if !matches!(layout.projection, Projection::Flat) {
        return Vec::new();
    }
    let Some(playing) = playing else {
        return Vec::new();
    };
    let Some(target) = &playing.target else {
        return Vec::new();
    };

    // Place the screen a comfortable distance ahead, sized to keep the source
    // aspect ratio. Width is in metres; at this distance it subtends roughly
    // the same angle as a large television.
    const SCREEN_DISTANCE_M: f32 = 3.0;
    const SCREEN_WIDTH_M: f32 = 3.2;

    let (eye_w, eye_h) = match layout.stereo {
        Stereo::Mono => (target.width as f32, target.height as f32),
        Stereo::SideBySide => (target.width as f32 / 2.0, target.height as f32),
        Stereo::TopBottom => (target.width as f32, target.height as f32 / 2.0),
    };
    let aspect = if eye_h > 0.0 { eye_w / eye_h } else { 16.0 / 9.0 };
    let size = xr::Extent2Df {
        width: SCREEN_WIDTH_M,
        height: SCREEN_WIDTH_M / aspect,
    };

    // Put the screen along the direction the viewer was facing when playback
    // started — including how far back they were reclined, so a flat title
    // started lying down appears overhead rather than at their feet.
    let forward = glam::Quat::from_rotation_y(yaw) * glam::Quat::from_rotation_x(pitch)
        * glam::Vec3::NEG_Z;
    let centre = glam::Vec3::new(0.0, EYE_HEIGHT_M, 0.0) + forward * SCREEN_DISTANCE_M;
    let pose = crate::xr::pose_facing(
        yaw,
        pitch,
        xr::Vector3f { x: centre.x, y: centre.y, z: centre.z },
    );

    let eyes: &[Eye] = match layout.stereo {
        Stereo::Mono => &[Eye::Left],
        _ => &[Eye::Left, Eye::Right],
    };

    eyes.iter()
        .map(|&eye| {
            let visibility = match layout.stereo {
                Stereo::Mono => xr::EyeVisibility::BOTH,
                _ => eye.visibility(),
            };
            xr::CompositionLayerQuad::new()
                .space(stage)
                .eye_visibility(visibility)
                .pose(pose)
                .size(size)
                .sub_image(
                    xr::SwapchainSubImage::new()
                        .swapchain(&target.swapchain)
                        .image_array_index(0)
                        .image_rect(target.eye_rect(layout.stereo, eye)),
                )
        })
        .collect()
}

/// The curved panel carrying the interface, blended over the video.
/// The whole of `ui_target` is shown. Playback and the library pass different
/// targets, each already sized to its own content, so this never has to address
/// part of a differently shaped image.
/// The interface panel, curved where the runtime can manage it and flat where
/// it cannot.
///
/// Both variants deref to the same layer base, so submission does not care
/// which one it got.
enum UiLayer<'a> {
    Curved(xr::CompositionLayerCylinderKHR<'a, xr::OpenGL>),
    Flat(xr::CompositionLayerQuad<'a, xr::OpenGL>),
}

impl<'a> std::ops::Deref for UiLayer<'a> {
    type Target = xr::CompositionLayerBase<'a, xr::OpenGL>;

    fn deref(&self) -> &Self::Target {
        match self {
            UiLayer::Curved(layer) => layer,
            UiLayer::Flat(layer) => layer,
        }
    }
}

fn build_ui_layer<'a>(
    stage: &'a xr::Space,
    ui_target: &'a RenderTarget,
    panel: &CylinderPanel,
    curved: bool,
    visible: bool,
) -> Option<UiLayer<'a>> {
    if !visible {
        return None;
    }

    let sub_image = xr::SwapchainSubImage::new()
        .swapchain(&ui_target.swapchain)
        .image_array_index(0)
        .image_rect(xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: ui_target.width as i32,
                height: ui_target.height as i32,
            },
        });

    // egui leaves most of the surface transparent, and the video must show
    // through it.
    let flags = xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;

    if curved {
        return Some(UiLayer::Curved(
            xr::CompositionLayerCylinderKHR::new()
                .space(stage)
                .layer_flags(flags)
                .eye_visibility(xr::EyeVisibility::BOTH)
                .pose(panel.pose)
                .radius(panel.radius)
                .central_angle(panel.central_angle)
                .aspect_ratio(panel.aspect_ratio)
                .sub_image(sub_image),
        ));
    }

    // Without the cylinder extension, place a flat quad across the same arc.
    // Its width is the chord rather than the arc length, so the panel subtends
    // the angle it was asked for instead of appearing wider than intended.
    let width = 2.0 * panel.radius * (panel.central_angle * 0.5).sin();
    let height = width / panel.aspect_ratio;
    let forward = glam::Quat::from_xyzw(
        panel.pose.orientation.x,
        panel.pose.orientation.y,
        panel.pose.orientation.z,
        panel.pose.orientation.w,
    ) * glam::Vec3::NEG_Z;
    let centre = glam::Vec3::new(
        panel.pose.position.x,
        panel.pose.position.y,
        panel.pose.position.z,
    ) + forward * panel.radius;

    Some(UiLayer::Flat(
        xr::CompositionLayerQuad::new()
            .space(stage)
            .layer_flags(flags)
            .eye_visibility(xr::EyeVisibility::BOTH)
            .pose(xr::Posef {
                orientation: panel.pose.orientation,
                position: xr::Vector3f { x: centre.x, y: centre.y, z: centre.z },
            })
            .size(xr::Extent2Df { width, height })
            .sub_image(sub_image),
    ))
}

impl App {
    /// Measures where the panel should sit: the viewer's current heading, and
    /// their actual eye height.
    ///
    /// Eye height is measured rather than assumed, because a panel centred on a
    /// hard-coded 1.6 m sits noticeably high or low for anyone who is not that
    /// tall, or who is sitting down.
    fn measure_anchor(&self, time: xr::Time) -> Anchor {
        let (yaw, pitch) = self.xr.view_angles(time).unwrap_or((0.0, 0.0));
        let head = self
            .xr
            .view_space
            .locate(&self.xr.stage, time)
            .ok()
            .map(|l| l.pose.position)
            // Reject a height that cannot be real: tracking may not have
            // settled, and anchoring to a bogus position would put the panel
            // somewhere unreachable. Lying down is a legitimate low height, so
            // the floor of this range is generous.
            .filter(|p| p.y > 0.1 && p.y < 2.5)
            .unwrap_or(xr::Vector3f { x: 0.0, y: EYE_HEIGHT_M, z: 0.0 });
        Anchor {
            yaw,
            pitch: crate::xr::significant_pitch(pitch),
            head,
        }
    }
}

impl App {
    /// Where in the window the interface is drawn.
    ///
    /// During playback it is a bar across the bottom, over the video. Elsewhere
    /// it takes the whole window, since there is nothing behind it.
    fn mirror_band(&self, playing_view: bool) -> (i32, i32, i32, i32) {
        let (w, h) = self.mirror_size;
        if !playing_view {
            return (0, 0, w, h);
        }
        // As tall as the control surface's own aspect wants at full width, so
        // the bar is not stretched, and capped so it cannot swallow the picture
        // on an unusually narrow window.
        let aspect = UI_CONTROLS_WIDTH as f32 / UI_CONTROLS_HEIGHT as f32;
        let band_h = ((w as f32 / aspect).round() as i32).min(h / 2);
        // GL's origin is bottom-left, so the bottom of the window is y = 0.
        (0, 0, w, band_h)
    }
}
