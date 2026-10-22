use std::{
    io::Cursor,
    path::Path,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::{MAX_DIAGNOSTIC_BYTES, Mp4Writer, diagnostic_tail};

use crate::recording::{FrameWriter, RecordingCallbacks, RecordingSettings, record_frames};
use sharer::{
    config::{CaptureResolution, RecordingFormat},
    upload::UploadPayload,
};

const TEST_MAX_BYTES: u64 = 256 * 1024 * 1024;

fn assert_decodable(path: &Path) {
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();

    assert!(
        decode.status.success(),
        "{}",
        String::from_utf8_lossy(&decode.stderr)
    );
}

#[test]
fn diagnostics_keep_only_a_bounded_tail() {
    let mut messages = vec![b'a'; MAX_DIAGNOSTIC_BYTES * 10];

    messages.extend_from_slice(b"last encoder error");
    let diagnostic = diagnostic_tail(Cursor::new(messages));

    assert_eq!(diagnostic.len(), MAX_DIAGNOSTIC_BYTES);
    assert!(diagnostic.ends_with("last encoder error"));
}

#[test]
#[ignore = "requires FFmpeg with libx264 and ffprobe on PATH"]
fn slow_capture_produces_a_playable_mp4_with_elapsed_timing_and_cleanup() {
    let dimensions = (65, 49);
    let writer = Mp4Writer::new(dimensions, 30, TEST_MAX_BYTES).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let started = Instant::now();
    let recording = record_frames(
        writer,
        &receiver,
        RecordingSettings {
            frames_per_second: 30,
            max_bytes: TEST_MAX_BYTES,
            max_seconds: 1,
            capture_resolution: CaptureResolution::Native,
            format: RecordingFormat::Mp4,
        },
        RecordingCallbacks {
            now: Instant::now,
            capture: || {
                thread::sleep(Duration::from_millis(100));
                Ok(vec![
                    255;
                    usize::try_from(dimensions.0 * dimensions.1 * 4)
                        .unwrap()
                ])
            },
        },
    )
    .unwrap();

    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!recording.stopped_at_limit);
    let temporary_path = recording.output.path().to_owned();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames:format=duration",
            "-of",
            "json",
        ])
        .arg(&temporary_path)
        .output()
        .unwrap();

    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let stream = &metadata["streams"][0];

    assert_eq!(stream["codec_name"], "h264");
    assert_eq!(stream["width"], 66);
    assert_eq!(stream["height"], 50);
    let duration: f64 = metadata["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let frames: u32 = stream["nb_read_frames"].as_str().unwrap().parse().unwrap();

    assert!(
        (0.7..1.6).contains(&duration),
        "unexpected playback duration: {duration}"
    );
    assert!(
        (5..20).contains(&frames),
        "unexpected frame count: {frames}"
    );
    assert_decodable(&temporary_path);
    let directory = tempfile::tempdir().unwrap();
    let mut payload = UploadPayload::from_reader(
        recording.output,
        recording.length,
        "test-recording.mp4".into(),
        "video/mp4".into(),
    );
    let saved = payload.save_generated_copy(directory.path()).unwrap();

    assert_eq!(std::fs::metadata(&saved).unwrap().len(), recording.length);
    assert_eq!(payload.mime, "video/mp4");
    drop(payload);
    assert!(!temporary_path.exists());
    assert!(saved.exists());
}

#[test]
#[ignore = "requires FFmpeg with libx264 on PATH"]
fn early_capture_failure_terminates_ffmpeg_and_removes_temporary_output() {
    let writer = Mp4Writer::new((64, 48), 30, TEST_MAX_BYTES).unwrap();
    let temporary_path = writer.output.as_ref().unwrap().path().to_owned();
    let (_sender, receiver) = mpsc::channel();
    let started = Instant::now();
    let result = record_frames(
        writer,
        &receiver,
        RecordingSettings {
            frames_per_second: 30,
            max_bytes: TEST_MAX_BYTES,
            max_seconds: 1,
            capture_resolution: CaptureResolution::Native,
            format: RecordingFormat::Mp4,
        },
        RecordingCallbacks {
            now: Instant::now,
            capture: || anyhow::bail!("capture failed"),
        },
    );

    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!temporary_path.exists());
}

#[test]
#[ignore = "requires FFmpeg with libx264 on PATH"]
fn stopping_immediately_still_finishes_the_first_frame() {
    let writer = Mp4Writer::new((64, 48), 30, TEST_MAX_BYTES).unwrap();
    let (sender, receiver) = mpsc::channel();

    sender.send(()).unwrap();
    let recording = record_frames(
        writer,
        &receiver,
        RecordingSettings {
            frames_per_second: 30,
            max_bytes: TEST_MAX_BYTES,
            max_seconds: 60,
            capture_resolution: CaptureResolution::Native,
            format: RecordingFormat::Mp4,
        },
        RecordingCallbacks {
            now: Instant::now,
            capture: || Ok(vec![255; 64 * 48 * 4]),
        },
    )
    .unwrap();

    assert!(recording.length > 0);
    let mut writer = Mp4Writer::new((64, 48), 30, TEST_MAX_BYTES).unwrap();

    assert!(writer.add_rgba(vec![0; 3], 0).is_err());
}

#[test]
#[ignore = "requires FFmpeg with libx264 on PATH"]
fn file_limit_finishes_a_playable_partial_recording() {
    let max_bytes = 2 * 1024 * 1024;
    let writer = Mp4Writer::new((256, 256), 30, max_bytes).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let mut random = 1_u32;
    let recording = record_frames(
        writer,
        &receiver,
        RecordingSettings {
            frames_per_second: 30,
            max_bytes,
            max_seconds: 10,
            capture_resolution: CaptureResolution::Native,
            format: RecordingFormat::Mp4,
        },
        RecordingCallbacks {
            now: Instant::now,
            capture: || {
                let mut frame = vec![0; 256 * 256 * 4];

                for pixel in frame.as_chunks_mut::<4>().0 {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    pixel.copy_from_slice(&random.to_le_bytes());
                    pixel[3] = 255;
                }

                Ok(frame)
            },
        },
    )
    .unwrap();

    assert!(recording.stopped_at_limit);
    assert!(recording.length >= 1024 * 1024);
    assert!(recording.length <= max_bytes);
    assert_decodable(recording.output.path());
}

#[cfg(windows)]
#[test]
#[ignore = "captures a live desktop region; requires FFmpeg with libx264 and an interactive Windows session"]
fn live_desktop_capture_saves_a_playable_mp4() {
    use sharer::capture::{self, CaptureColorMode, NormalizedRegion};

    let screens =
        capture::capture_region_sources(CaptureColorMode::Sdr, CaptureResolution::Native).unwrap();
    let region = screens
        .first()
        .unwrap()
        .desktop_region(NormalizedRegion::new([0.05, 0.05, 0.2, 0.2]))
        .unwrap();

    drop(screens);
    let (_sender, receiver) = mpsc::channel();
    let mut recorded = crate::recording::capture_region(
        region,
        &receiver,
        "desktop-smoke",
        RecordingSettings {
            frames_per_second: 30,
            max_bytes: TEST_MAX_BYTES,
            max_seconds: 10,
            capture_resolution: CaptureResolution::Native,
            format: RecordingFormat::Mp4,
        },
        Instant::now,
    )
    .unwrap();

    assert_eq!(recorded.payload.filename, "desktop-smoke-recording.mp4");
    assert_eq!(recorded.payload.mime, "video/mp4");
    assert!(recorded.warning.is_none());
    let directory = tempfile::tempdir().unwrap();
    let saved = recorded
        .payload
        .save_generated_copy(directory.path())
        .unwrap();

    assert_decodable(&saved);
}
