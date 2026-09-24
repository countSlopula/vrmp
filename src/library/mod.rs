//! The media library: what is on disk, and how it groups into watchable titles.

pub mod index;
pub mod projection;
pub mod scan;
pub mod thumbs;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use projection::{Confidence, Layout, Projection, Stereo};

/// Extensions we will attempt to open. libmpv handles far more than this, but a
/// scanner that opens every file on a multi-terabyte share is worse than one
/// that misses an oddity, and these cover what media actually arrives as.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "m4v", "mov", "avi", "wmv", "ts", "m2ts", "flv",
];

/// A single file on disk, with everything needed to play it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Coded frame size. Zero until the file has been probed.
    pub width: u32,
    pub height: u32,
    pub duration_secs: f64,
    pub codec: String,
    pub layout: Layout,
}

impl MediaFile {
    /// Rough ranking used to pick a default among quality variants: pixel count,
    /// which correctly prefers 8K over 4K and 4K over 2K.
    pub fn quality_rank(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn file_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    }

    /// Per-eye frame size, after unpacking the stereo layout. This is what
    /// actually has to be decoded and is the honest measure of "how big".
    pub fn per_eye_size(&self) -> (u32, u32) {
        match self.layout.stereo {
            Stereo::Mono => (self.width, self.height),
            Stereo::SideBySide => (self.width / 2, self.height),
            Stereo::TopBottom => (self.width, self.height / 2),
        }
    }
}

/// One watchable unit. Where a release is split across several files — discs
/// are commonly `_A`, `_B`, `_C` — each is a separate part played in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    /// "A", "B", … or empty when a title is a single part.
    pub label: String,
    /// Quality variants of identical content, best first. Never empty.
    pub variants: Vec<MediaFile>,
}

impl Part {
    /// The variant to play by default: the highest resolution available.
    pub fn best(&self) -> &MediaFile {
        &self.variants[0]
    }
}

/// A title as shown in the browser: one cover, one row in the grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Title {
    /// Stable identifier derived from the title's path, used as the cache key
    /// for cover art and as the persisted key for viewer overrides.
    pub id: String,
    pub name: String,
    /// The collection this came from, i.e. the top-level folder under a root.
    pub collection: String,
    /// Directory the title lives in, or the containing directory for a title
    /// built from loose files.
    pub folder: PathBuf,
    pub parts: Vec<Part>,
    /// Cover image found next to the media, if any. When absent the player
    /// generates one into its own cache rather than writing to the library.
    pub cover: Option<PathBuf>,
}

impl Title {
    pub fn total_duration_secs(&self) -> f64 {
        self.parts.iter().map(|p| p.best().duration_secs).sum()
    }

    pub fn total_size_bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.best().size_bytes).sum()
    }

    /// Layout shown as the browser's badge.
    ///
    /// Some releases ship flat 2D cuts alongside VR ones in the same folder, so
    /// the first part is not representative. A title that contains any VR
    /// content is a VR title, and is reported as such.
    pub fn layout(&self) -> Layout {
        self.first_vr_part()
            .or_else(|| self.parts.first())
            .map(|p| p.best().layout)
            .unwrap_or(Layout::FLAT)
    }

    /// The part playback should start from: the first VR part where the title
    /// has one, so that picking a mixed release does not open a flat 2D loop in
    /// the headset.
    pub fn default_part_index(&self) -> usize {
        self.parts
            .iter()
            .position(|p| !matches!(p.best().layout.projection, Projection::Flat))
            .unwrap_or(0)
    }

    fn first_vr_part(&self) -> Option<&Part> {
        self.parts
            .iter()
            .find(|p| !matches!(p.best().layout.projection, Projection::Flat))
    }

    /// Whether the title has any VR content at all.
    pub fn is_vr(&self) -> bool {
        self.first_vr_part().is_some()
    }

    /// True when a title mixes VR and flat content, which the browser flags so
    /// the viewer knows both are available.
    pub fn is_mixed(&self) -> bool {
        let vr = self
            .parts
            .iter()
            .filter(|p| !matches!(p.best().layout.projection, Projection::Flat))
            .count();
        vr > 0 && vr < self.parts.len()
    }
}

// Cover art is only ever read from the library; the player never writes it
// there. Anything it generates goes to its own directory instead.

/// A bare `.cover` file beside the media is the preferred way to supply artwork
/// by hand, and takes precedence over every other name.
///
/// It deliberately has no extension: the image format is detected from the
/// file's contents, so any common image type can simply be renamed to `.cover`.
/// Being a dotfile, it also stays out of the way in ordinary directory listings.
/// In a title folder it covers the whole title; beside a loose video file,
/// `scene.mp4` is matched by `scene.cover`.
pub const COVER_MARKER: &str = ".cover";

/// Other cover file names recognised next to media, in order of preference.
pub const COVER_NAMES: &[&str] = &[
    "cover", "folder", "poster", "thumb", "fanart",
];

/// Extensions tried with each of [`COVER_NAMES`]. The `.cover` marker needs no
/// entry here, since its format comes from its contents.
pub const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];
