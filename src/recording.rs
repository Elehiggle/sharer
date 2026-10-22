//! Region recording into a self-contained animated WebP.

use std::{
    io::{Seek, Write},
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use sharer::{
    capture::{DesktopRegion, RegionCapturer, recording_frame_bytes},
    config::CaptureResolution,
    upload::UploadPayload,
    validate_recording_fps,
};
use webp_animation::{Encoder, EncoderOptions, EncodingConfig};

mod webp;

use webp::WebpWriter;

#[derive(Clone, Copy, Debug)]
pub(super) struct RecordingSettings {
    pub(super) frames_per_second: u32,
    pub(super) max_seconds: u32,
    pub(super) max_bytes: u64,
    pub(super) capture_resolution: CaptureResolution,
}

pub(super) struct RecordedCapture {
    pub(super) payload: UploadPayload,
    pub(super) warning: Option<String>,
}

pub(super) fn capture_region(
    region: DesktopRegion,
    stop: &Receiver<()>,
    filename_stem: &str,
    settings: RecordingSettings,
    now: fn() -> Instant,
) -> Result<RecordedCapture> {
    validate_recording_fps(settings.frames_per_second)?;
    sharer::validate_recording_seconds(settings.max_seconds)?;

    let capturer = RegionCapturer::new_for_recording(region, settings.capture_resolution)?;
    let output = tempfile::tempfile().context("failed to create a temporary recording file")?;
    let writer = WebpWriter::new(output, capturer.dimensions(), settings.max_bytes)?;

    let animation = record_frames(
        writer,
        stop,
        settings,
        RecordingCallbacks {
            now,
            capture: || capturer.capture_rgba(),
        },
    )?;
    let warning = animation.stopped_at_limit.then(|| {
        format!(
            "recording stopped at the {} GiB file limit",
            settings.max_bytes / (1024 * 1024 * 1024)
        )
    });

    Ok(RecordedCapture {
        payload: UploadPayload::from_reader(
            animation.output,
            animation.length,
            format!("{filename_stem}-recording.webp"),
            "image/webp".to_owned(),
        ),
        warning,
    })
}

struct RecordedAnimation<W> {
    output: W,
    length: u64,
    stopped_at_limit: bool,
}

struct RecordingCallbacks<N, C> {
    now: N,
    capture: C,
}

fn record_frames<W, N, C>(
    mut writer: WebpWriter<W>,
    stop: &Receiver<()>,
    settings: RecordingSettings,
    callbacks: RecordingCallbacks<N, C>,
) -> Result<RecordedAnimation<W>>
where
    W: Write + Seek,
    N: FnMut() -> Instant,
    C: FnMut() -> Result<Vec<u8>>,
{
    let RecordingCallbacks {
        mut now,
        mut capture,
    } = callbacks;
    let RecordingSettings {
        frames_per_second,
        max_seconds,
        max_bytes: _,
        capture_resolution: _,
    } = settings;
    let dimensions = writer.dimensions();
    let frame_bytes = recording_frame_bytes(dimensions)?;
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(frames_per_second));
    let started = now();
    let deadline = started
        .checked_add(Duration::from_secs(u64::from(max_seconds)))
        .context("recording duration is too long")?;
    let mut next_frame = started;
    let mut last_timestamp: Option<i32> = None;
    let mut stopped_at_limit = false;

    loop {
        let current_time = now();

        if last_timestamp.is_some() && current_time >= deadline {
            break;
        }

        let elapsed_timestamp =
            duration_timestamp(current_time.saturating_duration_since(started))?;
        let timestamp = match last_timestamp {
            Some(previous) => previous
                .checked_add(1)
                .context("recording duration is too long")?
                .max(elapsed_timestamp),

            None => 0,
        };
        let image = capture().context("failed to capture a recording frame")?;

        anyhow::ensure!(
            image.len() == frame_bytes,
            "screen capture returned an unexpected recording size"
        );
        // Each encoder sees one bounded frame. Completed frames live in the temporary
        // file, so neither encoding nor finalization retains the entire recording.
        let data = encode_frame(dimensions, &image)?;

        drop(image);

        if !writer.add_frame(&data, timestamp)? {
            anyhow::ensure!(
                last_timestamp.is_some(),
                "recording frame exceeds the file limit"
            );
            stopped_at_limit = true;

            break;
        }

        last_timestamp = Some(timestamp);

        next_frame = next_frame
            .checked_add(frame_interval)
            .context("recording duration is too long")?;
        let current_time = now();

        if next_frame < current_time {
            next_frame = current_time;
        }

        let next_wake = next_frame.min(deadline);

        match stop.recv_timeout(next_wake.saturating_duration_since(current_time)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    let final_timestamp =
        duration_timestamp(now().min(deadline).saturating_duration_since(started))?.max(
            last_timestamp
                .context("recording produced no frames")?
                .checked_add(1)
                .context("recording duration is too long")?,
        );
    let (output, length) = writer.finish(final_timestamp)?;

    Ok(RecordedAnimation {
        output,
        length,
        stopped_at_limit,
    })
}

fn encode_frame(dimensions: (u32, u32), image: &[u8]) -> Result<webp_animation::WebPData> {
    // Enforce the bound before the native encoder allocates its canvas and scratch buffers.
    anyhow::ensure!(
        image.len() == recording_frame_bytes(dimensions)?,
        "screen capture returned an unexpected recording size"
    );
    let options = EncoderOptions {
        encoding_config: Some(EncodingConfig::new_lossy(80.0)),
        ..EncoderOptions::default()
    };
    let mut encoder = Encoder::new_with_options(dimensions, options)
        .context("failed to initialize the WebP recorder")?;

    encoder
        .add_frame(image, 0)
        .context("failed to encode a recording frame")?;
    encoder
        .finalize(1)
        .context("failed to finish a recording frame")
}

fn duration_timestamp(elapsed: Duration) -> Result<i32> {
    let milliseconds = elapsed.as_millis();

    i32::try_from(milliseconds).context("recording duration is too long")
}

#[cfg(test)]
mod tests;
