//! Cover art.
//!
//! Media often arrives with no cover art, and library directories are treated
//! as read-only mounts, so covers are extracted with ffmpeg into the player's
//! own directory, keyed by title id.
//!
//! VR frames need care: a frame grabbed from a side-by-side 180 file is a pair
//! of fisheye-ish circles, which is unrecognisable as a thumbnail. We therefore
//! crop to one eye and take the centre region, which is roughly what the viewer
//! looks at, before scaling down.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::io::Write;
use std::process::Command;

use anyhow::{bail, Result};

use super::{MediaFile, Projection, Stereo, Title};
use crate::ui::COVER_ASPECT;

/// Width of generated covers. Wide enough to look sharp on a curved panel a
/// couple of metres away, small enough that hundreds fit in memory.
const COVER_WIDTH: u32 = 480;

/// Where a title's generated cover lives.
pub fn cover_path(covers_dir: &Path, title_id: &str) -> PathBuf {
    covers_dir.join(format!("{title_id}.jpg"))
}

/// Returns the cover for a title, generating one if needed.
///
/// Hand-placed art beside the media always wins: if someone has curated a
/// `cover.jpg`, that is a deliberate choice and better than any frame grab.
pub fn ensure_cover(covers_dir: &Path, title: &Title) -> Result<PathBuf> {
    if let Some(existing) = &title.cover {
        return Ok(existing.clone());
    }
    let out = cover_path(covers_dir, &title.id);
    if out.is_file() {
        return Ok(out);
    }
    let Some(part) = title.parts.first() else {
        bail!("title {} has no parts", title.name);
    };
    generate(part.best(), &out)?;
    Ok(out)
}

/// Extracts a representative frame, unpacking stereo and cropping for VR.
fn generate(media: &MediaFile, out: &Path) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Seek to a little way in. Openings are frequently black frames, titles, or
    // studio logos, none of which identify the scene.
    //
    // Clamped to stay inside the file: the one-second floor keeps the grab past
    // any opening black, but on a very short clip it would land beyond the end
    // and ffmpeg would write nothing at all.
    let seek = seek_point(media);

    let filter = thumbnail_filter(media);

    // `-ss` before `-i` seeks by keyframe without decoding the intervening
    // frames, which matters when the file is 10 GB across a network share.
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(["-ss", &format!("{seek:.3}")])
        .arg("-i")
        .arg(&media.path)
        .args(["-frames:v", "1", "-vf", &filter, "-q:v", "3"])
        .arg(out)
        .status()?;

    if !status.success() {
        // Deliberately does not repeat the path: every caller already reports
        // which file it was working on, and doubling it makes the log harder to
        // scan rather than easier.
        bail!("ffmpeg could not extract a frame (seek {seek:.1}s of {:.1}s)", media.duration_secs);
    }
    Ok(())
}

/// Where in the file to grab the frame from.
///
/// Clamped to stay inside the clip: the one-second floor keeps the grab past any
/// opening black, but on a very short clip it would land beyond the end and
/// ffmpeg would write nothing at all.
fn seek_point(media: &MediaFile) -> f64 {
    let fraction = if media.duration_secs > 120.0 { 0.25 } else { 0.35 };
    (media.duration_secs * fraction)
        .max(1.0)
        .min((media.duration_secs - 0.2).max(0.0))
}

/// Builds the ffmpeg filter chain that turns a frame into a legible thumbnail.
fn thumbnail_filter(media: &MediaFile) -> String {
    let mut stages: Vec<String> = Vec::new();

    // Take a single eye, so the thumbnail is not a doubled image.
    match media.layout.stereo {
        Stereo::Mono => {}
        Stereo::SideBySide => stages.push("crop=iw/2:ih:0:0".into()),
        Stereo::TopBottom => stages.push("crop=iw:ih/2:0:0".into()),
    }

    // In equirect footage the edges are extreme distortion and the subject is
    // near the centre of the forward hemisphere, so crop to the middle.
    match media.layout.projection {
        Projection::Equirect { degrees } => {
            // 360 content wraps the full sphere, so the useful region is a
            // narrower slice of the frame than for 180 content.
            let keep = if degrees >= 360 { 0.28 } else { 0.55 };
            stages.push(format!(
                "crop=iw*{keep}:ih*{keep}:(iw-iw*{keep})/2:(ih-ih*{keep})/2"
            ));
        }
        Projection::Fisheye { .. } => {
            stages.push("crop=iw*0.6:ih*0.6:(iw-iw*0.6)/2:(ih-ih*0.6)/2".into());
        }
        Projection::Flat => {}
    }

    // Finish at the same portrait shape as real cover art, so a library mixing
    // generated grabs with hand-placed posters still lays out as an even grid.
    // Cropping rather than padding: a frame grab has no meaningful edges, and
    // bars would make the generated ones look broken next to real covers.
    let height = (COVER_WIDTH as f32 / COVER_ASPECT).round() as u32;
    stages.push(format!(
        "scale={COVER_WIDTH}:{height}:force_original_aspect_ratio=increase:flags=lanczos"
    ));
    stages.push(format!("crop={COVER_WIDTH}:{height}"));
    stages.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Confidence, Layout};

    fn media(stereo: Stereo, projection: Projection) -> MediaFile {
        MediaFile {
            path: PathBuf::from("/tmp/x.mp4"),
            size_bytes: 0,
            width: 4096,
            height: 2048,
            duration_secs: 600.0,
            codec: "h264".into(),
            layout: Layout { projection, stereo, confidence: Confidence::Tagged },
        }
    }

    #[test]
    fn side_by_side_is_cropped_to_one_eye() {
        let f = thumbnail_filter(&media(Stereo::SideBySide, Projection::Equirect { degrees: 180 }));
        assert!(f.starts_with("crop=iw/2:ih:0:0"), "got {f}");
    }

    #[test]
    fn flat_video_is_not_cropped_before_framing() {
        // Nothing to unpack or de-distort, so the only stages are the ones that
        // bring it to cover shape.
        let f = thumbnail_filter(&media(Stereo::Mono, Projection::Flat));
        assert!(f.starts_with("scale="), "got {f}");
        assert_eq!(f.matches("crop=").count(), 1, "only the framing crop: {f}");
    }

    /// Every cover ends at the grid's aspect ratio, whatever the source shape,
    /// so hand-placed posters and generated grabs tile together evenly.
    #[test]
    fn all_covers_end_at_the_grid_aspect() {
        let height = (COVER_WIDTH as f32 / COVER_ASPECT).round() as u32;
        for (stereo, projection) in [
            (Stereo::Mono, Projection::Flat),
            (Stereo::SideBySide, Projection::Equirect { degrees: 180 }),
            (Stereo::TopBottom, Projection::Equirect { degrees: 360 }),
            (Stereo::SideBySide, Projection::Fisheye { degrees: 200 }),
        ] {
            let f = thumbnail_filter(&media(stereo, projection));
            assert!(
                f.ends_with(&format!("crop={COVER_WIDTH}:{height}")),
                "{stereo:?}/{projection:?} did not end at cover shape: {f}"
            );
        }
    }
}

/// Generates missing covers in the background, newest-looking first.
///
/// Covers are a cache, not content: every one can be rebuilt from the media. But
/// rebuilding is slow — each is an ffmpeg seek into a multi-gigabyte file, which
/// over a network share costs about a second — so doing it eagerly at startup
/// would mean a minute of staring at empty tiles.
///
/// Running it on a worker instead means the library is usable immediately and
/// fills in as it goes, and a title added later gets art without anyone
/// remembering to run `vrmp covers`.
pub fn spawn_generator(covers_dir: &Path, log_path: &Path, titles: &[Title]) -> Arc<Progress> {
    // Collect the work up front so the thread owns everything it needs and does
    // not borrow the library.
    let covers_dir = covers_dir.to_path_buf();
    let pending: Vec<(PathBuf, MediaFile)> = titles
        .iter()
        .filter(|t| t.cover.is_none())
        .filter_map(|t| {
            let out = cover_path(&covers_dir, &t.id);
            if out.is_file() {
                return None;
            }
            Some((out, t.parts.first()?.best().clone()))
        })
        .collect();

    let progress = Arc::new(Progress {
        total: pending.len(),
        done: AtomicUsize::new(0),
        failed: AtomicUsize::new(0),
        log_path: log_path.to_path_buf(),
    });

    if pending.is_empty() {
        return progress;
    }

    let worker = Arc::clone(&progress);
    let log_path = log_path.to_path_buf();
    std::thread::spawn(move || {
        eprintln!("generating {} missing covers in the background…", worker.total);

        // Started fresh each run so the file describes this run, and the count
        // shown on screen matches what is actually in it. Opened lazily on the
        // first failure: an empty log left behind from a clean run would be
        // confusing to find.
        let mut log: Option<std::fs::File> = None;

        for (out, media) in pending {
            if let Err(e) = generate(&media, &out) {
                eprintln!("  cover failed for {}: {e}", media.path.display());
                worker.failed.fetch_add(1, Ordering::Relaxed);

                let file = log.get_or_insert_with(|| open_log(&log_path));
                // Full path, not just the file name: a bare name is not enough
                // to find the offending file in a library this size.
                let _ = writeln!(file, "{}\n    {e}", media.path.display());
                let _ = file.flush();
            }
            // Counted either way: the progress figure is about how much work is
            // left, and a file that cannot be read never will be.
            worker.done.fetch_add(1, Ordering::Relaxed);

            // A brief pause between files. This is mostly network reads on the
            // same share the video streams from, so it deliberately leaves room
            // rather than saturating it.
            std::thread::sleep(std::time::Duration::from_millis(120));
        }

        let failed = worker.failures();
        if failed > 0 {
            eprintln!("cover generation finished — {failed} failed, see {}", log_path.display());
        } else {
            eprintln!("cover generation finished");
        }
    });

    progress
}

/// Opens the failure log, truncating whatever the previous run left.
fn open_log(path: &Path) -> std::fs::File {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::File::create(path) {
        Ok(mut file) => {
            let _ = writeln!(
                file,
                "covers that could not be generated\n\
                 the media is still playable; only its thumbnail is missing\n"
            );
            file
        }
        // Falling back to the sink keeps a failing log from taking the
        // generator down with it; the same lines still reach stderr.
        Err(e) => {
            eprintln!("warning: cannot write {}: {e}", path.display());
            std::fs::File::create("/dev/null").expect("opening /dev/null")
        }
    }
}

/// How far background cover generation has got.
///
/// Reported to the viewer rather than acted on. Generation competes with video
/// for the same network share, so it may slow playback on a first run — but
/// telling them that is better than having the worker quietly pause whenever
/// something plays, which would make its speed depend on invisible state and
/// leave "why is this taking so long?" with no answer on screen.
pub struct Progress {
    pub total: usize,
    done: AtomicUsize,
    failed: AtomicUsize,
    log_path: std::path::PathBuf,
}

impl Progress {
    pub fn done(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    /// How many covers could not be generated.
    pub fn failures(&self) -> usize {
        self.failed.load(Ordering::Relaxed)
    }

    /// Whether work is still outstanding, and so worth telling the viewer about.
    pub fn in_progress(&self) -> bool {
        self.total > 0 && self.done() < self.total
    }

    /// Where the failures were written, for pointing the viewer at.
    ///
    /// Absolute where it can be resolved: the relative form is meaningless to
    /// someone reading it in a headset, who has no idea what the working
    /// directory was.
    pub fn log_path(&self) -> std::path::PathBuf {
        std::fs::canonicalize(&self.log_path).unwrap_or_else(|_| self.log_path.clone())
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;
    use crate::library::{Confidence, Layout, Stereo};

    fn clip(duration_secs: f64) -> MediaFile {
        MediaFile {
            path: PathBuf::from("/tmp/x.mp4"),
            size_bytes: 0,
            width: 4096,
            height: 2048,
            duration_secs,
            codec: "h264".into(),
            layout: Layout {
                projection: Projection::Equirect { degrees: 180 },
                stereo: Stereo::SideBySide,
                confidence: Confidence::Tagged,
            },
        }
    }

    /// The grab point has to land inside the file.
    ///
    /// The one-second floor that skips opening black would otherwise seek past
    /// the end of a very short clip, and ffmpeg writes no frame at all — which
    /// showed up as loops failing to produce any cover.
    #[test]
    fn the_seek_point_stays_within_the_clip() {
        for duration in [0.5, 1.0, 2.0, 16.0, 600.0, 4000.0] {
            let seek = seek_point(&clip(duration));
            assert!(
                seek < duration,
                "a {duration}s clip seeks to {seek}s, past its end"
            );
            assert!(seek >= 0.0, "negative seek for a {duration}s clip");
        }
    }

    /// Longer files still skip well past any opening titles.
    #[test]
    fn long_files_grab_from_a_quarter_in() {
        let seek = seek_point(&clip(4000.0));
        assert!((seek - 1000.0).abs() < 1.0, "expected a quarter in, got {seek}");
    }
}
