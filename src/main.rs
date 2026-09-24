//! vrmp — a VR media player for high-resolution 180/360 video.

mod app;
mod config;
mod mpv;
mod ui;
mod xr;
mod library;

use std::path::PathBuf;

use anyhow::{Context, Result};

use config::{Config, Paths};
use library::index::{Overrides, ProbeCache};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("play");

    // All state lives beside the project, never in the user's home or the
    // library itself.
    let data_dir = std::env::var_os("VRMP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));
    let paths = Paths::new(data_dir);
    paths.ensure_dirs()?;

    let mut cfg = Config::load(&paths.config_file)?;

    match command {
        "scan" => cmd_scan(&paths, &cfg, args.get(2..).unwrap_or(&[])),
        "add-root" => cmd_add_root(&paths, &mut cfg, args.get(2..).unwrap_or(&[])),
        "covers" => cmd_covers(&paths, &cfg),
        "play" => app::run(paths, cfg),
        "doctor" => cmd_doctor(&paths, &cfg),
        "config" => cmd_config(&paths, &cfg),
        "preview" => cmd_preview(&paths, &cfg, args.get(2..).unwrap_or(&[])),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "vrmp — VR media player

USAGE:
    vrmp play               start the player in the headset (default)
    vrmp add-root <DIR>     register a directory to scan for media
    vrmp scan [--verbose]   scan libraries and report what was found
    vrmp covers             generate any missing cover art
    vrmp doctor             check the runtime, headset, and decode setup
    vrmp config             show every setting and write them all to the file
    vrmp preview [FILE]     render the library view to a PNG, no headset needed
    vrmp help

State is kept in ./data (override with VRMP_DATA_DIR). Library directories are
only ever read from."
    );
}

fn cmd_add_root(paths: &Paths, cfg: &mut Config, args: &[String]) -> Result<()> {
    let Some(dir) = args.first() else {
        anyhow::bail!("usage: vrmp add-root <DIR>");
    };
    let dir = PathBuf::from(dir)
        .canonicalize()
        .with_context(|| format!("resolving {dir}"))?;
    if !dir.is_dir() {
        anyhow::bail!("{} is not a directory", dir.display());
    }
    if cfg.roots.contains(&dir) {
        println!("already registered: {}", dir.display());
        return Ok(());
    }
    cfg.roots.push(dir.clone());
    cfg.save(&paths.config_file)?;
    println!("added library root: {}", dir.display());
    Ok(())
}

fn cmd_scan(paths: &Paths, cfg: &Config, args: &[String]) -> Result<()> {
    if cfg.roots.is_empty() {
        anyhow::bail!("no library roots configured — run: vrmp add-root <DIR>");
    }
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");

    let mut cache = ProbeCache::load(&paths.probe_cache);
    let started = std::time::Instant::now();
    let mut report = library::scan::scan(&cfg.roots, &mut cache)?;
    cache.save(&paths.probe_cache)?;

    // Apply the viewer's corrections before reporting, so the summary describes
    // what will actually play rather than what detection guessed. Without this
    // the report claims titles are "guessed" that were fixed long ago.
    let overrides = Overrides::load(&paths.overrides);
    library::index::apply_overrides(&mut report.titles, &overrides);

    println!(
        "scanned {} files into {} titles in {:.1}s ({} newly probed)",
        report.files_seen,
        report.titles.len(),
        started.elapsed().as_secs_f32(),
        report.files_probed,
    );

    // Summarise what was detected, since layout detection is the part most
    // likely to be wrong and worth eyeballing.
    let mut by_collection: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut guessed = 0usize;
    let mut flat = 0usize;
    for t in &report.titles {
        *by_collection.entry(t.collection.as_str()).or_default() += 1;
        if t.layout().confidence == library::Confidence::Guessed {
            guessed += 1;
        }
        if !t.is_vr() {
            flat += 1;
        }
    }

    println!("\ncollections:");
    for (name, count) in &by_collection {
        println!("  {count:>5}  {name}");
    }
    println!(
        "\n{guessed} titles have a guessed layout, {flat} are flat (non-VR), \
         {} overrides stored",
        overrides.layouts.len()
    );

    // Count how covers are being sourced, so a hand-placed `.cover` that is not
    // being picked up is visible here rather than only in the headset.
    let hand_placed = report.titles.iter().filter(|t| t.cover.is_some()).count();
    println!(
        "{hand_placed} titles use hand-placed cover art, {} fall back to a \
         generated frame",
        report.titles.len() - hand_placed
    );

    if verbose {
        println!("\ntitles:");
        for t in &report.titles {
            let l = t.layout();
            let f = t.parts[t.default_part_index()].best();
            let cover = match &t.cover {
                Some(path) => format!(
                    " cover={}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                ),
                None => String::new(),
            };
            println!(
                "  [{}] {} — {} part(s){}, {}x{} {} {:?}/{:?} ({:?}) {}{}",
                t.collection,
                t.name,
                t.parts.len(),
                if t.is_mixed() { " mixed VR/flat" } else { "" },
                f.width,
                f.height,
                f.codec,
                l.projection,
                l.stereo,
                l.confidence,
                format_duration(t.total_duration_secs()),
                cover,
            );
        }
    }

    Ok(())
}

fn cmd_covers(paths: &Paths, cfg: &Config) -> Result<()> {
    if cfg.roots.is_empty() {
        anyhow::bail!("no library roots configured — run: vrmp add-root <DIR>");
    }
    let mut cache = ProbeCache::load(&paths.probe_cache);
    let report = library::scan::scan(&cfg.roots, &mut cache)?;
    cache.save(&paths.probe_cache)?;

    // The same worker the player uses, waited on rather than left in the
    // background. Sharing the implementation means this command and a normal
    // launch produce identical results and write the same failure log, instead
    // of two code paths that can drift apart.
    let progress =
        library::thumbs::spawn_generator(&paths.covers_dir, &paths.cover_log, &report.titles);

    if progress.total == 0 {
        println!("every title already has a cover");
        return Ok(());
    }

    while progress.in_progress() {
        eprint!("\r  {}/{} …", progress.done(), progress.total);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let failures = progress.failures();
    println!(
        "\rgenerated {} covers, {failures} failed",
        progress.total - failures
    );
    if failures > 0 {
        println!("see {}", progress.log_path().display());
    }
    Ok(())
}

/// Formats a duration for display, keeping short loops legible instead of
/// rounding a 17-second clip to "0min".
fn format_duration(secs: f64) -> String {
    if secs < 1.0 {
        "unknown".to_string()
    } else if secs < 90.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{:.0}min", secs / 60.0)
    } else {
        format!("{}h{:02.0}m", (secs / 3600.0) as u32, (secs % 3600.0) / 60.0)
    }
}

/// Reports on the pieces the player depends on, so a headset that will not
/// start can be diagnosed without going into VR.
fn cmd_doctor(paths: &Paths, cfg: &Config) -> Result<()> {
    println!("vrmp doctor\n");

    println!("library roots:");
    if cfg.roots.is_empty() {
        println!("  (none)  — run: vrmp add-root <DIR>");
    }
    for root in &cfg.roots {
        let state = if root.is_dir() { "ok" } else { "MISSING" };
        println!("  [{state}] {}", root.display());
    }

    println!("\nstate directory: {}", paths.data_dir.display());
    let covers = std::fs::read_dir(&paths.covers_dir)
        .map(|d| d.count())
        .unwrap_or(0);
    println!("  covers generated: {covers}");

    println!("\nexternal tools:");
    for tool in ["ffmpeg", "ffprobe"] {
        match std::process::Command::new(tool).arg("-version").output() {
            Ok(out) if out.status.success() => {
                let first = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                println!("  [ok] {first}");
            }
            _ => println!("  [MISSING] {tool} — cover art will not be generated"),
        }
    }

    println!("\nOpenXR:");
    match std::env::var("XR_RUNTIME_JSON") {
        Ok(path) => println!("  XR_RUNTIME_JSON={path}"),
        Err(_) => println!("  XR_RUNTIME_JSON is unset; the loader will use the system default"),
    }

    // Enumerating extensions needs only the loader, not a running session, so
    // this works even with the headset switched off.
    let entry = openxr::Entry::linked();
    match entry.enumerate_extensions() {
        Err(e) => {
            println!("  [FAIL] no runtime found: {e}");
            println!("         start an OpenXR runtime (WiVRn, Monado, Envision, …),");
            println!("         or set XR_RUNTIME_JSON to its manifest");
        }
        Ok(ext) => {
            println!("  [ok] runtime responded");
            let required = [
                ("XR_KHR_opengl_enable", ext.khr_opengl_enable, true),
                (
                    "XR_KHR_composition_layer_equirect2",
                    ext.khr_composition_layer_equirect2,
                    true,
                ),
                (
                    "XR_KHR_composition_layer_cylinder",
                    ext.khr_composition_layer_cylinder,
                    false,
                ),
            ];
            for (name, present, needed) in required {
                let tag = match (present, needed) {
                    (true, _) => "ok",
                    (false, true) => "FAIL",
                    (false, false) => "warn",
                };
                println!("  [{tag}] {name}");
            }
            if !ext.khr_opengl_enable {
                println!("\n  This runtime cannot render OpenGL, which this player requires.");
            }
            if !ext.khr_composition_layer_equirect2 {
                println!("\n  Without equirect2 layers, 180/360 video cannot be projected.");
            }
        }
    }

    Ok(())
}

/// Renders the library view to a PNG without a headset.
///
/// This exists for two reasons: the interface can be looked at and iterated on
/// without putting the headset on, and it exercises the whole UI path — GL
/// context, egui, cover loading, framebuffer rendering — which is otherwise only
/// reachable inside a live OpenXR session.
fn cmd_preview(paths: &Paths, cfg: &Config, args: &[String]) -> Result<()> {
    use glow::HasContext as _;

    let out = args
        .first()
        .cloned()
        .unwrap_or_else(|| "library-preview.png".to_string());

    let mut cache = ProbeCache::load(&paths.probe_cache);
    let report = library::scan::scan(&cfg.roots, &mut cache)?;
    cache.save(&paths.probe_cache)?;
    let overrides = Overrides::load(&paths.overrides);
    let mut titles = report.titles;
    library::index::apply_overrides(&mut titles, &overrides);

    let gl_ctx = xr::gl_context::GlContext::create()
        .context("creating an OpenGL context (needs a display)")?;
    let glow_ctx = std::sync::Arc::new(gl_ctx.glow());

    let (w, h) = (ui::UI_WIDTH, ui::UI_HEIGHT);

    // An ordinary offscreen target standing in for the OpenXR swapchain image.
    let (fbo, pixels) = unsafe {
        let texture = glow_ctx
            .create_texture()
            .map_err(|e| anyhow::anyhow!("creating texture: {e}"))?;
        glow_ctx.bind_texture(glow::TEXTURE_2D, Some(texture));
        glow_ctx.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::SRGB8_ALPHA8 as i32,
            w as i32,
            h as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        let fbo = glow_ctx
            .create_framebuffer()
            .map_err(|e| anyhow::anyhow!("creating framebuffer: {e}"))?;
        glow_ctx.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        glow_ctx.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        if glow_ctx.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
            anyhow::bail!("offscreen framebuffer is incomplete");
        }
        (fbo.0.get(), vec![0u8; (w * h * 4) as usize])
    };

    let mut ui = ui::Ui::new(&glow_ctx)?;
    let playback = mpv::PlaybackState::default();

    // Two passes: egui lays out on the first frame and only has correct sizes
    // and loaded cover textures on the second.
    let mut pixels = pixels;
    for _ in 0..2 {
        ui.run(
            &titles,
            &paths.covers_dir,
            &playback,
            // Nothing generates during a headless render.
            None,
            false,
            0.0,
            // No keyboard: this render is headless and takes no input.
            &[],
            0.0,
            fbo,
            (w, h),
            // Nothing is generating during a headless render.
            // Nothing generates during a headless render.
            None,
        )?;
    }

    unsafe {
        glow_ctx.bind_framebuffer(glow::FRAMEBUFFER, std::num::NonZeroU32::new(fbo).map(glow::NativeFramebuffer));
        glow_ctx.read_pixels(
            0,
            0,
            w as i32,
            h as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut pixels)),
        );
    }

    // GL reads bottom-up; PNG is top-down.
    let row = (w * 4) as usize;
    let mut flipped = vec![0u8; pixels.len()];
    for y in 0..h as usize {
        let src = y * row;
        let dst = (h as usize - 1 - y) * row;
        flipped[dst..dst + row].copy_from_slice(&pixels[src..src + row]);
    }

    // The panel is transparent where egui painted nothing, which reads as
    // checkerboard in most viewers. Composite onto the dark ground the
    // compositor would show behind it.
    const GROUND: f32 = 10.0;
    let (pixels_rgba, _) = flipped.as_chunks_mut::<4>();
    for px in pixels_rgba {
        let (rgb, alpha) = px.split_at_mut(3);
        let a = alpha[0] as f32 / 255.0;
        for channel in rgb {
            *channel = (*channel as f32 * a + GROUND * (1.0 - a)) as u8;
        }
        alpha[0] = 255;
    }

    image::RgbaImage::from_raw(w, h, flipped)
        .context("assembling the preview image")?
        .save(&out)
        .with_context(|| format!("writing {out}"))?;

    println!("wrote {out} ({w}x{h}, {} titles)", titles.len());
    Ok(())
}

/// Prints the effective configuration and writes it out in full.
///
/// Options default when absent from the file, which is convenient but leaves
/// them undiscoverable — you cannot edit a setting whose name you have never
/// seen. This materialises every key at its current value, so the file becomes
/// a complete record of what can be changed.
fn cmd_config(paths: &Paths, cfg: &Config) -> Result<()> {
    cfg.save(&paths.config_file)?;
    println!("{}\n", paths.config_file.display());
    println!("{}", serde_json::to_string_pretty(cfg)?);
    println!(
        "\nroots           directories scanned for media\n\
         ui_distance_m   metres from you to the interface panel\n\
         ui_angle_deg    horizontal arc it spans; the single size control\n\
         mirror_window   mirror one eye to the desktop window\n\
         mirror_width    that window's width in pixels\n\
         cache_secs      seconds of media buffered ahead\n\
         cache_max_mib   ceiling on that buffer"
    );
    Ok(())
}
