// Media tools — a Lintel feature. The contract is docs/ARCHITECTURE.md §2: nothing
// runs before `start`, everything is gone after `stop`, and every command of
// the feature arrives through `call`.
//
// Four tools on files the user chose or dropped: compress a video, make a GIF, convert / resize /
// watermark pictures, read the text in a picture. Everything runs on Windows' own components
// (Media Foundation, WIC, the text recogniser of svc-capture): nothing is downloaded, and a job
// exists only while the user's click started it. Results are written beside the input under a new
// name, never over it, and the page hands them to the shelf.
// Sizing plans are ported from reference/vorssaint-utils/.../Media/MediaSupport.swift.

pub mod gif;
pub mod img;
pub mod video;

use notch_core::{Ctx, Feature, Group, Manifest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const ID: &str = "media-tools";

static MANIFEST: Manifest = Manifest {
    id: ID,
    name: "Media tools",
    group: Group::Tools,
    default_on: false,
    uses: &[],
    shortcuts: &[],
    tray: &[],
    defaults: r#"{"videoSizing":"resolution","videoMaxDim":1280,"videoCompression":"medium","videoFps":30,"videoKeepAudio":true,"videoTargetMb":20,
"gifSizing":"resolution","gifFps":12,"gifWidth":720,"gifLoops":true,"gifTargetMb":10,
"imgFormat":"jpeg","imgQuality":0.72,"imgResize":"max","imgMax":1600,"imgW":1600,"imgH":1200,"imgFit":"fit","imgBg":"white","imgSubfolder":false,"imgPattern":"",
"wmText":"","wmPos":3,"wmOpacity":0.45,"wmScale":0.18,"wmMargin":32}"#,
};

#[derive(Default)]
pub struct MediaTools {
    /// The running job's cancel flag; one job at a time.
    job: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

const VIDEO: &[&str] = &["mp4", "mov", "m4v", "wmv", "avi", "mkv", "webm", "mpg", "mpeg", "3gp"];

fn ext(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn path_arg(args: &Value) -> Result<PathBuf, String> {
    let p = PathBuf::from(args["path"].as_str().ok_or("no file")?);
    if p.is_file() { Ok(p) } else { Err("That file is not there.".into()) }
}

fn num(s: &Value, k: &str, d: f64) -> f64 {
    s[k].as_f64().unwrap_or(d)
}

impl MediaTools {
    /// Runs `work` on its own thread; its answer or its error reaches the page as `done` or `failed`.
    fn spawn(&self, cx: &Ctx, kind: &'static str, work: impl FnOnce(&AtomicBool, video::Progress) -> Result<Value, String> + Send + 'static) -> Result<Value, String> {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let mut slot = self.job.lock().unwrap();
            if slot.is_some() {
                return Err("A job is already running.".into());
            }
            *slot = Some(flag.clone());
        }
        let cx = cx.clone();
        let slot = self.job.clone();
        std::thread::spawn(move || {
            img::com();
            let c = cx.clone();
            let progress: video::Progress = Arc::new(move |f| c.emit(ID, "progress", json!({ "kind": kind, "fraction": f.clamp(0.0, 1.0) })));
            let r = work(&flag, progress);
            *slot.lock().unwrap() = None;
            match r {
                Ok(mut v) => {
                    v["kind"] = json!(kind);
                    cx.emit(ID, "done", v)
                }
                Err(m) => cx.emit(ID, "failed", json!({ "kind": kind, "message": m, "cancelled": flag.load(Ordering::Relaxed) })),
            }
        });
        Ok(json!({}))
    }
}

impl Feature for MediaTools {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn start(&self, _cx: &Ctx) -> Result<(), String> {
        Ok(())
    }

    fn stop(&self, _cx: &Ctx) {
        if let Some(flag) = self.job.lock().unwrap().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    fn call(&self, cx: &Ctx, method: &str, args: Value) -> Result<Value, String> {
        let s = cx.settings(ID);
        match method {
            "status" => Ok(json!({ "ocr": svc_capture::ocr_available(), "busy": self.job.lock().unwrap().is_some() })),
            "pick" => Ok(json!(svc_shell::pick_files("Choose files").iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>())),
            // What the island's drop gave: the page decides what to do with the files.
            "dropped" => {
                cx.emit(ID, "dropped", args["paths"].clone());
                Ok(Value::Null)
            }
            "cancel" => {
                if let Some(flag) = self.job.lock().unwrap().as_ref() {
                    flag.store(true, Ordering::Relaxed);
                }
                Ok(Value::Null)
            }
            "info" => {
                let p = path_arg(&args)?;
                if VIDEO.contains(&ext(&p).as_str()) {
                    let i = video::info(&p)?;
                    Ok(json!({ "kind": "video", "w": i.w, "h": i.h, "secs": i.secs, "fps": i.fps, "audio": i.audio, "bytes": p.metadata().map(|m| m.len()).unwrap_or(0) }))
                } else {
                    let (w, h) = img::size_of_file(&p)?;
                    Ok(json!({ "kind": "image", "w": w, "h": h, "bytes": p.metadata().map(|m| m.len()).unwrap_or(0) }))
                }
            }
            "compress" => {
                let p = path_arg(&args)?;
                let sized = s["videoSizing"] == "size";
                let o = video::Opts {
                    max_dim: num(&s, "videoMaxDim", 1280.0) as u32,
                    bpp: match s["videoCompression"].as_str() {
                        Some("high") => 0.04,
                        Some("low") => 0.12,
                        _ => 0.07,
                    },
                    fps: num(&s, "videoFps", 30.0),
                    keep_audio: s["videoKeepAudio"].as_bool().unwrap_or(true),
                    target_mb: sized.then(|| num(&s, "videoTargetMb", 20.0) as u32),
                    start: num(&args, "start", 0.0),
                    end: num(&args, "end", 0.0),
                };
                self.spawn(cx, "compress", move |cancel, progress| {
                    let out = video::compress(&p, &o, cancel, progress)?;
                    Ok(json!({ "path": out.to_string_lossy(), "from": p.metadata().map(|m| m.len()).unwrap_or(0), "to": out.metadata().map(|m| m.len()).unwrap_or(0) }))
                })
            }
            "audio" => {
                let p = path_arg(&args)?;
                let trim = (num(&args, "start", 0.0), num(&args, "end", 0.0));
                self.spawn(cx, "audio", move |cancel, progress| {
                    let out = video::audio(&p, trim, cancel, progress)?;
                    Ok(json!({ "path": out.to_string_lossy(), "from": p.metadata().map(|m| m.len()).unwrap_or(0), "to": out.metadata().map(|m| m.len()).unwrap_or(0) }))
                })
            }
            "gif" => {
                let p = path_arg(&args)?;
                let o = gif::Opts {
                    fps: num(&s, "gifFps", 12.0),
                    width: num(&s, "gifWidth", 720.0) as u32,
                    loops: s["gifLoops"].as_bool().unwrap_or(true),
                    target_mb: (s["gifSizing"] == "size").then(|| num(&s, "gifTargetMb", 10.0) as u32),
                    start: num(&args, "start", 0.0),
                    end: num(&args, "end", 0.0),
                };
                self.spawn(cx, "gif", move |cancel, progress| {
                    let out = gif::make(&p, &o, cancel, progress)?;
                    Ok(json!({ "path": out.to_string_lossy(), "from": p.metadata().map(|m| m.len()).unwrap_or(0), "to": out.metadata().map(|m| m.len()).unwrap_or(0) }))
                })
            }
            "images" => {
                let paths: Vec<PathBuf> = args["paths"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default();
                if paths.is_empty() {
                    return Err("There were no pictures.".into());
                }
                self.spawn(cx, "images", move |cancel, progress| {
                    let mut saved = vec![];
                    let mut failed = vec![];
                    for (i, p) in paths.iter().enumerate() {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        match convert(p, &s, i + 1) {
                            Ok(out) => saved.push(out.to_string_lossy().to_string()),
                            Err(m) => failed.push(format!("{}: {m}", p.file_name().unwrap_or_default().to_string_lossy())),
                        }
                        progress((i + 1) as f64 / paths.len() as f64);
                    }
                    if saved.is_empty() && !failed.is_empty() {
                        return Err(failed.join("\n"));
                    }
                    Ok(json!({ "paths": saved, "failed": failed }))
                })
            }
            "text" => {
                let p = path_arg(&args)?;
                img::com();
                Ok(json!({ "text": text_of(&p)? }))
            }
            _ => Err("unknown method".into()),
        }
    }

    fn strings(&self) -> &'static str {
        include_str!("../../../src/features/media-tools/strings.json")
    }
}

/// The text in a picture file, lines from the top.
pub fn text_of(path: &Path) -> Result<String, String> {
    let mut image = img::load(path, &|w, h| (w, h))?;
    img::flatten(&mut image, [255, 255, 255]);
    Ok(svc_capture::ocr(&image)?.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"))
}

/// The size a picture is decoded at, from the resize settings.
fn target(s: &Value, w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w as f64, h as f64);
    let by = |k: f64| (((w * k).round() as u32).max(1), ((h * k).round() as u32).max(1));
    match s["imgResize"].as_str() {
        Some("max") => by((num(s, "imgMax", 1600.0) / w.max(h)).min(1.0)),
        Some("width") => by((num(s, "imgW", 1600.0) / w).min(1.0)),
        Some("height") => by((num(s, "imgH", 1200.0) / h).min(1.0)),
        Some("exact") => {
            let (tw, th) = (num(s, "imgW", 1600.0), num(s, "imgH", 1200.0));
            match s["imgFit"].as_str() {
                Some("stretch") => (tw as u32, th as u32),
                Some("fill") => by((tw / w).max(th / h)),
                _ => by((tw / w).min(th / h)),
            }
        }
        _ => (w as u32, h as u32),
    }
}

/// `{name} {date} {time} {width} {height} {format} {index}` (`{index:03}` pads).
fn named(pattern: &str, name: &str, pic: &svc_capture::Image, fmt: img::Format, index: usize) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // debt: UTC date, no zone database; local time would need GetLocalTime.
    let (days, rem) = (secs / 86400, secs % 86400);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let (y, doy) = (yoe + era * 400, doe - (365 * yoe + yoe / 4 - yoe / 100));
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = if m <= 2 { y + 1 } else { y };
    let mut out = pattern
        .replace("{name}", name)
        .replace("{date}", &format!("{y}-{m:02}-{d:02}"))
        .replace("{time}", &format!("{:02}-{:02}-{:02}", rem / 3600, rem % 3600 / 60, rem % 60))
        .replace("{width}", &pic.w.to_string())
        .replace("{height}", &pic.h.to_string())
        .replace("{format}", img::Format::ext(fmt))
        .replace("{index}", &index.to_string());
    for n in 2..=6 {
        out = out.replace(&format!("{{index:{n:02}}}"), &format!("{index:0n$}", n = n));
    }
    out
}

/// One picture through the settings; answers the file written (never over an existing one).
pub fn convert(path: &Path, s: &Value, index: usize) -> Result<PathBuf, String> {
    let fmt = match s["imgFormat"].as_str() {
        Some("png") => img::Format::Png,
        Some("pdf") => img::Format::Pdf,
        _ => img::Format::Jpeg,
    };
    let mut image = img::load(path, &|w, h| target(s, w, h))?;
    // Fill: scaled to cover, then the centre is cut to the asked size.
    if s["imgResize"] == "exact" && s["imgFit"] == "fill" {
        let (tw, th) = (num(s, "imgW", 1600.0) as i32, num(s, "imgH", 1200.0) as i32);
        image = image.crop(svc_capture::Rect { x: (image.w as i32 - tw) / 2, y: (image.h as i32 - th) / 2, w: tw, h: th });
    }
    // Formats without transparency, and the watermark's blend, want a flat background.
    let bg = if s["imgBg"] == "black" { [0, 0, 0] } else { [255, 255, 255] };
    if fmt != img::Format::Png {
        img::flatten(&mut image, bg);
    }
    img::watermark(&mut image, s["wmText"].as_str().unwrap_or(""), num(s, "wmPos", 3.0) as u8, num(s, "wmOpacity", 0.45) as f32, num(s, "wmMargin", 32.0) as i32, num(s, "wmScale", 0.18) as f32);
    let mut dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if s["imgSubfolder"].as_bool().unwrap_or(false) {
        dir = dir.join("Converted");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let pattern = s["imgPattern"].as_str().unwrap_or("").trim();
    let base = if pattern.is_empty() { stem } else { named(pattern, &stem, &image, fmt, index) };
    let base: String = base.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let out = svc_shell::free_name(&dir, &format!("{base}.{}", fmt.ext()));
    let part = svc_shell::free_name(&dir, &format!("{base}.{}.part", fmt.ext()));
    // Written under a part name and renamed, so a failure leaves no half file.
    img::save(&image, fmt, num(s, "imgQuality", 0.72) as f32, &part).inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })?;
    std::fs::rename(&part, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn feature() -> Arc<dyn Feature> {
    Arc::new(MediaTools::default())
}
