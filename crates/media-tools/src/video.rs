// Video: compressing through Windows' own transcoder (Media Foundation, H.264 into MP4) and reading
// frames for the GIF maker. Nothing is downloaded. Sizing plans are ported from
// reference/vorssaint-utils/Sources/Vorssaint/Services/Media/MediaSupport.swift (`videoSizePlan`,
// `targetRetryScale`, `scaledEvenSize`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Foundation::TimeSpan;
use windows_future::{AsyncActionProgressHandler, AsyncStatus};
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode, ExifOrientationMode};
use windows::Media::Editing::{MediaClip, MediaComposition, VideoFramePrecision};
use windows::Media::MediaProperties::{AudioEncodingProperties, AudioEncodingQuality, MediaEncodingProfile, VideoEncodingQuality};
use windows::Media::Transcoding::{MediaTranscoder, TranscodeFailureReason};
use windows::Storage::{CreationCollisionOption, StorageFile, StorageFolder};

pub type Progress = Arc<dyn Fn(f64) + Send + Sync>;

/// A compression pass that runs longer than this is stopped.
const LIMIT: Duration = Duration::from_secs(60 * 60);
const HEADROOM: f64 = 0.94;
const BITS_PER_PIXEL: f64 = 0.07;
const MIN_VIDEO_BITRATE: i64 = 240_000;

fn e(err: windows::core::Error) -> String {
    let m = err.message();
    if m.is_empty() { "Windows could not read this video. The Media Feature Pack may be missing.".into() } else { m }
}

pub fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| e.to_string())
}

fn file(path: &Path) -> Result<StorageFile, String> {
    let p = absolute(path)?;
    StorageFile::GetFileFromPathAsync(&HSTRING::from(p.to_string_lossy().as_ref())).map_err(e)?.get().map_err(e)
}

#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub w: u32,
    pub h: u32,
    pub secs: f64,
    pub fps: f64,
    pub audio: bool,
}

pub fn info(path: &Path) -> Result<Info, String> {
    let f = file(path)?;
    let props = f.Properties().map_err(e)?.GetVideoPropertiesAsync().map_err(e)?.get().map_err(e)?;
    let profile = MediaEncodingProfile::CreateFromFileAsync(&f).map_err(e)?.get().map_err(e)?;
    let v = profile.Video().map_err(e)?;
    let rate = v.FrameRate().map_err(e)?;
    let fps = rate.Numerator().unwrap_or(0) as f64 / rate.Denominator().unwrap_or(1).max(1) as f64;
    Ok(Info {
        w: props.Width().map_err(e)?,
        h: props.Height().map_err(e)?,
        secs: props.Duration().map_err(e)?.Duration as f64 / 1e7,
        fps: if fps > 0.0 { fps } else { 30.0 },
        audio: profile.Audio().ok().and_then(|a| a.SampleRate().ok()).unwrap_or(0) > 0,
    })
}

fn even(n: f64) -> u32 {
    ((n.round() as u32).max(2)) & !1
}

/// The size with its longer side at most `max_dim`, both sides even.
pub fn even_size(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    let k = (max_dim.max(2) as f64 / w.max(h) as f64).min(1.0);
    (even(w as f64 * k), even(h as f64 * k))
}

pub struct Opts {
    /// Longest side, for the resolution mode.
    pub max_dim: u32,
    /// Bits per pixel and frame: how hard to compress (resolution mode).
    pub bpp: f64,
    pub fps: f64,
    pub keep_audio: bool,
    /// Decimal megabytes; set means the file size mode.
    pub target_mb: Option<u32>,
    pub start: f64,
    /// 0 means to the end.
    pub end: f64,
}

struct Plan {
    w: u32,
    h: u32,
    video: i64,
    audio: i64,
}

fn size_plan(target: i64, secs: f64, src: &Info, fps: f64, audio: bool, scale: f64) -> Option<Plan> {
    let a = if !audio { 0 } else if (target as f64 * 8.0 / secs) < 1_200_000.0 { 64_000 } else { 128_000 };
    let budget = (target as f64 * 8.0 * HEADROOM / secs).floor() as i64 - a;
    let video = (budget as f64 * scale.min(1.0)) as i64;
    if video < MIN_VIDEO_BITRATE {
        return None;
    }
    let ratio = ((video as f64 / (fps * BITS_PER_PIXEL)) / (src.w as f64 * src.h as f64)).sqrt().min(1.0);
    let long = src.w.max(src.h) as f64;
    // A tiny frame is worse than a soft one: scaling stops at 480 on the long edge.
    let floor = (480.0 / long).min(1.0);
    let (w, h) = even_size(src.w, src.h, (long * ratio.max(floor)).round() as u32);
    Some(Plan { w, h, video, audio: a })
}

/// Compresses `input` to a new MP4 beside it and answers its path. `cancel` ends the run; a pass is also ended after an hour.
pub fn compress(input: &Path, o: &Opts, cancel: &AtomicBool, progress: Progress) -> Result<PathBuf, String> {
    let src = info(input)?;
    let end = if o.end > 0.0 { o.end.min(src.secs) } else { src.secs };
    let start = o.start.clamp(0.0, end);
    let secs = end - start;
    if secs <= 0.0 {
        return Err("The trim leaves nothing of the clip.".into());
    }
    let fps = o.fps.min(src.fps).max(1.0).round();
    let stem = input.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let dir = input.parent().map(Path::to_path_buf).unwrap_or_default();
    let dir = absolute(&dir)?;
    let out = svc_shell::free_name(&dir, &format!("{stem} compressed.mp4"));
    let part = svc_shell::free_name(&dir, &format!("{stem} compressed.mp4.part"));
    let source = file(input)?;
    let mut scale = 1.0;
    for pass in 0..3 {
        let plan = match o.target_mb {
            Some(mb) => {
                let target = mb.clamp(1, 512) as i64 * 1_000_000;
                size_plan(target, secs, &src, fps, o.keep_audio && src.audio, scale).ok_or("Target size too small for this clip.")?
            }
            None => {
                let (w, h) = even_size(src.w, src.h, o.max_dim);
                Plan { w, h, video: ((w as f64 * h as f64 * fps * o.bpp) as i64).max(MIN_VIDEO_BITRATE), audio: 128_000 }
            }
        };
        let p = progress.clone();
        let step: Progress = Arc::new(move |f| p((pass as f64 + f) / 3.0));
        let _ = std::fs::remove_file(&part);
        run(&source, &part, &plan, fps, o.keep_audio && src.audio, (start, if o.end > 0.0 { end } else { 0.0 }), cancel, step)?;
        let bytes = std::fs::metadata(&part).map(|m| m.len() as i64).unwrap_or(0);
        // One pass of rate control lands near the budget, not on it: a size target gets another pass at a smaller scale.
        let again = o.target_mb.and_then(|mb| {
            let target = mb.clamp(1, 512) as i64 * 1_000_000;
            (bytes > target).then(|| (scale * target as f64 / bytes as f64 * HEADROOM).min(scale * 0.9))
        });
        match again {
            Some(next) if pass < 2 => scale = next,
            _ => break,
        }
    }
    std::fs::rename(&part, &out).map_err(|e| e.to_string())?;
    progress(1.0);
    Ok(out)
}

/// The sound of an audio or video file as an MP3 beside it (Windows' own MP3 encoder); answers its path.
pub fn audio(input: &Path, trim: (f64, f64), cancel: &AtomicBool, progress: Progress) -> Result<PathBuf, String> {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let dir = absolute(input.parent().unwrap_or(Path::new(".")))?;
    let out = svc_shell::free_name(&dir, &format!("{stem}.mp3"));
    let part = svc_shell::free_name(&dir, &format!("{stem}.mp3.part"));
    let src = file(input)?;
    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(dir.to_string_lossy().as_ref())).map_err(e)?.get().map_err(e)?;
    let name = part.file_name().unwrap_or_default().to_string_lossy().to_string();
    let dst = folder.CreateFileAsync(&HSTRING::from(name), CreationCollisionOption::ReplaceExisting).map_err(e)?.get().map_err(e)?;
    let profile = MediaEncodingProfile::CreateMp3(AudioEncodingQuality::High).map_err(e)?;
    let t = MediaTranscoder::new().map_err(e)?;
    if trim.0 > 0.0 {
        t.SetTrimStartTime(TimeSpan { Duration: (trim.0 * 1e7) as i64 }).map_err(e)?;
    }
    if trim.1 > 0.0 {
        t.SetTrimStopTime(TimeSpan { Duration: (trim.1 * 1e7) as i64 }).map_err(e)?;
    }
    let prep = t.PrepareFileTranscodeAsync(&src, &dst, &profile).map_err(e)?.get().map_err(e)?;
    if !prep.CanTranscode().map_err(e)? {
        let _ = std::fs::remove_file(&part);
        return Err(match prep.FailureReason().map_err(e)? {
            TranscodeFailureReason::CodecNotFound => "Media Feature Pack needed: Windows has no codec for this file.".into(),
            _ => "Windows cannot take the sound out of this file.".to_string(),
        });
    }
    let op = prep.TranscodeAsync().map_err(e)?;
    op.SetProgress(&AsyncActionProgressHandler::new(move |_, f| {
        progress(*f / 100.0);
        Ok(())
    }))
    .map_err(e)?;
    let began = Instant::now();
    while op.Status().map_err(e)? == AsyncStatus::Started {
        if cancel.load(Ordering::Relaxed) || began.elapsed() > LIMIT {
            let _ = op.Cancel();
            while op.Status().ok() == Some(AsyncStatus::Started) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = std::fs::remove_file(&part);
            return Err(if cancel.load(Ordering::Relaxed) { "Cancelled.".into() } else { "The conversion took too long and was stopped.".into() });
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    if let Err(err) = op.GetResults() {
        let _ = std::fs::remove_file(&part);
        return Err(e(err));
    }
    std::fs::rename(&part, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn run(src: &StorageFile, part: &Path, plan: &Plan, fps: f64, audio: bool, trim: (f64, f64), cancel: &AtomicBool, progress: Progress) -> Result<(), String> {
    let mut last = "Windows could not compress this video.".to_string();
    // The graphics chip's encoder first; some refuse a fixed bitrate, then the software one.
    for hardware in [true, false] {
        let dir = part.parent().unwrap_or(Path::new("."));
        let name = part.file_name().unwrap_or_default().to_string_lossy().to_string();
        let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(dir.to_string_lossy().as_ref())).map_err(e)?.get().map_err(e)?;
        let dst = folder.CreateFileAsync(&HSTRING::from(name), CreationCollisionOption::ReplaceExisting).map_err(e)?.get().map_err(e)?;
        let profile = MediaEncodingProfile::CreateMp4(VideoEncodingQuality::Auto).map_err(e)?;
        let v = profile.Video().map_err(e)?;
        v.SetWidth(plan.w).map_err(e)?;
        v.SetHeight(plan.h).map_err(e)?;
        v.SetBitrate(plan.video as u32).map_err(e)?;
        let rate = v.FrameRate().map_err(e)?;
        rate.SetNumerator(fps as u32).map_err(e)?;
        rate.SetDenominator(1).map_err(e)?;
        if audio {
            profile.Audio().map_err(e)?.SetBitrate(plan.audio as u32).map_err(e)?;
        } else {
            profile.SetAudio(None::<&AudioEncodingProperties>).map_err(e)?;
        }
        let t = MediaTranscoder::new().map_err(e)?;
        t.SetHardwareAccelerationEnabled(hardware).map_err(e)?;
        if trim.0 > 0.0 {
            t.SetTrimStartTime(TimeSpan { Duration: (trim.0 * 1e7) as i64 }).map_err(e)?;
        }
        if trim.1 > 0.0 {
            t.SetTrimStopTime(TimeSpan { Duration: (trim.1 * 1e7) as i64 }).map_err(e)?;
        }
        let prep = t.PrepareFileTranscodeAsync(src, &dst, &profile).map_err(e)?.get().map_err(e)?;
        if !prep.CanTranscode().map_err(e)? {
            return Err(match prep.FailureReason().map_err(e)? {
                TranscodeFailureReason::CodecNotFound => "Media Feature Pack needed: Windows has no codec for this video.".into(),
                _ => "Windows cannot compress this video.".to_string(),
            });
        }
        let op = prep.TranscodeAsync().map_err(e)?;
        let p = progress.clone();
        op.SetProgress(&AsyncActionProgressHandler::new(move |_, f| {
            p(*f / 100.0);
            Ok(())
        }))
        .map_err(e)?;
        let began = Instant::now();
        while op.Status().map_err(e)? == AsyncStatus::Started {
            if cancel.load(Ordering::Relaxed) || began.elapsed() > LIMIT {
                let _ = op.Cancel();
                while op.Status().ok() == Some(AsyncStatus::Started) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = std::fs::remove_file(part);
                return Err(if cancel.load(Ordering::Relaxed) { "Cancelled.".into() } else { "The compression took too long and was stopped.".into() });
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        match op.GetResults() {
            Ok(()) => return Ok(()),
            Err(err) => last = e(err),
        }
    }
    let _ = std::fs::remove_file(part);
    Err(last)
}

/// The picture of the video at second `at`, `w` x `h` pixels, as RGBA.
pub struct Frames {
    comp: MediaComposition,
}

impl Frames {
    pub fn open(path: &Path) -> Result<Frames, String> {
        let clip = MediaClip::CreateFromFileAsync(&file(path)?).map_err(e)?.get().map_err(e)?;
        let comp = MediaComposition::new().map_err(e)?;
        comp.Clips().map_err(e)?.Append(&clip).map_err(e)?;
        Ok(Frames { comp })
    }

    pub fn at(&self, at: f64, w: u32, h: u32) -> Result<Vec<u8>, String> {
        let stream = self
            .comp
            .GetThumbnailAsync(TimeSpan { Duration: (at * 1e7) as i64 }, w as i32, h as i32, VideoFramePrecision::NearestFrame)
            .map_err(e)?
            .get()
            .map_err(e)?;
        let dec = BitmapDecoder::CreateAsync(&stream).map_err(e)?.get().map_err(e)?;
        let data = dec
            .GetPixelDataTransformedAsync(
                BitmapPixelFormat::Rgba8,
                BitmapAlphaMode::Ignore,
                &BitmapTransform::new().map_err(e)?,
                ExifOrientationMode::IgnoreExifOrientation,
                ColorManagementMode::DoNotColorManage,
            )
            .map_err(e)?
            .get()
            .map_err(e)?
            .DetachPixelData()
            .map_err(e)?;
        if data.len() != w as usize * h as usize * 4 {
            return Err("A frame came back at another size.".into());
        }
        Ok(data.to_vec())
    }
}
