//! Configuration and on-disk locations.
//!
//! Everything the player writes — config, probe cache, generated covers — lives
//! under a single directory next to the binary's project, never inside the media
//! library and never scattered through the user's home directory. That keeps the
//! whole footprint visible and removable in one place, and keeps library mounts
//! strictly read-only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directories scanned for media. Each immediate subdirectory of a root is
    /// treated as a collection.
    pub roots: Vec<PathBuf>,

    /// Metres from the viewer to the curved library panel. Far enough to be
    /// comfortable to read, near enough to feel reachable.
    #[serde(default = "default_ui_distance")]
    pub ui_distance_m: f32,

    /// Horizontal arc the interface panel spans, in degrees. Panel height
    /// follows from this, so it is the single control for apparent size.
    #[serde(default = "default_ui_angle")]
    pub ui_angle_deg: f32,

    /// Mirror what the player draws into the desktop window, for checking what
    /// is playing without putting the headset on. This is the layer sources
    /// composited flat, not a view through an eye: no equirect projection and
    /// no panel curvature, since both of those are the compositor's work.
    ///
    /// The window exists regardless — it is what keyboard input arrives
    /// through — so this only decides whether anything is drawn into it.
    #[serde(default = "default_mirror")]
    pub mirror_window: bool,

    /// Width of that window in pixels; the height follows the source aspect.
    #[serde(default = "default_mirror_width")]
    pub mirror_width: u32,

    /// Seconds of media libmpv keeps buffered ahead. Generous by default: the
    /// library is on a network share, and a stall mid-scene is the single most
    /// irritating failure mode.
    #[serde(default = "default_cache_secs")]
    pub cache_secs: u32,

    /// Upper bound on that buffer in MiB, so an 8K stream cannot exhaust RAM.
    #[serde(default = "default_cache_mib")]
    pub cache_max_mib: u32,
}

fn default_ui_distance() -> f32 {
    2.0
}
fn default_ui_angle() -> f32 {
    // Comfortably inside the central field of view: readable without turning
    // your head, and small enough to feel like a panel rather than a wall.
    70.0
}
fn default_mirror() -> bool {
    true
}
fn default_mirror_width() -> u32 {
    // Large enough to read the interface and judge a frame, small enough to sit
    // beside other windows without taking over the desktop.
    960
}
fn default_cache_secs() -> u32 {
    60
}
fn default_cache_mib() -> u32 {
    2048
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            ui_distance_m: default_ui_distance(),
            ui_angle_deg: default_ui_angle(),
            mirror_window: default_mirror(),
            mirror_width: default_mirror_width(),
            cache_secs: default_cache_secs(),
            cache_max_mib: default_cache_mib(),
        }
    }
}

/// Resolved locations for everything the player reads and writes.
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub probe_cache: PathBuf,
    pub overrides: PathBuf,
    pub covers_dir: PathBuf,
    /// Where cover-generation failures are recorded, so the message shown in
    /// the headset can point somewhere real.
    pub cover_log: PathBuf,
}

impl Paths {
    /// Roots all state at `data_dir`, which defaults to `./data` beside the
    /// project so nothing escapes the project directory.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            config_file: data_dir.join("config.json"),
            probe_cache: data_dir.join("probe-cache.json"),
            overrides: data_dir.join("overrides.json"),
            covers_dir: data_dir.join("covers"),
            cover_log: data_dir.join("cover-failures.log"),
            data_dir,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;
        std::fs::create_dir_all(&self.covers_dir)
            .with_context(|| format!("creating {}", self.covers_dir.display()))?;
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
