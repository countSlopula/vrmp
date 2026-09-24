//! Walks library roots and groups files into titles.
//!
//! Real libraries are not uniformly organised. A single one may contain several
//! different shapes at once, and the grouping rules below handle all of them
//! without per-collection configuration:
//!
//! * one folder per release, split into discs — `Studio/TITLE-001/{_A,_B,_C}.mp4`
//! * deeply nested quality folders — `Studio/<title>/VR/VR 4K/<scene>.mp4`
//! * loose files with quality suffixes — `Studio/42. Scene Name … 2k.mp4`
//!
//! The unifying idea is that a folder directly under a collection is a title,
//! and within a title, file names that differ only by quality tokens are
//! variants of the same content while genuinely different names are parts.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use walkdir::WalkDir;

use super::index::{ProbeCache, ProbeEntry};
use super::{
    projection, Layout, MediaFile, Part, Title, COVER_EXTENSIONS, COVER_MARKER, COVER_NAMES,
    VIDEO_EXTENSIONS,
};

/// Quality and encoding tokens stripped when deciding whether two file names
/// describe the same content. Order matters only in that longer tokens must be
/// removed before shorter ones they contain.
const QUALITY_TOKENS: &[&str] = &[
    "remastered", "original", "hevc", "h265", "h264", "x265", "x264", "av1", "vp9",
    "8k", "6k", "5k", "4k", "3k", "2k", "uhd", "fhd", "hd", "sd",
    "60fps", "30fps", "24fps", "60", "slow ver.", "low-end", "mobile",
];

pub struct ScanReport {
    pub titles: Vec<Title>,
    pub files_seen: usize,
    pub files_probed: usize,
}

/// Scans every root and returns titles sorted by collection then name.
pub fn scan(roots: &[PathBuf], cache: &mut ProbeCache) -> Result<ScanReport> {
    let mut titles = Vec::new();
    let mut files_seen = 0usize;
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    // Files needing a fresh ffprobe, gathered across all roots so they can be
    // probed in one parallel batch.
    let pending: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    for root in roots {
        if !root.is_dir() {
            eprintln!("warning: library root {} is not a directory", root.display());
            continue;
        }
        for collection_entry in read_dir_sorted(root) {
            let (collection_name, video_files) = if collection_entry.is_dir() {
                let name = file_name_of(&collection_entry);
                (name, collect_videos(&collection_entry))
            } else {
                continue;
            };
            files_seen += video_files.len();
            for f in &video_files {
                seen_paths.insert(f.clone());
                if probe_from_cache(f, cache).is_none() {
                    pending.lock().unwrap().push(f.clone());
                }
            }
            titles.extend(group_into_titles(
                &collection_entry,
                &collection_name,
                video_files,
            ));
        }

        // Loose videos sitting directly in a root, with no collection folder.
        let loose = read_dir_sorted(root)
            .into_iter()
            .filter(|p| p.is_file() && is_video(p))
            .collect::<Vec<_>>();
        files_seen += loose.len();
        for f in &loose {
            seen_paths.insert(f.clone());
            if probe_from_cache(f, cache).is_none() {
                pending.lock().unwrap().push(f.clone());
            }
        }
        if !loose.is_empty() {
            let name = file_name_of(root);
            titles.extend(titles_from_loose_files(root, &name, loose));
        }
    }

    // Probe everything the cache could not answer. ffprobe spends nearly all of
    // its time waiting on network reads, so run a healthy number in parallel.
    let to_probe: Vec<PathBuf> = pending.into_inner().unwrap();
    let files_probed = to_probe.len();
    if files_probed > 0 {
        eprintln!("probing {files_probed} new or changed files…");
        for (path, entry) in probe_all(&to_probe) {
            cache.insert(path, entry);
        }
    }

    // Fill in the probed metadata now that every file has a cache entry.
    for title in &mut titles {
        for part in &mut title.parts {
            for variant in &mut part.variants {
                apply_probe(variant, cache);
            }
            // Highest resolution first, so `best()` picks the best quality.
            part.variants.sort_by_key(|v| std::cmp::Reverse(v.quality_rank()));
        }
        title.parts.retain(|p| !p.variants.is_empty());
    }
    titles.retain(|t| !t.parts.is_empty());

    cache.retain_existing(&seen_paths);
    titles.sort_by(|a, b| {
        a.collection
            .cmp(&b.collection)
            .then_with(|| natural_cmp(&a.name, &b.name))
    });

    Ok(ScanReport { titles, files_seen, files_probed })
}

/// Builds titles for one collection folder.
fn group_into_titles(collection_dir: &Path, collection: &str, videos: Vec<PathBuf>) -> Vec<Title> {
    let mut by_title_folder: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut loose: Vec<PathBuf> = Vec::new();

    for video in videos {
        match title_folder_for(collection_dir, &video) {
            Some(folder) => by_title_folder.entry(folder).or_default().push(video),
            // Sitting directly in the collection folder.
            None => loose.push(video),
        }
    }

    let mut titles: Vec<Title> = by_title_folder
        .into_iter()
        .map(|(folder, files)| title_from_folder(collection, folder, files))
        .collect();

    titles.extend(titles_from_loose_files(collection_dir, collection, loose));
    titles
}

/// The directory directly beneath `collection_dir` that contains `video`
/// somewhere below it, or `None` when the file sits in `collection_dir` itself.
fn title_folder_for(collection_dir: &Path, video: &Path) -> Option<PathBuf> {
    let rel = video.strip_prefix(collection_dir).ok()?;
    let first = rel.components().next()?;
    let candidate = collection_dir.join(first.as_os_str());
    candidate.is_dir().then_some(candidate)
}

/// A folder-backed title: distinct content within it becomes parts, and names
/// differing only by quality tokens become variants of a part.
fn title_from_folder(collection: &str, folder: PathBuf, files: Vec<PathBuf>) -> Title {
    let groups = group_by_content_key(&files);
    let labels = derive_part_labels(&groups.keys().cloned().collect::<Vec<_>>());

    let parts = groups
        .into_iter()
        .zip(labels)
        .map(|((_, paths), label)| Part {
            label,
            variants: paths.into_iter().map(new_media_file).collect(),
        })
        .collect();

    let name = file_name_of(&folder);
    Title {
        id: title_id(&folder),
        name,
        collection: collection.to_string(),
        cover: find_cover(&folder),
        folder,
        parts,
    }
}

/// Loose files become one title per distinct content key, so two files that
/// differ only by `2k` / `4k h265` collapse into a single entry.
fn titles_from_loose_files(dir: &Path, collection: &str, files: Vec<PathBuf>) -> Vec<Title> {
    group_by_content_key(&files)
        .into_iter()
        .map(|(key, paths)| {
            // Name from the shortest variant's stem: the shortest is the one
            // carrying fewest quality suffixes.
            let name = paths
                .iter()
                .min_by_key(|p| file_stem_of(p).len())
                .map(|p| file_stem_of(p))
                .unwrap_or_else(|| key.clone());
            Title {
                id: title_id(&dir.join(&key)),
                name: display_name_from_stem(&name),
                collection: collection.to_string(),
                folder: dir.to_path_buf(),
                cover: paths.first().and_then(|p| find_cover_beside(p)),
                parts: vec![Part {
                    label: String::new(),
                    variants: paths.into_iter().map(new_media_file).collect(),
                }],
            }
        })
        .collect()
}

/// Groups paths by a name key with quality tokens removed, preserving a stable
/// order so parts come out in a sensible sequence.
fn group_by_content_key(files: &[PathBuf]) -> BTreeMap<String, Vec<PathBuf>> {
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in files {
        groups
            .entry(content_key(&file_stem_of(path)))
            .or_default()
            .push(path.clone());
    }
    groups
}

/// Normalises a file stem so that quality variants of the same content collide.
///
/// `TITLE-001_A_2160p_4K_LR_180` and `TITLE-001_A_original_LR_180` both reduce
/// to `title 001 a lr 180`, while `_B` stays distinct because the disc letter is
/// content, not quality.
pub fn content_key(stem: &str) -> String {
    let mut s = stem.to_ascii_lowercase();

    // Resolution markers like 2160p or 3630p, and explicit WxH pairs.
    s = strip_resolution_markers(&s);

    // Separators to spaces so token matching is uniform.
    s = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { ' ' })
        .collect();

    s.split_whitespace()
        .filter(|t| !is_quality_token(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a token describes encoding quality rather than content.
///
/// Covers the fixed list plus any `<number>fps`, so that 60fps and 90fps cuts of
/// the same scene collapse together instead of becoming separate titles.
fn is_quality_token(token: &str) -> bool {
    if QUALITY_TOKENS.contains(&token) {
        return true;
    }
    if let Some(digits) = token.strip_suffix("fps") {
        return !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    }
    false
}

fn strip_resolution_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        // A run of digits, optionally followed by `p` or `x<digits>`.
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits: String = bytes[start..i].iter().collect();
            let is_resolution = digits.len() >= 3
                && (i < bytes.len() && bytes[i] == 'p'
                    || digits.parse::<u32>().map(|n| n >= 480).unwrap_or(false)
                        && i < bytes.len()
                        && bytes[i] == 'x');
            if is_resolution {
                // Consume the `p`, or the `x<digits>` tail.
                if bytes[i] == 'p' {
                    i += 1;
                } else {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                out.push(' ');
                continue;
            }
            out.push_str(&digits);
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Builds a display name from a raw file stem, dropping layout tags and quality
/// markers while preserving the original wording and capitalisation.
///
/// `42. Scene Name A1 VR 4k h265` becomes `42. Scene Name A1 VR`.
/// This runs on the stem rather than the normalised key so that punctuation and
/// case survive into the browser.
fn display_name_from_stem(stem: &str) -> String {
    let stripped = tidy_display_name(stem);
    let kept: Vec<&str> = stripped
        .split_whitespace()
        .filter(|t| {
            let lower = t.to_ascii_lowercase();
            let bare = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            !is_quality_token(bare) && !is_resolution_token(bare)
        })
        .collect();
    let name = kept.join(" ");
    let name = name
        .trim_matches(|c: char| c == '_' || c == '-' || c.is_whitespace())
        .to_string();
    if name.is_empty() {
        stripped
    } else {
        name
    }
}

/// Whether a token is a bare resolution marker such as `2160p` or `3840x2160`.
fn is_resolution_token(token: &str) -> bool {
    if let Some(digits) = token.strip_suffix('p') {
        return digits.len() >= 3 && digits.chars().all(|c| c.is_ascii_digit());
    }
    if let Some((w, h)) = token.split_once('x') {
        return !w.is_empty()
            && !h.is_empty()
            && w.chars().all(|c| c.is_ascii_digit())
            && h.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Labels parts by whatever distinguishes them, dropping both the prefix and the
/// suffix they all share.
///
/// The keys for one release are typically `title 001 a lr 180`, `…b…`, `…c…`:
/// they share a prefix (the catalogue number) *and* a suffix (the layout tag),
/// and only the middle carries meaning. Stripping both yields "a", "b", "c".
fn derive_part_labels(keys: &[String]) -> Vec<String> {
    if keys.len() <= 1 {
        return vec![String::new(); keys.len()];
    }
    let words: Vec<Vec<&str>> = keys.iter().map(|k| k.split_whitespace().collect()).collect();
    let shortest = words.iter().map(|w| w.len()).min().unwrap_or(0);

    let prefix = common_affix(&words, shortest, |w, i| w[i]);
    // Leave at least one word after the prefix, so a suffix match cannot eat the
    // entire label when keys differ only in their prefix.
    let remaining = shortest - prefix;
    let suffix = common_affix(&words, remaining.saturating_sub(1), |w, i| w[w.len() - 1 - i]);

    words
        .iter()
        .map(|w| {
            let rest = &w[prefix..w.len() - suffix];
            if rest.is_empty() {
                w.join(" ")
            } else {
                tidy_display_name(&rest.join(" "))
            }
        })
        .collect()
}

/// Length of the longest run, up to `limit`, where `pick` returns the same word
/// for every key. Used for both the leading and trailing shared runs.
fn common_affix<'a>(
    words: &[Vec<&'a str>],
    limit: usize,
    pick: impl Fn(&Vec<&'a str>, usize) -> &'a str,
) -> usize {
    let mut n = 0;
    while n < limit {
        let first = pick(&words[0], n);
        if !words[1..].iter().all(|w| pick(w, n) == first) {
            break;
        }
        n += 1;
    }
    n
}

/// Trims layout tags and surrounding punctuation from a name meant for display.
fn tidy_display_name(name: &str) -> String {
    let mut s = name.trim().to_string();
    for tag in [
        "_LR_180", "_TB_180", "_SBS_180", "_LR_360", "_TB_360", "_MONO_360",
    ] {
        if let Some(pos) = s.to_ascii_uppercase().find(tag) {
            s.truncate(pos);
        }
    }
    s.trim_matches(|c: char| c == '_' || c == '-' || c == '.' || c.is_whitespace())
        .to_string()
}

fn new_media_file(path: PathBuf) -> MediaFile {
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    MediaFile {
        path,
        size_bytes,
        width: 0,
        height: 0,
        duration_secs: 0.0,
        codec: String::new(),
        layout: Layout::FLAT,
    }
}

/// Copies cached probe data onto a media file.
fn apply_probe(file: &mut MediaFile, cache: &ProbeCache) {
    let (size, mtime) = stat(&file.path);
    if let Some(entry) = cache.get(&file.path, size, mtime) {
        file.width = entry.width;
        file.height = entry.height;
        file.duration_secs = entry.duration_secs;
        file.codec = entry.codec.clone();
        file.layout = entry.layout;
    }
}

fn probe_from_cache<'a>(path: &Path, cache: &'a ProbeCache) -> Option<&'a ProbeEntry> {
    let (size, mtime) = stat(path);
    cache.get(path, size, mtime)
}

fn stat(path: &Path) -> (u64, i64) {
    match std::fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (m.len(), mtime)
        }
        Err(_) => (0, 0),
    }
}

/// Runs ffprobe over many files at once. Bounded concurrency keeps the NFS
/// share responsive while still hiding per-file latency.
fn probe_all(paths: &[PathBuf]) -> Vec<(PathBuf, ProbeEntry)> {
    const PARALLELISM: usize = 12;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Mutex<Vec<(PathBuf, ProbeEntry)>> = Mutex::new(Vec::with_capacity(paths.len()));

    std::thread::scope(|scope| {
        for _ in 0..PARALLELISM.min(paths.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(path) = paths.get(i) else { return };
                if let Some(entry) = probe_one(path) {
                    results.lock().unwrap().push((path.clone(), entry));
                }
            });
        }
    });

    results.into_inner().unwrap()
}

fn probe_one(path: &Path) -> Option<ProbeEntry> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height,codec_name:format=duration",
            "-of", "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("warning: ffprobe failed for {}", path.display());
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut width = 0u32;
    let mut height = 0u32;
    let mut duration = 0.0f64;
    let mut codec = String::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "width" => width = value.trim().parse().unwrap_or(0),
            "height" => height = value.trim().parse().unwrap_or(0),
            "duration" => duration = value.trim().parse().unwrap_or(0.0),
            "codec_name" => codec = value.trim().to_string(),
            _ => {}
        }
    }
    if width == 0 || height == 0 {
        return None;
    }

    let file_name = path.file_name()?.to_str()?;
    let (size_bytes, mtime_secs) = stat(path);
    Some(ProbeEntry {
        size_bytes,
        mtime_secs,
        width,
        height,
        duration_secs: duration,
        codec,
        layout: projection::detect(file_name, width, height),
    })
}

/// Every video file at or below `dir`.
fn collect_videos(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| is_video(p))
        .collect()
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Looks for hand-placed cover art in a title folder.
///
/// A bare `.cover` file wins outright: it is unambiguous, cannot be confused
/// with artwork that happens to live alongside the media, and stays out of the
/// way in listings. Its image format is detected from the file's contents, so
/// the extension it lacks does not matter.
fn find_cover(folder: &Path) -> Option<PathBuf> {
    let dot_cover = folder.join(COVER_MARKER);
    if dot_cover.is_file() {
        return Some(dot_cover);
    }
    for name in COVER_NAMES {
        for ext in COVER_EXTENSIONS {
            let candidate = folder.join(format!("{name}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Cover art named after a specific file, e.g. `scene.mp4` + `scene.cover`,
/// or `scene.jpg`. Used for titles built from loose files, where a folder-wide
/// cover would apply to the wrong thing.
fn find_cover_beside(video: &Path) -> Option<PathBuf> {
    let dot_cover = video.with_extension(COVER_MARKER.trim_start_matches('.'));
    if dot_cover.is_file() {
        return Some(dot_cover);
    }
    for ext in COVER_EXTENSIONS {
        let candidate = video.with_extension(ext);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Stable identifier for a title, used to key cover art and viewer overrides.
/// Derived from the path so it survives restarts, hashed so it is safe as a
/// file name.
fn title_id(path: &Path) -> String {
    blake3::hash(path.as_os_str().as_encoded_bytes())
        .to_hex()
        .chars()
        .take(16)
        .collect()
}

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

fn file_stem_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Orders names so that `Part 2` sorts before `Part 10`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na: String = take_digits(&mut ai);
                let nb: String = take_digits(&mut bi);
                let ord = na
                    .trim_start_matches('0')
                    .len()
                    .cmp(&nb.trim_start_matches('0').len())
                    .then_with(|| na.cmp(&nb));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while it.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        s.push(it.next().unwrap());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_variants_collapse_to_one_key() {
        // Two encodes of the same scene.
        let a = content_key("42. Scene Name A1 VR 2k");
        let b = content_key("42. Scene Name A1 VR 4k h265");
        assert_eq!(a, b, "quality suffixes should not split a title");
    }

    #[test]
    fn disc_letters_stay_distinct() {
        let a = content_key("TITLE-001_A_2160p_4K_LR_180");
        let b = content_key("TITLE-001_B_2160p_4K_LR_180");
        assert_ne!(a, b, "disc letters are content, not quality");
    }

    #[test]
    fn resolution_markers_are_stripped() {
        let a = content_key("scene_2160p_LR_180");
        let b = content_key("scene_3630p_LR_180");
        assert_eq!(a, b);
    }

    #[test]
    fn part_labels_drop_the_shared_prefix() {
        let keys = vec![
            "title 001 a lr 180".to_string(),
            "title 001 b lr 180".to_string(),
            "title 001 c lr 180".to_string(),
        ];
        let labels = derive_part_labels(&keys);
        assert_eq!(labels, vec!["a", "b", "c"]);
    }

    #[test]
    fn natural_order_sorts_numbers_numerically() {
        let mut v = vec!["Title 10", "Title 2", "Title 1"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["Title 1", "Title 2", "Title 10"]);
    }
}

#[cfg(test)]
mod cover_tests {
    use super::*;

    /// Scratch directory unique to each test, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "vrmp-test-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creating temp dir");
            TempDir(path)
        }
        fn touch(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, b"x").expect("writing file");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dot_cover_is_found_in_a_title_folder() {
        let dir = TempDir::new("dotcover");
        let expected = dir.touch(".cover");
        assert_eq!(find_cover(&dir.0), Some(expected));
    }

    #[test]
    fn dot_cover_wins_over_other_names() {
        let dir = TempDir::new("precedence");
        dir.touch("folder.jpg");
        dir.touch("poster.png");
        let expected = dir.touch(".cover");
        assert_eq!(
            find_cover(&dir.0),
            Some(expected),
            "an explicit .cover should beat incidental artwork"
        );
    }

    #[test]
    fn named_covers_still_work_without_a_dot_cover() {
        let dir = TempDir::new("named");
        let expected = dir.touch("cover.jpg");
        assert_eq!(find_cover(&dir.0), Some(expected));
    }

    #[test]
    fn no_cover_reports_none() {
        let dir = TempDir::new("empty");
        dir.touch("readme.txt");
        assert_eq!(find_cover(&dir.0), None);
    }

    #[test]
    fn loose_files_match_a_sibling_dot_cover() {
        let dir = TempDir::new("beside");
        let video = dir.touch("scene.mp4");
        let expected = dir.touch("scene.cover");
        assert_eq!(find_cover_beside(&video), Some(expected));
    }

    #[test]
    fn a_sibling_cover_belongs_only_to_its_own_video() {
        let dir = TempDir::new("sibling");
        let a = dir.touch("scene_a.mp4");
        let b = dir.touch("scene_b.mp4");
        dir.touch("scene_a.cover");
        assert!(find_cover_beside(&a).is_some());
        assert_eq!(
            find_cover_beside(&b),
            None,
            "one video's cover must not be claimed by another"
        );
    }
}
