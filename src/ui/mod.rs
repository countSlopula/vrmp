//! The in-headset interface: a library browser and playback controls.
//!
//! The UI is an egui surface rendered into a texture, which is then shown as an
//! OpenXR cylinder layer curved around the viewer. Pointing with a controller
//! casts a ray at that cylinder, and the hit point becomes a mouse position, so
//! the whole interface is ordinary 2D egui code driven by a 3D pointer.

pub mod pointer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use egui::{Color32, RichText, TextureHandle};

use crate::library::{Confidence, Layout, Projection, Stereo, Title};
use crate::mpv::PlaybackState;

/// Pixel size of the UI surface. Wide because the library is a grid and the
/// cylinder wraps a long way around the viewer; tall enough that body text is
/// comfortably legible at a couple of metres.
pub const UI_WIDTH: u32 = 2048;
pub const UI_HEIGHT: u32 = 1152;

/// Size of the surface the playback controls are drawn into.
///
/// The controls get their own swapchain at this size rather than a strip of the
/// library's, so the transport bar can be short and wide without inheriting the
/// tall panel shape the cover grid needs.
///
/// A dedicated swapchain, rather than an `imageRect` selecting part of the
/// larger one, specifically because the origin convention for a sub-rect is
/// ambiguous here: OpenGL textures start at the bottom-left while OpenXR rects
/// are specified from the top-left, so a strip selected by rect can silently
/// address the opposite end of the image from where egui drew. Sizing the
/// swapchain to the content sidesteps the question.
pub const UI_CONTROLS_WIDTH: u32 = 2048;
pub const UI_CONTROLS_HEIGHT: u32 = 420;

/// How many cover textures to keep resident. Each is roughly 0.9 MB on the GPU,
/// so this bounds cover memory at a few hundred megabytes while comfortably
/// covering everything on screen plus a margin for scrolling.
const COVER_BUDGET: usize = 256;

/// Width divided by height of a cover tile.
///
/// Chosen to match the portrait shape cover art is usually distributed in — 588
/// by 800 and its multiples are common — so the grid fits real artwork rather
/// than imposing a shape it would have to be letterboxed into.
pub const COVER_ASPECT: f32 = 0.735;

/// What background cover generation is doing, for reporting to the viewer.
///
/// Carries the log location rather than just a count: a failure the viewer
/// cannot investigate is only slightly better than a silent one.
pub struct CoverStatus {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub failures: usize,
    pub log_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Library,
    Playing,
}

/// Something the app loop should do as a result of interaction.
#[derive(Debug, Clone)]
pub enum Action {
    Play { title: usize, part: usize },
    TogglePause,
    SeekTo(f64),
    SeekBy(f64),
    /// Correct the detected layout of the playing title, and remember it.
    SetLayout(Layout),
    /// Correct the layout of any title from the library, without playing it.
    SetLayoutFor { title: usize, layout: Layout },
    /// Jump to another part of the title already playing.
    PlayPart(usize),
    StopPlayback,
    Quit,
}

pub struct Ui {
    ctx: egui::Context,
    painter: egui_glow::Painter,
    covers: CoverCache,
    pub view: View,
    search: String,
    collection: Option<String>,
    vr_only: bool,
    /// Whether the search field has been given focus once at startup.
    search_focused_once: bool,
    /// Title id whose format selector is currently open, if any.
    editing_format: Option<String>,
    /// Index into the filtered list, for the currently playing title.
    pub playing: Option<usize>,
    /// Keeps the controls visible for a moment after interaction, then fades
    /// them so they do not sit over the video permanently.
    controls_shown_until: f64,
    /// Whether egui currently believes the primary button is down.
    pointer_down: bool,
    /// Last position the ray hit, reused to keep a drag alive off-panel.
    last_pointer: Option<egui::Pos2>,
}

impl Ui {
    pub fn new(gl: &std::sync::Arc<glow::Context>) -> Result<Self> {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);

        // A dark, low-contrast theme: this interface floats in front of video,
        // often in an otherwise black field, and a bright panel is unpleasant.
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgba_unmultiplied(12, 12, 16, 235);
        visuals.window_fill = visuals.panel_fill;
        ctx.set_visuals(visuals);

        // Everything is read from a couple of metres away through a headset's
        // limited angular resolution, so scale the whole type ramp up.
        ctx.all_styles_mut(|style| {
            for font in style.text_styles.values_mut() {
                font.size *= 1.8;
            }
        });

        // egui's click detection is tuned for a mouse, and both defaults are
        // hostile to a controller ray. A press only counts as a click if the
        // pointer moved less than 6px and was held under 0.8s — but a hand at
        // two metres shakes by well over 6px on a 2048px panel, so real clicks
        // were being reclassified as drags and silently dropped.
        ctx.options_mut(|options| {
            // Generous enough to absorb tremor, still far below a deliberate
            // scrub, so dragging the seek bar is unaffected.
            options.input_options.max_click_dist = 32.0;
            // Trigger pulls in VR are slower and less crisp than mouse clicks.
            options.input_options.max_click_duration = 2.5;
        });

        let painter = egui_glow::Painter::new(gl.clone(), "", None, false)
            .map_err(|e| anyhow::anyhow!("creating egui painter: {e}"))?;

        Ok(Ui {
            ctx,
            painter,
            covers: CoverCache::default(),
            view: View::Library,
            search: String::new(),
            collection: None,
            vr_only: false,
            search_focused_once: false,
            editing_format: None,
            playing: None,
            controls_shown_until: 0.0,
            pointer_down: false,
            last_pointer: None,
        })
    }

    /// Builds and renders one frame of UI into `fbo`, returning what the viewer
    /// asked for.
    ///
    /// `held` is whether the trigger is down *right now*, not whether it was
    /// just pressed: egui needs a sustained button state across frames to
    /// recognise a drag, which is what makes the seek bar scrubbable.
    ///
    /// `keys` carries text input from X11, since OpenXR has no keyboard of its
    /// own; see [`crate::xr::desktop`].
    ///
    /// `surface` is the pixel size of the target being drawn into: playback and
    /// the library use differently shaped swapchains.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        titles: &[Title],
        covers_dir: &Path,
        playback: &PlaybackState,
        pointer: Option<egui::Pos2>,
        held: bool,
        scroll: f32,
        keys: &[crate::xr::desktop::KeyInput],
        now: f64,
        fbo: u32,
        surface: (u32, u32),
        cover_status: Option<&CoverStatus>,
    ) -> Result<Vec<Action>> {
        let mut actions = Vec::new();

        if held || scroll.abs() > 0.01 || pointer.is_some() {
            self.controls_shown_until = now + 5.0;
        }

        // While the trigger is held, keep using the last position the ray hit.
        // A drag that wanders off the edge of the panel should continue rather
        // than freezing, which is exactly what happens when scrubbing a seek bar
        // near the bottom of the panel.
        let position = match (pointer, held) {
            (Some(raw), _) => {
                // Smooth the ray, which attacks the jitter itself rather than
                // only widening the tolerance for it. A held controller is never
                // still, and at this distance small tremors are large pixel
                // movements. Converging halfway per frame settles within a frame
                // or two at headset refresh rates, so it steadies the cursor
                // without perceptible lag.
                const SMOOTHING: f32 = 0.5;
                let smoothed = match self.last_pointer {
                    Some(prev) => prev + (raw - prev) * SMOOTHING,
                    None => raw,
                };
                self.last_pointer = Some(smoothed);
                Some(smoothed)
            }
            // A drag already under way may wander off the panel — keep feeding
            // it the last known point so scrubbing near an edge does not stall.
            //
            // Crucially this only continues an *existing* drag. Reviving the
            // last position for a press that starts off-panel would fire
            // whichever widget the ray last touched, so aiming away and pulling
            // the trigger would activate a button that is nowhere near where you
            // are pointing.
            (None, true) if self.pointer_down => self.last_pointer,
            _ => None,
        };

        let mut events = Vec::new();
        if let Some(pos) = position {
            events.push(egui::Event::PointerMoved(pos));

            // Emit the button as edges of a held state rather than a press and
            // release in the same frame. Sending both at once is a click and can
            // never become a drag, which is why sliders could not be scrubbed.
            if held != self.pointer_down {
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: held,
                    modifiers: Default::default(),
                });
            }
        } else {
            if self.pointer_down {
                // Lost the panel mid-drag: release so nothing is left stuck down.
                if let Some(pos) = self.last_pointer {
                    events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    });
                }
            }
            // Tell egui the pointer has left. Without this it keeps the last
            // position indefinitely, so whatever the ray last touched stays
            // highlighted long after it has been aimed somewhere else.
            events.push(egui::Event::PointerGone);
            self.last_pointer = None;
        }
        self.pointer_down = held && position.is_some();
        if scroll.abs() > 0.01 {
            events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::Vec2::new(0.0, scroll),
                phase: egui::TouchPhase::Move,
                modifiers: Default::default(),
            });
        }

        for key in keys {
            events.push(key.to_egui());
        }

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(surface.0 as f32, surface.1 as f32),
            )),
            time: Some(now),
            events,
            ..Default::default()
        };

        let view = self.view;
        let full_output = self.ctx.clone().run_ui(raw_input, |ui| {
            match view {
                View::Library => {
                    self.library_view(
                        ui, titles, covers_dir, &mut actions, now, cover_status,
                    )
                }
                View::Playing => self.player_view(ui, playback, titles, &mut actions),
            }
            // The controller ray has no geometry in the scene, so the cursor is
            // what tells the viewer where they are aiming. Drawn on a foreground
            // layer so nothing in the interface can cover it.
            if let Some(pos) = pointer {
                draw_cursor(ui.ctx(), pos, held);
            }
        });

        // Draw into the swapchain image backing `fbo`.
        let mut full_output = full_output;
        let clipped = self
            .ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        unsafe {
            use glow::HasContext as _;
            let gl = self.painter.gl();
            gl.bind_framebuffer(
                glow::FRAMEBUFFER,
                std::num::NonZeroU32::new(fbo).map(glow::NativeFramebuffer),
            );
            gl.viewport(0, 0, surface.0 as i32, surface.1 as i32);
            gl.disable(glow::SCISSOR_TEST);
            // Fully transparent: the compositor blends the panel over the video,
            // so anything egui does not paint should show the scene through.
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        self.painter.paint_and_update_textures(
            [surface.0, surface.1],
            full_output.pixels_per_point,
            &clipped,
            &mut full_output.textures_delta,
        );

        Ok(actions)
    }

    /// Titles passing the current search and filters, with their real indices.
    fn filtered<'a>(&self, titles: &'a [Title]) -> Vec<(usize, &'a Title)> {
        let needle = self.search.to_ascii_lowercase();
        titles
            .iter()
            .enumerate()
            .filter(|(_, t)| !self.vr_only || t.is_vr())
            .filter(|(_, t)| {
                self.collection
                    .as_ref()
                    .map(|c| &t.collection == c)
                    .unwrap_or(true)
            })
            .filter(|(_, t)| needle.is_empty() || t.name.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    fn library_view(
        &mut self,
        root: &mut egui::Ui,
        titles: &[Title],
        covers_dir: &Path,
        actions: &mut Vec<Action>,
        now: f64,
        cover_status: Option<&CoverStatus>,
    ) {
        let collections: Vec<String> = {
            let mut c: Vec<String> = titles.iter().map(|t| t.collection.clone()).collect();
            c.sort();
            c.dedup();
            c
        };

        egui::Panel::top("top").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Library");
                ui.separator();
                // Typed into from a real keyboard. OpenXR carries no text input,
                // so keystrokes arrive over X11 instead — see `xr::desktop`.
                // Any keyboard that produces ordinary system key events works,
                // including a VR overlay one.
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("search")
                        .desired_width(420.0),
                );
                // Focus it by default so typing goes somewhere sensible without
                // first having to hit a small target with the ray.
                if !field.has_focus() && !self.search_focused_once {
                    field.request_focus();
                    self.search_focused_once = true;
                }
                if !self.search.is_empty() && ui.button("clear").clicked() {
                    self.search.clear();
                }
                ui.separator();
                ui.checkbox(&mut self.vr_only, "VR only");

                // Playback keeps running while browsing, so there has to be a
                // way back to it. Without this, leaving the player stranded the
                // viewer in the library with a title still playing behind them
                // and no route to its controls.
                if let Some(index) = self.playing {
                    ui.separator();
                    let name = titles
                        .get(index)
                        .map(|t| truncate(&t.name, 24))
                        .unwrap_or_else(|| "current title".to_string());
                    if ui
                        .button(RichText::new(format!("▶ {name}")).strong())
                        .on_hover_text("Back to what is playing")
                        .clicked()
                    {
                        self.view = View::Playing;
                    }
                }

                ui.separator();
                if ui.button("Quit").clicked() {
                    actions.push(Action::Quit);
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .selectable_label(self.collection.is_none(), "All")
                    .clicked()
                {
                    self.collection = None;
                }
                for c in &collections {
                    if ui
                        .selectable_label(self.collection.as_ref() == Some(c), c)
                        .clicked()
                    {
                        self.collection = Some(c.clone());
                    }
                }
            });

        });

        let shown = self.filtered(titles);

        egui::CentralPanel::default().show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} titles", shown.len()))
                        .color(Color32::from_gray(140)),
                );

                // Say plainly that work is happening and what it may cost,
                // rather than having the worker quietly back off when something
                // plays. Adapting would make its speed depend on state the
                // viewer cannot see, leaving "why is this slow?" unanswerable.
                if let Some(status) = cover_status {
                    if status.running {
                        ui.separator();
                        ui.label(
                            RichText::new(format!(
                                "Generating covers {}/{} — playback may stutter",
                                status.done, status.total
                            ))
                            .color(Color32::from_rgb(220, 180, 110)),
                        );
                    }
                }
            });

            // Failures stay on screen after generation ends, naming the log.
            // A tile with no art is otherwise indistinguishable from one still
            // waiting its turn, and a message that vanishes with the work is no
            // use to someone who was in a video while it scrolled past.
            if let Some(status) = cover_status {
                if status.failures > 0 {
                    ui.label(
                        RichText::new(format!(
                            "⚠ {} cover{} could not be generated — see {}",
                            status.failures,
                            if status.failures == 1 { "" } else { "s" },
                            status.log_path,
                        ))
                        .size(19.0)
                        .color(Color32::from_rgb(230, 150, 120)),
                    );
                }
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Fit whole tiles to the available width rather than using a
                // fixed size, so the grid reaches the right edge instead of
                // leaving a ragged gap there.
                const SPACING: f32 = 16.0;
                const TARGET_W: f32 = 250.0;

                let available = ui.available_width();
                let per_row = ((available / (TARGET_W + SPACING)).floor() as usize).max(1);
                let cover_w =
                    ((available - SPACING * per_row as f32) / per_row as f32).max(120.0);

                for chunk in shown.chunks(per_row) {
                    ui.horizontal(|ui| {
                        for (index, title) in chunk {
                            self.cover_button(
                                ui, *index, title, covers_dir, cover_w, actions, now,
                            );
                        }
                    });
                    ui.add_space(12.0);
                }
            });
        });
    }


    /// One cover tile: art, name, and a badge describing what it is.
    #[allow(clippy::too_many_arguments)]
    fn cover_button(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        title: &Title,
        covers_dir: &Path,
        width: f32,
        actions: &mut Vec<Action>,
        now: f64,
    ) {
        // Tiles are portrait, matching the shape of real cover art. Generated
        // frame grabs are cropped to the same ratio, so a library mixing the two
        // still lays out as an even grid.
        //
        // `maintain_aspect_ratio` letterboxes anything that does not match,
        // rather than stretching it — a wrong-shaped cover looks unloved, but a
        // distorted one looks broken.
        let art = egui::vec2(width, width / COVER_ASPECT);
        ui.allocate_ui(egui::vec2(width, width / COVER_ASPECT + 130.0), |ui| {
            ui.vertical(|ui| {
                let texture = self.covers.get(&self.ctx, covers_dir, title, now);
                let response = match texture {
                    Some(tex) => ui.add(
                        egui::Image::new(&tex)
                            .fit_to_exact_size(art)
                            .maintain_aspect_ratio(true)
                            .corner_radius(8.0)
                            .sense(egui::Sense::click()),
                    ),
                    None => ui.add_sized(
                        art,
                        egui::Button::new(RichText::new("no cover").size(18.0)),
                    ),
                };

                // Lift the tile under the pointer so the ray has visible
                // feedback; at arm's length a cursor alone is hard to track.
                if response.hovered() {
                    ui.painter().rect_stroke(
                        response.rect,
                        8.0,
                        egui::Stroke::new(2.5, Color32::from_rgb(130, 180, 240)),
                        egui::StrokeKind::Inside,
                    );
                }

                ui.label(RichText::new(truncate(&title.name, 40)).strong());

                // Resolution and length, the two things that decide whether a
                // title is worth opening.
                ui.label(
                    RichText::new(format!(
                        "{} · {}",
                        quality_label(title),
                        clock(title.total_duration_secs())
                    ))
                    .size(17.0)
                    .color(Color32::from_gray(130)),
                );

                // The format badge doubles as the control for correcting it.
                // Detection is heuristic, so being able to fix a title without
                // first launching it matters — especially for the ones marked
                // "guessed".
                let editing = self.editing_format.as_deref() == Some(title.id.as_str());
                let badge = ui.add(
                    egui::Button::new(
                        RichText::new(describe(title))
                            .size(18.0)
                            .color(if editing {
                                Color32::from_rgb(150, 190, 240)
                            } else {
                                Color32::from_gray(150)
                            }),
                    )
                    .frame(editing),
                );
                if badge.clicked() {
                    self.editing_format = if editing {
                        None
                    } else {
                        Some(title.id.clone())
                    };
                }

                if editing {
                    let current = title.layout();
                    ui.horizontal_wrapped(|ui| {
                        for (label, layout) in layout_choices() {
                            let selected = layout.projection == current.projection
                                && layout.stereo == current.stereo;
                            if ui
                                .add(
                                    egui::Button::selectable(
                                        selected,
                                        RichText::new(label).size(17.0),
                                    ),
                                )
                                .clicked()
                            {
                                actions.push(Action::SetLayoutFor {
                                    title: index,
                                    layout,
                                });
                                self.editing_format = None;
                            }
                        }
                    });
                }

                // Clicking the cover plays; clicking the badge does not, so a
                // format correction never launches the title by accident.
                if response.clicked() {
                    self.editing_format = None;
                    actions.push(Action::Play {
                        title: index,
                        part: title.default_part_index(),
                    });
                }
            });
        });
    }

    fn player_view(
        &mut self,
        root: &mut egui::Ui,
        playback: &PlaybackState,
        titles: &[Title],
        actions: &mut Vec<Action>,
    ) {
        let title = self.playing.and_then(|i| titles.get(i));

        egui::CentralPanel::default()

            .show(root, |ui| {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("◀ Library").clicked() {
                        self.view = View::Library;
                    }
                    ui.separator();
                    if let Some(t) = title {
                        ui.label(RichText::new(truncate(&t.name, 48)).strong());
                    }
                });

                // Part selector. Most titles here are split across several
                // files, so jumping between them has to be one click rather
                // than a trip back to the library.
                if let Some(t) = title {
                    if t.parts.len() > 1 {
                        let current = playback.playlist_pos.max(0) as usize;
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!("Part {}/{}", current + 1, t.parts.len()))
                                    .color(Color32::from_gray(150)),
                            );
                            for (i, part) in t.parts.iter().enumerate() {
                                // Disc letters where the release has them,
                                // otherwise a plain number.
                                let label = if part.label.is_empty() {
                                    format!("{}", i + 1)
                                } else {
                                    part.label.to_uppercase()
                                };
                                if ui
                                    .add(egui::Button::selectable(
                                        i == current,
                                        RichText::new(truncate(&label, 14)),
                                    ))
                                    .clicked()
                                {
                                    actions.push(Action::PlayPart(i));
                                }
                            }
                        });
                    }
                }

                ui.add_space(8.0);

                // Scrub bar, spanning the full width of the panel.
                //
                // Aiming a controller at two metres is far less precise than a
                // mouse, so the target has to be generous: a thin default-width
                // slider is close to unusable in a headset.
                let duration = playback.duration_secs.max(0.001);
                let mut position = playback.position_secs.clamp(0.0, duration);

                let slider = ui.scope(|ui| {
                    let full_width = ui.available_width();
                    let style = ui.style_mut();
                    style.spacing.slider_width = full_width;
                    // A tall rail and a broad handle, both sized for a shaky ray
                    // rather than a mouse cursor.
                    style.spacing.slider_rail_height = 22.0;
                    style.spacing.interact_size.y = 48.0;
                    ui.add(
                        egui::Slider::new(&mut position, 0.0..=duration)
                            .show_value(false)
                            .handle_shape(egui::style::HandleShape::Rect { aspect_ratio: 0.5 }),
                    )
                })
                .inner;

                // Seek once when the drag finishes, rather than on every frame
                // of it: scrubbing a network-backed 8K file with a seek per
                // frame would thrash the decoder and the NAS. A plain click on
                // the rail still seeks immediately, since that is not a drag.
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    actions.push(Action::SeekTo(position));
                }

                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} / {}",
                        clock(playback.position_secs),
                        clock(playback.duration_secs)
                    ));
                    ui.separator();
                    if ui.button("−30s").clicked() {
                        actions.push(Action::SeekBy(-30.0));
                    }
                    if ui.button("−5s").clicked() {
                        actions.push(Action::SeekBy(-5.0));
                    }
                    if ui
                        .button(if playback.paused { "▶ Play" } else { "⏸ Pause" })
                        .clicked()
                    {
                        actions.push(Action::TogglePause);
                    }
                    if ui.button("+5s").clicked() {
                        actions.push(Action::SeekBy(5.0));
                    }
                    if ui.button("+30s").clicked() {
                        actions.push(Action::SeekBy(30.0));
                    }
                    ui.separator();
                    if ui.button("Stop").clicked() {
                        actions.push(Action::StopPlayback);
                    }
                });

                ui.add_space(8.0);

                // Layout correction. Detection is heuristic, and being able to
                // fix a wrong guess without leaving the headset matters more
                // than any amount of extra guessing.
                if let Some(t) = title {
                    let current = t.layout();

                    // Fisheye needs a mesh the compositor cannot describe, so
                    // nothing will be drawn. Say so, rather than leaving the
                    // viewer staring at an empty void wondering what broke.
                    if matches!(current.projection, Projection::Fisheye { .. }) {
                        ui.label(
                            RichText::new(
                                "Fisheye projection is not supported yet — \
                                 choose another format below to view this title.",
                            )
                            .size(19.0)
                            .color(Color32::from_rgb(230, 170, 90)),
                        );
                    }

                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Projection")
                                .color(Color32::from_gray(150)),
                        );
                        for (label, layout) in layout_choices() {
                            let selected = layout.projection == current.projection
                                && layout.stereo == current.stereo;
                            if ui.selectable_label(selected, label).clicked() {
                                actions.push(Action::SetLayout(layout));
                            }
                        }
                    });
                }

                // Buffer health, which is the thing worth seeing when the
                // library lives on a network share.
                ui.horizontal(|ui| {
                    let buffered = playback.cache_secs;
                    let color = if buffered < 3.0 {
                        Color32::from_rgb(220, 120, 90)
                    } else {
                        Color32::from_gray(130)
                    };
                    ui.label(
                        RichText::new(format!("buffered {buffered:.0}s"))
                            .size(18.0)
                            .color(color),
                    );
                    if playback.dropped_frames > 0 {
                        ui.label(
                            RichText::new(format!("dropped {}", playback.dropped_frames))
                                .size(18.0)
                                .color(Color32::from_rgb(220, 120, 90)),
                        );
                    }
                    ui.label(
                        RichText::new(format!(
                            "{}x{}",
                            playback.video_width, playback.video_height
                        ))
                        .size(18.0)
                        .color(Color32::from_gray(130)),
                    );
                });
                ui.add_space(12.0);
            });
    }
}

/// The projection presets offered for manual correction.
fn layout_choices() -> Vec<(&'static str, Layout)> {
    let c = Confidence::Confirmed;
    vec![
        (
            "180 SBS",
            Layout { projection: Projection::Equirect { degrees: 180 }, stereo: Stereo::SideBySide, confidence: c },
        ),
        (
            "180 TB",
            Layout { projection: Projection::Equirect { degrees: 180 }, stereo: Stereo::TopBottom, confidence: c },
        ),
        (
            "180 mono",
            Layout { projection: Projection::Equirect { degrees: 180 }, stereo: Stereo::Mono, confidence: c },
        ),
        (
            "360 SBS",
            Layout { projection: Projection::Equirect { degrees: 360 }, stereo: Stereo::SideBySide, confidence: c },
        ),
        (
            "360 mono",
            Layout { projection: Projection::Equirect { degrees: 360 }, stereo: Stereo::Mono, confidence: c },
        ),
        ("Flat", Layout { projection: Projection::Flat, stereo: Stereo::Mono, confidence: c }),
    ]
}

/// The format badge under a cover, which doubles as the selector's label.
fn describe(title: &Title) -> String {
    let layout = title.layout();
    let kind = match layout.projection {
        Projection::Flat => "flat".to_string(),
        Projection::Equirect { degrees } => format!("{degrees}°"),
        Projection::Fisheye { degrees } => format!("fisheye {degrees}°"),
    };
    let stereo = match layout.stereo {
        Stereo::Mono => "mono",
        Stereo::SideBySide => "SBS",
        Stereo::TopBottom => "TB",
    };
    let mut s = format!("{kind} {stereo}");
    if title.parts.len() > 1 {
        s.push_str(&format!(" · {} parts", title.parts.len()));
    }
    // Flagged because a guess is what the selector exists to correct.
    if layout.confidence == Confidence::Guessed {
        s.push_str(" · guessed?");
    }
    s
}

/// Marketing-style resolution label, based on the size of a single eye.
///
/// Per-eye is the honest measure: a 4320x2160 side-by-side file is "4K" per eye,
/// not 8K, and quoting the packed width would overstate every stereo title.
fn quality_label(title: &Title) -> String {
    let Some(part) = title.parts.first() else {
        return "—".to_string();
    };
    let media = part.best();
    let (w, h) = media.per_eye_size();
    if w == 0 || h == 0 {
        return "—".to_string();
    }
    let name = match w {
        0..=1499 => "HD",
        1500..=2299 => "2K",
        2300..=3299 => "4K",
        3300..=4499 => "5K",
        4500..=6499 => "6K",
        _ => "8K",
    };
    format!("{name} {w}×{h}")
}

fn clock(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "--:--".into();
    }
    let total = secs as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Lazily loads cover images onto the GPU, bounded so a large library cannot
/// exhaust video memory.
#[derive(Default)]
struct CoverCache {
    textures: HashMap<String, TextureHandle>,
    /// When a title's cover last failed to load, so we neither retry every
    /// frame nor give up forever. Covers are generated in the background, so a
    /// file that is missing now may well exist a second from now.
    failed: HashMap<String, f64>,
    /// Insertion order, used to evict the oldest when over budget.
    order: Vec<String>,
}

/// How long to wait before looking again for a cover that was not there.
///
/// Long enough not to hammer the filesystem while scrolling past dozens of
/// unfinished tiles; short enough that art appears while the viewer is still
/// looking at the same screen.
const COVER_RETRY_S: f64 = 2.0;

impl CoverCache {
    fn get(
        &mut self,
        ctx: &egui::Context,
        covers_dir: &Path,
        title: &Title,
        now: f64,
    ) -> Option<TextureHandle> {
        if let Some(tex) = self.textures.get(&title.id) {
            return Some(tex.clone());
        }
        // Back off after a miss, but keep trying: the cover may still be being
        // generated in the background.
        if let Some(last) = self.failed.get(&title.id) {
            if now - last < COVER_RETRY_S {
                return None;
            }
        }

        let path = title
            .cover
            .clone()
            .unwrap_or_else(|| crate::library::thumbs::cover_path(covers_dir, &title.id));

        match load_image(&path) {
            Ok(image) => {
                let tex = ctx.load_texture(&title.id, image, egui::TextureOptions::LINEAR);
                self.textures.insert(title.id.clone(), tex.clone());
                self.order.push(title.id.clone());
                self.failed.remove(&title.id);
                self.evict_if_needed();
                Some(tex)
            }
            Err(_) => {
                self.failed.insert(title.id.clone(), now);
                None
            }
        }
    }

    fn evict_if_needed(&mut self) {
        while self.order.len() > COVER_BUDGET {
            let oldest = self.order.remove(0);
            self.textures.remove(&oldest);
        }
    }
}

/// Decodes a cover image, identifying the format from its contents.
///
/// Content sniffing rather than the file extension, because a hand-placed
/// `.cover` file has no extension by design — and because a `.jpg` that is
/// really a PNG should still load rather than fail on a technicality.
fn load_image(path: &PathBuf) -> Result<egui::ColorImage> {
    let decoded = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?
        .to_rgba8();
    let size = [decoded.width() as usize, decoded.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        decoded.as_raw(),
    ))
}

/// Draws the aiming cursor on the panel.
///
/// Without a rendered laser in the scene this is the only feedback the viewer
/// gets about where the controller is pointing, so it is deliberately large and
/// high-contrast: a dark outline keeps it legible over pale cover art, and a
/// bright core keeps it visible over dark art.
fn draw_cursor(ctx: &egui::Context, pos: egui::Pos2, pressed: bool) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("vrmp_cursor"),
    ));

    let radius = if pressed { 13.0 } else { 17.0 };
    let accent = Color32::from_rgb(120, 190, 255);

    // Outline first, so the cursor reads against any background.
    painter.circle_stroke(pos, radius + 2.0, egui::Stroke::new(4.0, Color32::from_black_alpha(190)));
    painter.circle_stroke(pos, radius, egui::Stroke::new(2.5, accent));

    // A filled centre gives a precise aim point; it grows on press so the click
    // is visibly acknowledged even when it lands on empty space.
    painter.circle_filled(
        pos,
        if pressed { 6.0 } else { 3.5 },
        if pressed { accent } else { Color32::WHITE },
    );
}
