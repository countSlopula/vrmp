//! Works out how a video file should be projected in the headset.
//!
//! There is no reliable universal metadata for this. In practice VR video is
//! distributed with the layout encoded in the file name (`..._LR_180.mp4`), and
//! different producers use different spellings for the same thing: one writes
//! `_LR_180`, another `_SBS_180`, and both mean identical pixels. Some
//! producers tag nothing at all, so we fall back to inferring layout from the
//! frame aspect ratio.
//!
//! Getting this wrong is very visible — a flat video projected onto a sphere is
//! unwatchable, and a stereo video treated as mono shows both eyes to one eye —
//! so every guess records how confident it is, and the player lets the viewer
//! override it at runtime.

use serde::{Deserialize, Serialize};

/// How the frame maps onto geometry around the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Projection {
    /// Ordinary 2D video, shown on a flat panel floating in front of the viewer.
    Flat,
    /// Equirectangular, covering `degrees` of horizontal arc (180 or 360).
    Equirect { degrees: u16 },
    /// Fisheye domes as used by MKX200/RF52-style cameras, `degrees` field of view.
    /// Not representable as a compositor layer, so these need mesh rendering.
    Fisheye { degrees: u16 },
}

/// How the two eyes are packed into a single frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stereo {
    /// One image for both eyes.
    Mono,
    /// Left eye in the left half of the frame, right eye in the right half.
    SideBySide,
    /// Left eye in the top half, right eye in the bottom half.
    TopBottom,
}

/// How much to trust a [`Layout`], which decides whether the player nags the
/// viewer to confirm it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Confidence {
    /// Inferred from frame dimensions alone; may well be wrong.
    Guessed,
    /// Read from an explicit tag in the file name.
    Tagged,
    /// Set by the viewer, or read from container metadata. Never overridden.
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub projection: Projection,
    pub stereo: Stereo,
    pub confidence: Confidence,
}

impl Layout {
    pub const FLAT: Layout = Layout {
        projection: Projection::Flat,
        stereo: Stereo::Mono,
        confidence: Confidence::Guessed,
    };

    /// The horizontal arc each eye covers, in radians, for compositor layers.
    pub fn horizontal_angle(&self) -> f32 {
        match self.projection {
            Projection::Equirect { degrees } => (degrees as f32).to_radians(),
            Projection::Fisheye { degrees } => (degrees as f32).to_radians(),
            Projection::Flat => 0.0,
        }
    }

    /// Whether this can be handed to the runtime as an equirect composition
    /// layer. Fisheye content needs our own mesh, and flat content a quad.
    pub fn is_equirect(&self) -> bool {
        matches!(self.projection, Projection::Equirect { .. })
    }
}

/// Detects layout from a file name, then from frame dimensions if the name is
/// untagged. `width`/`height` are the coded frame size.
pub fn detect(file_name: &str, width: u32, height: u32) -> Layout {
    if let Some(layout) = from_file_name(file_name) {
        return layout;
    }
    from_dimensions(width, height)
}

/// Parses the layout tags producers embed in file names.
fn from_file_name(name: &str) -> Option<Layout> {
    let lower = name.to_ascii_lowercase();

    // Fisheye tags name the lens rather than the layout, and are always stereo
    // side-by-side in practice. Checked first because a name may contain both a
    // lens tag and a misleading angle.
    for (tag, degrees) in [
        ("mkx200", 200u16),
        ("mkx220", 220),
        ("vrca220", 220),
        ("rf52", 190),
        ("fisheye190", 190),
        ("fisheye", 180),
    ] {
        if lower.contains(tag) {
            return Some(Layout {
                projection: Projection::Fisheye { degrees },
                stereo: Stereo::SideBySide,
                confidence: Confidence::Tagged,
            });
        }
    }

    // Layout tags proper. `lr` and `sbs` are synonyms, as are `tb` and `ou`.
    let stereo = if has_tag(&lower, "lr") || has_tag(&lower, "sbs") {
        Some(Stereo::SideBySide)
    } else if has_tag(&lower, "tb") || has_tag(&lower, "ou") {
        Some(Stereo::TopBottom)
    } else if has_tag(&lower, "mono") {
        Some(Stereo::Mono)
    } else {
        None
    };

    let degrees = if has_tag(&lower, "360") {
        Some(360u16)
    } else if has_tag(&lower, "180") {
        Some(180)
    } else {
        None
    };

    match (stereo, degrees) {
        // Both present: unambiguous.
        (Some(stereo), Some(degrees)) => Some(Layout {
            projection: Projection::Equirect { degrees },
            stereo,
            confidence: Confidence::Tagged,
        }),
        // An angle alone implies mono at that angle.
        (None, Some(degrees)) => Some(Layout {
            projection: Projection::Equirect { degrees },
            stereo: Stereo::Mono,
            confidence: Confidence::Tagged,
        }),
        // A stereo tag alone is almost always 180 side-by-side footage.
        (Some(stereo), None) => Some(Layout {
            projection: Projection::Equirect { degrees: 180 },
            stereo,
            confidence: Confidence::Tagged,
        }),
        (None, None) => None,
    }
}

/// Tests for `tag` delimited by separators, so `_lr_` matches but `colour` does
/// not match `ou`, and `1080` does not match `180`.
fn has_tag(haystack: &str, tag: &str) -> bool {
    let is_sep = |c: char| !c.is_ascii_alphanumeric();
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(tag) {
        let start = from + rel;
        let end = start + tag.len();
        let before_ok = start == 0 || is_sep(bytes[start - 1] as char);
        let after_ok = end == bytes.len() || is_sep(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Last-resort inference from the frame aspect ratio.
///
/// Each equirect eye covering 180° horizontally and vertically is square, so
/// side-by-side 180 is 2:1 overall and top-bottom 180 is 1:2. A 360° eye is 2:1,
/// making mono 360 also 2:1 — genuinely ambiguous with side-by-side 180. VR
/// libraries are overwhelmingly the latter, so that is the guess, and the player
/// offers a live toggle because we will sometimes be wrong.
fn from_dimensions(width: u32, height: u32) -> Layout {
    if width == 0 || height == 0 {
        return Layout::FLAT;
    }
    let aspect = width as f32 / height as f32;
    let near = |target: f32| (aspect - target).abs() < target * 0.06;

    if near(4.0) {
        // Two 360° eyes side by side.
        Layout {
            projection: Projection::Equirect { degrees: 360 },
            stereo: Stereo::SideBySide,
            confidence: Confidence::Guessed,
        }
    } else if near(2.0) {
        Layout {
            projection: Projection::Equirect { degrees: 180 },
            stereo: Stereo::SideBySide,
            confidence: Confidence::Guessed,
        }
    } else if near(0.5) {
        Layout {
            projection: Projection::Equirect { degrees: 180 },
            stereo: Stereo::TopBottom,
            confidence: Confidence::Guessed,
        }
    } else if near(1.0) {
        // Equally consistent with mono 180 and top-bottom 360; mono is safer,
        // since showing mono to both eyes merely loses depth rather than
        // showing each eye the wrong half of the image.
        Layout {
            projection: Projection::Equirect { degrees: 180 },
            stereo: Stereo::Mono,
            confidence: Confidence::Guessed,
        }
    } else {
        // 16:9 and friends: ordinary video.
        Layout::FLAT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq(degrees: u16, stereo: Stereo) -> (Projection, Stereo) {
        (Projection::Equirect { degrees }, stereo)
    }

    #[test]
    fn parses_the_tag_styles_present_in_the_library() {
        // The most common style: studio, scene, id, then the layout.
        let l = detect("STUDIO_Scene_Name_original_12345_LR_180.mp4", 4096, 2048);
        assert_eq!((l.projection, l.stereo), eq(180, Stereo::SideBySide));
        assert_eq!(l.confidence, Confidence::Tagged);

        // A different spelling of the identical layout.
        let l = detect("scene-name-VR_4k60FPS_SBS_180.mp4", 4000, 2000);
        assert_eq!((l.projection, l.stereo), eq(180, Stereo::SideBySide));

        // Top-bottom, 24 files in the library.
        let l = detect("TITLE-001_A_2160p_4K_TB_180.mp4", 2160, 4320);
        assert_eq!((l.projection, l.stereo), eq(180, Stereo::TopBottom));
    }

    #[test]
    fn falls_back_to_aspect_ratio_when_untagged() {
        // Some files say "VR" but carry no layout tag at all.
        let l = detect("42. Some Scene Name A1 VR 4k h265.mp4", 8192, 4096);
        assert_eq!((l.projection, l.stereo), eq(180, Stereo::SideBySide));
        assert_eq!(l.confidence, Confidence::Guessed);
    }

    #[test]
    fn ordinary_video_is_not_projected_onto_a_sphere() {
        // Producers often ship flat 2D cuts beside the VR versions.
        let l = detect("scene_name_4K60fps_2-b_noSlate.mp4", 3840, 2160);
        assert_eq!(l.projection, Projection::Flat);
        assert_eq!(l.stereo, Stereo::Mono);
    }

    #[test]
    fn tag_matching_respects_delimiters() {
        // "1080" must not be read as a 180 tag, and the resolution must not
        // drag an untagged file into equirect.
        assert!(!has_tag("scene1_1080p60fps.mp4", "180"));
        let l = detect("scene1_1080p60FPS.mp4", 1920, 1080);
        assert_eq!(l.projection, Projection::Flat);

        // A word ending in "ou" must not be read as over-under.
        assert!(!has_tag("a_rendezvous_clip.mp4", "ou"));
    }

    #[test]
    fn recognises_fisheye_lenses() {
        let l = detect("STUDIO_scene_MKX200.mp4", 4096, 2048);
        assert_eq!(l.projection, Projection::Fisheye { degrees: 200 });
        assert_eq!(l.stereo, Stereo::SideBySide);
    }
}
