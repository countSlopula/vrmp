//! On-disk cache of probe results.
//!
//! Probing a large library with ffprobe can take minutes, especially over a
//! network share, and almost nothing changes between runs. Results are therefore
//! cached and invalidated by size and mtime, so a rescan only pays for the files
//! that actually changed.
//!
//! The cache lives inside the player's own data directory. Nothing is ever
//! written next to the media: the library is treated as a read-only mount.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::Layout;

/// A cached probe result, valid only while size and mtime match the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeEntry {
    pub size_bytes: u64,
    pub mtime_secs: i64,
    pub width: u32,
    pub height: u32,
    pub duration_secs: f64,
    pub codec: String,
    /// Layout as detected at scan time. A viewer override is stored separately
    /// in [`Overrides`] so that a rescan cannot silently discard it.
    pub layout: Layout,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProbeCache {
    #[serde(default)]
    entries: HashMap<PathBuf, ProbeEntry>,
}

impl ProbeCache {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating cache directory {}", parent.display()))?;
        }
        let json = serde_json::to_string(self)?;
        // Write via a temporary file so an interrupted save cannot leave a
        // truncated cache that fails to parse on next start.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Returns the cached probe if it still matches the file on disk.
    pub fn get(&self, path: &Path, size_bytes: u64, mtime_secs: i64) -> Option<&ProbeEntry> {
        self.entries
            .get(path)
            .filter(|e| e.size_bytes == size_bytes && e.mtime_secs == mtime_secs)
    }

    pub fn insert(&mut self, path: PathBuf, entry: ProbeEntry) {
        self.entries.insert(path, entry);
    }

    /// Drops entries whose files no longer exist, so the cache does not grow
    /// without bound as a library is reorganised.
    pub fn retain_existing(&mut self, seen: &std::collections::HashSet<PathBuf>) {
        self.entries.retain(|path, _| seen.contains(path));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Viewer corrections to detected layout, keyed by title id.
///
/// Layout detection is heuristic and will sometimes be wrong; when the viewer
/// fixes it in the headset the correction has to outlive both the session and
/// any future rescan.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default)]
    pub layouts: HashMap<String, Layout>,
    /// Playback position in seconds, keyed by title id, so a long title can be
    /// resumed where it was left.
    #[serde(default)]
    pub resume: HashMap<String, f64>,
    /// Which part that position belongs to, for titles split across discs.
    ///
    /// Kept as a separate map rather than folded into `resume` so that an older
    /// overrides file still parses; a failed parse would silently discard the
    /// viewer's layout corrections too, which are far more costly to lose.
    #[serde(default)]
    pub resume_part: HashMap<String, usize>,
}

impl Overrides {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Reapplies viewer layout corrections over freshly detected values.
///
/// Detection runs on every scan and would otherwise reinstate the same wrong
/// guess each time, silently undoing a correction the viewer made in the
/// headset. Every path that presents scanned titles has to call this, or it will
/// report layouts that disagree with what actually plays.
pub fn apply_overrides(titles: &mut [crate::library::Title], overrides: &Overrides) {
    for title in titles {
        let Some(layout) = overrides.layouts.get(&title.id) else { continue };
        for part in &mut title.parts {
            for variant in &mut part.variants {
                variant.layout = *layout;
            }
        }
    }
}
