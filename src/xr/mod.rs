//! OpenXR session, swapchains, and the frame loop.
//!
//! The player does not render a sphere itself. Where the runtime advertises
//! `XR_KHR_composition_layer_equirect2`, the decoded video frame is handed to
//! the runtime as an equirect layer and the compositor performs the projection.
//! That is both less code and better image quality: the compositor reprojects
//! the layer at the headset's own rate, so the picture stays stable during head
//! motion regardless of the video's frame rate.
//!
//! Stereo costs nothing extra. A side-by-side frame is one swapchain image, and
//! the two eyes are two layers over the same image distinguished by
//! `eye_visibility` and an `image_rect` naming each half.

pub mod gl_context;
pub mod input;
pub mod desktop;

use anyhow::{bail, Context, Result};
use openxr as xr;
use openxr::Graphics as _;

use crate::library::{Projection, Stereo};

/// GL_SRGB8_ALPHA8. Preferred so the compositor knows the encoding; mpv writes
/// sRGB-encoded bytes with `GL_FRAMEBUFFER_SRGB` left disabled.
const GL_SRGB8_ALPHA8: u32 = 0x8C43;
const GL_RGBA8: u32 = 0x8058;

pub struct Xr {
    pub instance: xr::Instance,
    pub system: xr::SystemId,
    pub session: xr::Session<xr::OpenGL>,
    pub frame_wait: xr::FrameWaiter,
    pub frame_stream: xr::FrameStream<xr::OpenGL>,
    /// Room-scale space when available, falling back to a seated origin.
    pub stage: xr::Space,
    /// Tracks the headset itself, used to recentre content on the viewer.
    pub view_space: xr::Space,
    pub environment_blend_mode: xr::EnvironmentBlendMode,
    /// Whether the runtime can show a curved panel. Without it the interface
    /// falls back to a flat quad, which every runtime supports.
    pub has_cylinder: bool,
    session_running: bool,
}

impl Xr {
    /// Connects to the runtime and creates a session on the given GL context.
    pub fn new(gl: &gl_context::GlContext) -> Result<Self> {
        let entry = xr::Entry::linked();
        let available = entry
            .enumerate_extensions()
            .context("enumerating OpenXR extensions (is a runtime installed?)")?;

        // The player needs no particular runtime, only these standard
        // extensions. OpenGL is the one hard requirement, since the decoder
        // renders through it; the layer types degrade to alternatives.
        if !available.khr_opengl_enable {
            bail!(
                "this OpenXR runtime does not support XR_KHR_opengl_enable, which \
                 the video pipeline requires.\n\
                 Monado-based runtimes (WiVRn, Monado, Envision) provide it; \
                 SteamVR on Linux does not."
            );
        }
        if !available.khr_composition_layer_equirect2 {
            // Not fatal: flat playback still works, and the caller reports it.
            eprintln!(
                "warning: runtime lacks XR_KHR_composition_layer_equirect2; \
                 180/360 projection will be unavailable, flat playback still works"
            );
        }
        if !available.khr_composition_layer_cylinder {
            eprintln!(
                "note: runtime lacks XR_KHR_composition_layer_cylinder; \
                 the interface will be shown on a flat panel instead of a curved one"
            );
        }

        let mut enabled = xr::ExtensionSet::default();
        enabled.khr_opengl_enable = true;
        enabled.khr_composition_layer_equirect2 = available.khr_composition_layer_equirect2;
        enabled.khr_composition_layer_cylinder = available.khr_composition_layer_cylinder;

        let instance = entry
            .create_instance(
                &xr::ApplicationInfo {
                    application_name: "vrmp",
                    application_version: 1,
                    engine_name: "vrmp",
                    engine_version: 1,
                    api_version: xr::Version::new(1, 0, 0),
                },
                &enabled,
                &[],
            )
            .context("creating OpenXR instance")?;

        let props = instance.properties()?;
        eprintln!("openxr runtime: {} {}", props.runtime_name, props.runtime_version);

        let system = instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .context("no head-mounted display found — is the headset connected?")?;

        // The runtime states which GL versions it supports; failing to query
        // this is a protocol error even though we do not act on the result.
        let _reqs = xr::OpenGL::requirements(&instance, system)?;

        let (session, frame_wait, frame_stream) = unsafe {
            instance.create_session::<xr::OpenGL>(
                system,
                &xr::opengl::SessionCreateInfo::Xlib {
                    x_display: gl.display as *mut _,
                    visualid: gl.visual_id,
                    glx_fb_config: gl.fb_config as *mut _,
                    glx_drawable: gl.window,
                    glx_context: gl.context as *mut _,
                },
            )
        }
        .context("creating OpenXR session")?;

        // Stage space is floor-relative and preferred; some runtimes and seated
        // setups only offer Local.
        let stage = session
            .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
            .or_else(|_| {
                session.create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
            })
            .context("creating a reference space")?;

        let view_space =
            session.create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)?;

        let environment_blend_mode = instance
            .enumerate_environment_blend_modes(system, xr::ViewConfigurationType::PRIMARY_STEREO)?
            .first()
            .copied()
            .unwrap_or(xr::EnvironmentBlendMode::OPAQUE);

        Ok(Xr {
            instance,
            system,
            session,
            frame_wait,
            frame_stream,
            stage,
            view_space,
            environment_blend_mode,
            has_cylinder: available.khr_composition_layer_cylinder,
            session_running: false,
        })
    }

    pub fn is_running(&self) -> bool {
        self.session_running
    }

    /// Handles runtime lifecycle events.
    ///
    /// Returns `Ok(false)` when the application should exit, which happens when
    /// the runtime says so or the user quits from the system menu.
    pub fn poll_events(&mut self, buffer: &mut xr::EventDataBuffer) -> Result<bool> {
        while let Some(event) = self.instance.poll_event(buffer)? {
            use xr::Event::*;
            match event {
                SessionStateChanged(e) => match e.state() {
                    xr::SessionState::READY => {
                        self.session
                            .begin(xr::ViewConfigurationType::PRIMARY_STEREO)?;
                        self.session_running = true;
                        eprintln!("xr session ready");
                    }
                    xr::SessionState::STOPPING => {
                        self.session.end()?;
                        self.session_running = false;
                        eprintln!("xr session stopping");
                    }
                    xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                        return Ok(false);
                    }
                    _ => {}
                },
                InstanceLossPending(_) => return Ok(false),
                _ => {}
            }
        }
        Ok(true)
    }

    /// Yaw, in radians, of the headset's current forward direction.
    ///
    /// Used to place video in front of wherever the viewer is looking when
    /// playback starts, rather than at a fixed compass bearing.
    pub fn view_yaw(&self, time: xr::Time) -> Result<f32> {
        Ok(self.view_angles(time)?.0)
    }

    /// Heading and recline of the headset, in radians, with roll discarded.
    ///
    /// Roll is dropped deliberately: tilting your head sideways should never
    /// tilt the horizon, which is both disorienting and hard to undo. Pitch is
    /// kept because it is the difference between sitting up and lying down, and
    /// content shot to be watched lying down has to be pitched to match.
    pub fn view_angles(&self, time: xr::Time) -> Result<(f32, f32)> {
        let location = self.view_space.locate(&self.stage, time)?;
        let q = location.pose.orientation;
        let (yaw, pitch, _roll) = glam::Quat::from_xyzw(q.x, q.y, q.z, q.w)
            .to_euler(glam::EulerRot::YXZ);
        Ok((yaw, pitch))
    }
}

/// A swapchain plus a framebuffer per image, so mpv and egui can render into it.
pub struct RenderTarget {
    pub swapchain: xr::Swapchain<xr::OpenGL>,
    pub width: u32,
    pub height: u32,
    /// Whether an image has ever been released on this swapchain.
    ///
    /// A layer may only reference a swapchain that has had at least one image
    /// released; submitting one before that is `XR_ERROR_LAYER_INVALID`. This
    /// matters because we deliberately skip rendering on frames where the
    /// decoder has nothing new — fine once the compositor has an image to keep
    /// reusing, but not before the first one exists.
    presented: bool,
    /// One GL framebuffer per swapchain image, indexed as the runtime indexes.
    framebuffers: Vec<glow::Framebuffer>,
}

impl RenderTarget {
    /// Creates a swapchain sized exactly to `width` x `height`.
    ///
    /// Sizing the video swapchain to the video's own resolution means mpv writes
    /// one pixel per source pixel, with no rescale on our side.
    pub fn new(
        session: &xr::Session<xr::OpenGL>,
        gl: &glow::Context,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        use glow::HasContext as _;

        let formats = session.enumerate_swapchain_formats()?;
        let format = if formats.contains(&GL_SRGB8_ALPHA8) {
            GL_SRGB8_ALPHA8
        } else if formats.contains(&GL_RGBA8) {
            GL_RGBA8
        } else {
            *formats
                .first()
                .context("runtime offered no swapchain formats")?
        };

        let swapchain = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                    | xr::SwapchainUsageFlags::SAMPLED,
                format,
                sample_count: 1,
                width,
                height,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })
            .with_context(|| format!("creating a {width}x{height} swapchain"))?;

        // Wrap each swapchain texture in a framebuffer once, up front: creating
        // them per frame would stall the pipeline.
        let images = swapchain.enumerate_images()?;
        let mut framebuffers = Vec::with_capacity(images.len());
        for texture in images {
            unsafe {
                let fbo = gl
                    .create_framebuffer()
                    .map_err(|e| anyhow::anyhow!("creating framebuffer: {e}"))?;
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(glow::NativeTexture(
                        std::num::NonZeroU32::new(texture)
                            .context("runtime returned a zero texture name")?,
                    )),
                    0,
                );
                let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if status != glow::FRAMEBUFFER_COMPLETE {
                    bail!("swapchain framebuffer incomplete: 0x{status:x}");
                }
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                framebuffers.push(fbo);
            }
        }

        Ok(RenderTarget { swapchain, width, height, presented: false, framebuffers })
    }

    /// Acquires the next image and returns its framebuffer name.
    ///
    /// The caller must call [`Self::release`] after rendering.
    pub fn acquire(&mut self) -> Result<u32> {
        let index = self.swapchain.acquire_image()?;
        self.swapchain.wait_image(xr::Duration::INFINITE)?;
        Ok(self.framebuffers[index as usize].0.get())
    }

    pub fn release(&mut self) -> Result<()> {
        self.swapchain.release_image()?;
        self.presented = true;
        Ok(())
    }

    /// Whether this target may be referenced by a composition layer yet.
    pub fn is_presentable(&self) -> bool {
        self.presented
    }

    /// The image rectangle for one eye, given how the frame packs stereo.
    pub fn eye_rect(&self, stereo: Stereo, eye: Eye) -> xr::Rect2Di {
        eye_rect_of(self.width, self.height, stereo, eye)
    }
}

/// Which part of a packed frame belongs to one eye.
///
/// Free-standing so it can be tested without a live OpenXR session. Getting this
/// wrong swaps the eyes, which is deeply unpleasant to look at and easy to miss
/// in a still screenshot, so the convention is pinned down by tests: left eye
/// occupies the left half of a side-by-side frame and the top half of a
/// top-bottom one.
fn eye_rect_of(width: u32, height: u32, stereo: Stereo, eye: Eye) -> xr::Rect2Di {
    let (w, h) = (width as i32, height as i32);
    match (stereo, eye) {
        (Stereo::Mono, _) => rect(0, 0, w, h),
        (Stereo::SideBySide, Eye::Left) => rect(0, 0, w / 2, h),
        (Stereo::SideBySide, Eye::Right) => rect(w / 2, 0, w / 2, h),
        (Stereo::TopBottom, Eye::Left) => rect(0, 0, w, h / 2),
        (Stereo::TopBottom, Eye::Right) => rect(0, h / 2, w, h / 2),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eye {
    Left,
    Right,
}

impl Eye {
    pub fn visibility(self) -> xr::EyeVisibility {
        match self {
            Eye::Left => xr::EyeVisibility::LEFT,
            Eye::Right => xr::EyeVisibility::RIGHT,
        }
    }
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> xr::Rect2Di {
    xr::Rect2Di {
        offset: xr::Offset2Di { x, y },
        extent: xr::Extent2Di { width: w, height: h },
    }
}

/// Geometry for an equirect video layer.
pub struct EquirectGeometry {
    pub central_horizontal_angle: f32,
    pub upper_vertical_angle: f32,
    pub lower_vertical_angle: f32,
}

impl EquirectGeometry {
    /// Derives layer angles from the detected projection.
    ///
    /// Equirect frames span their full vertical range regardless of horizontal
    /// coverage: a 180 file is a hemisphere, a 360 file a full sphere, and both
    /// run from pole to pole.
    pub fn for_projection(projection: Projection) -> Option<Self> {
        let degrees = match projection {
            Projection::Equirect { degrees } => degrees,
            // Fisheye needs a mesh the compositor cannot describe, and flat
            // content uses a quad layer instead.
            Projection::Fisheye { .. } | Projection::Flat => return None,
        };
        Some(EquirectGeometry {
            central_horizontal_angle: (degrees as f32).to_radians(),
            upper_vertical_angle: std::f32::consts::FRAC_PI_2,
            lower_vertical_angle: -std::f32::consts::FRAC_PI_2,
        })
    }
}

/// A pose that faces the given yaw, used to place content in front of the
/// viewer without inheriting their head pitch or roll.
pub fn yaw_pose(yaw: f32, position: xr::Vector3f) -> xr::Posef {
    pose_facing(yaw, 0.0, position)
}

/// A pose facing `yaw` and reclined by `pitch`, with no roll.
///
/// Yaw is applied first and pitch in the resulting frame, so pitch means "tip
/// the content towards the viewer" regardless of which way they are facing —
/// which is what makes content watchable lying down.
pub fn pose_facing(yaw: f32, pitch: f32, position: xr::Vector3f) -> xr::Posef {
    let q = glam::Quat::from_rotation_y(yaw) * glam::Quat::from_rotation_x(pitch);
    xr::Posef {
        orientation: xr::Quaternionf { x: q.x, y: q.y, z: q.z, w: q.w },
        position,
    }
}

/// Recline below which content is kept upright.
///
/// A viewer glancing down at the controls should not tip the whole scene, but
/// someone actually lying back should have it follow. Anything inside this cone
/// counts as sitting upright.
pub const UPRIGHT_PITCH_DEADZONE: f32 = 20.0;

/// Collapses small head tilts to level, leaving genuine recline intact.
pub fn significant_pitch(pitch: f32) -> f32 {
    if pitch.abs() < UPRIGHT_PITCH_DEADZONE.to_radians() {
        0.0
    } else {
        pitch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical side-by-side 180 frame.
    const W: u32 = 4320;
    const H: u32 = 2160;

    #[test]
    fn side_by_side_splits_left_then_right() {
        let left = eye_rect_of(W, H, Stereo::SideBySide, Eye::Left);
        let right = eye_rect_of(W, H, Stereo::SideBySide, Eye::Right);

        assert_eq!(left.offset.x, 0, "left eye starts at the left edge");
        assert_eq!(right.offset.x, W as i32 / 2, "right eye starts at the middle");
        assert_eq!(left.extent.width, W as i32 / 2);
        assert_eq!(right.extent.width, W as i32 / 2);
        // Both eyes use the full height, and together cover the whole frame.
        assert_eq!(left.extent.height, H as i32);
        assert_eq!(right.offset.x + right.extent.width, W as i32);
    }

    #[test]
    fn top_bottom_puts_the_left_eye_on_top() {
        let left = eye_rect_of(W, H, Stereo::TopBottom, Eye::Left);
        let right = eye_rect_of(W, H, Stereo::TopBottom, Eye::Right);

        assert_eq!(left.offset.y, 0);
        assert_eq!(right.offset.y, H as i32 / 2);
        assert_eq!(left.extent.width, W as i32, "no horizontal split");
        assert_eq!(right.offset.y + right.extent.height, H as i32);
    }

    #[test]
    fn mono_gives_both_eyes_the_whole_frame() {
        for eye in [Eye::Left, Eye::Right] {
            let r = eye_rect_of(W, H, Stereo::Mono, eye);
            assert_eq!((r.offset.x, r.offset.y), (0, 0));
            assert_eq!((r.extent.width, r.extent.height), (W as i32, H as i32));
        }
    }

    #[test]
    fn equirect_geometry_matches_the_projection() {
        let half = EquirectGeometry::for_projection(Projection::Equirect { degrees: 180 })
            .expect("180 is an equirect projection");
        assert!((half.central_horizontal_angle - std::f32::consts::PI).abs() < 1e-5);
        // Equirect frames span pole to pole regardless of horizontal coverage.
        assert!((half.upper_vertical_angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((half.lower_vertical_angle + std::f32::consts::FRAC_PI_2).abs() < 1e-5);

        let full = EquirectGeometry::for_projection(Projection::Equirect { degrees: 360 }).unwrap();
        assert!((full.central_horizontal_angle - std::f32::consts::TAU).abs() < 1e-5);

        // Neither flat nor fisheye can be expressed as an equirect layer.
        assert!(EquirectGeometry::for_projection(Projection::Flat).is_none());
        assert!(EquirectGeometry::for_projection(Projection::Fisheye { degrees: 200 }).is_none());
    }
}
