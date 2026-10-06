//! Representative-frame extraction for the `%s` description provider.
//!
//! A video has no single pixel stream a vision model can read the way a
//! photo does, so evenly spaced representative frames are decoded with
//! `ffmpeg` and forwarded to the same ollama vision transport used for
//! photos (the cache key already binds the video's content hash, exactly as
//! it does a photo's). Extraction runs in the background provider worker,
//! only on a provider cache miss, so a scan is never blocked on it and a
//! cached description is reused without re-reading the video.
//!
//! `ffmpeg` (and `ffprobe`, for the seek points) are optional: when the
//! tool is absent, the video cannot be decoded, or the extracted frames are
//! not images, the job degrades to the existing text-only prompt (the
//! model sees the path and kind) and `%s` still resolves. Every external
//! call runs under the `[limits] tool_timeout_seconds` discipline through
//! the shared `run_capture`, which clears the environment and discards
//! stderr; a timeout or non-zero exit is an extraction failure like any
//! other.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use homelinter_analyze::run_capture;
use homelinter_scan::ToolSet;

/// Approximate spacing between sampled video frames, in seconds. Frames
/// are distributed evenly, so a clip shorter than this yields one frame
/// while a longer one is sampled about once per second.
const FRAME_INTERVAL_SECS: f64 = 1.0;

/// ffprobe prints a single floating-point duration; the cap only guards a
/// runaway producer (the value is a few bytes).
const PROBE_MAX_BYTES: u64 = 64 * 1024;
/// ffmpeg writes each frame to a file, not stdout, so this only bounds
/// stray output; the cap exists to keep the shared runner's contract.
const FRAME_MAX_BYTES: u64 = 1024 * 1024;

/// Seek points (seconds) for representative frames: about one per second,
/// evenly distributed across `duration`, thinned to at most `max_frames`.
/// Always returns at least one point; `duration` must be positive.
fn frame_seek_points(duration: f64, max_frames: usize) -> Vec<f64> {
    let count = ((duration / FRAME_INTERVAL_SECS).ceil() as usize).clamp(1, max_frames.max(1));
    (0..count)
        .map(|i| duration * (i as f64 + 0.5) / count as f64)
        .collect()
}

/// Extract representative frames from `path`, each downscaled to JPEG
/// bytes. Returns an empty vector when ffmpeg is unavailable or extraction
/// fails; the caller then degrades to the text-only prompt. `max_dim`
/// follows `description.max_image_dimension` (0 disables the cap);
/// `max_frames` caps the sample count (`description.max_video_frames`).
pub fn extract_video_frames(
    path: &Path,
    max_dim: u64,
    max_frames: usize,
    tools: &ToolSet,
    timeout: Duration,
) -> Vec<Vec<u8>> {
    if !tools.has("ffmpeg") {
        return Vec::new();
    }
    let Some(dir) = temp_dir() else {
        return Vec::new();
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    // With a duration, sample about one frame per second, evenly
    // distributed across the clip and thinned to `max_frames`. Without
    // `ffprobe` or a usable duration, fall back to ffmpeg's `thumbnail`
    // filter, which picks one representative frame from a bounded window.
    let seeks: Vec<Option<f64>> = if tools.has("ffprobe") {
        match probe_duration(path, timeout) {
            Some(d) if d > 0.0 => frame_seek_points(d, max_frames)
                .into_iter()
                .map(Some)
                .collect(),
            _ => vec![None],
        }
    } else {
        vec![None]
    };
    let mut frames = Vec::new();
    for (i, seek) in seeks.iter().enumerate() {
        let out = dir.join(format!("frame_{i}.jpg"));
        if grab_frame(path, *seek, &out, timeout) {
            if let Ok(raw) = std::fs::read(&out) {
                frames.push(crate::providers::prepare_image_bytes_from_raw(raw, max_dim));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    frames
}

/// Run ffmpeg once, writing a single frame to `out`. With `seek` (`None`
/// means no `-ss`) the image is taken at that timestamp; without it the
/// `thumbnail` filter selects a representative frame. Returns whether the
/// frame was produced.
fn grab_frame(path: &Path, seek: Option<f64>, out: &Path, timeout: Duration) -> bool {
    let path = match path.to_str() {
        Some(p) => p,
        None => return false,
    };
    let out = match out.to_str() {
        Some(o) => o,
        None => return false,
    };
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    if let Some(seconds) = seek {
        args.push("-ss".into());
        args.push(format!("{seconds:.3}"));
    }
    args.push("-i".into());
    args.push(path.into());
    if seek.is_none() {
        args.push("-vf".into());
        args.push("thumbnail".into());
    }
    args.push("-frames:v".into());
    args.push("1".into());
    args.push("-f".into());
    args.push("image2".into());
    args.push(out.into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_capture("ffmpeg", &refs, timeout, FRAME_MAX_BYTES).is_some()
}

/// Read a video's duration in seconds with ffprobe. `None` on any failure
/// (missing tool, non-zero exit, unparsable output).
fn probe_duration(path: &Path, timeout: Duration) -> Option<f64> {
    let path = path.to_str()?;
    let out = run_capture(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path,
        ],
        timeout,
        PROBE_MAX_BYTES,
    )?;
    String::from_utf8_lossy(&out).trim().parse::<f64>().ok()
}

/// A unique per-call scratch directory under the system temp directory.
fn temp_dir() -> Option<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(std::env::temp_dir().join(format!("homelinter-video-{}-{nanos}", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn no_ffmpeg_degrades_to_no_frames() {
        let path = PathBuf::from("/tmp/homelinter-nope.mp4");
        let tools = ToolSet::default();
        assert!(extract_video_frames(&path, 1024, 8, &tools, Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn seek_points_are_one_per_second_and_evenly_capped() {
        // Shorter than the interval: a single centred frame.
        assert_eq!(frame_seek_points(0.5, 8), vec![0.25]);
        // Two seconds: one frame per second, at the centre of each second.
        assert_eq!(frame_seek_points(2.0, 8), vec![0.5, 1.5]);
        // Fractional seconds round up to keep at least one frame per second.
        assert_eq!(frame_seek_points(2.5, 8).len(), 3);
        // Eight seconds at the cap: every second kept.
        assert_eq!(frame_seek_points(8.0, 8).len(), 8);
        // Longer than the cap: thinned to the cap, evenly distributed.
        let long = frame_seek_points(60.0, 8);
        assert_eq!(long.len(), 8);
        assert!((long[0] - 3.75).abs() < 1e-9);
        assert!((long[7] - 56.25).abs() < 1e-9);
        // A cap of one still yields the clip's midpoint.
        assert_eq!(frame_seek_points(3.0, 1), vec![1.5]);
        // A zero cap is treated as one rather than producing no frames.
        assert_eq!(frame_seek_points(3.0, 0).len(), 1);
    }

    #[test]
    fn extracts_and_downscales_frames() {
        if !ffmpeg_available() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("homelinter-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.avi");
        // testsrc plus the built-in mjpeg encoder: no external codec needed.
        let ok = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=5",
                "-c:v",
                "mjpeg",
            ])
            .arg(&video)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut tools = ToolSet::default();
        tools.available.insert("ffmpeg".into());
        tools.available.insert("ffprobe".into());
        let frames = extract_video_frames(&video, 100, 8, &tools, Duration::from_secs(30));
        // A 2-second clip with a known duration yields one frame per second.
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            let img = image::load_from_memory(frame).expect("decodable jpeg");
            assert!(
                img.width() <= 100 && img.height() <= 100,
                "{}x{}",
                img.width(),
                img.height()
            );
            assert!(img.width() > 0 && img.height() > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
