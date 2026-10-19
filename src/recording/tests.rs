//! Decoder roundtrips and stopping conditions without a native desktop session.

use std::{
    io::{Cursor, Read as _},
    sync::mpsc,
    time::{Duration, Instant},
};

use webp_animation::Decoder;

use super::{
    MAX_RECORDING_BYTES, RecordingSettings, duration_timestamp, encode_frame, record_frames,
    webp::WebpWriter,
};
use sharer::config::CaptureResolution;

fn settings() -> RecordingSettings {
    RecordingSettings {
        frames_per_second: 60,
        max_seconds: 1,
        capture_resolution: CaptureResolution::Native,
    }
}

fn advancing_clock() -> impl FnMut() -> Instant {
    let started = Instant::now();
    let mut elapsed = Duration::ZERO;

    move || {
        let current = started + elapsed;

        elapsed += Duration::from_millis(100);

        current
    }
}

#[test]
fn timestamps_use_real_elapsed_milliseconds() {
    assert_eq!(duration_timestamp(Duration::ZERO).unwrap(), 0);
    assert_eq!(
        duration_timestamp(Duration::from_micros(16_999)).unwrap(),
        16
    );
    assert_eq!(duration_timestamp(Duration::from_secs(1)).unwrap(), 1_000);
    assert!(duration_timestamp(Duration::from_millis(u64::MAX)).is_err());
}

#[test]
fn muxed_frames_preserve_alpha_pixels_and_real_durations() {
    let dimensions = (3, 2);
    let mut writer =
        WebpWriter::new(Cursor::new(Vec::new()), dimensions, MAX_RECORDING_BYTES).unwrap();
    let mut expected_pixels = Vec::new();

    for (timestamp, pixel) in [
        (0, [255, 0, 0, 255]),
        (20, [0, 0, 0, 0]),
        (55, [0, 255, 0, 128]),
    ] {
        let pixels = pixel.repeat(6);
        let encoded = encode_frame(dimensions, &pixels).unwrap();
        let frame = Decoder::new(&encoded).unwrap().into_iter().next().unwrap();

        expected_pixels.push(frame.data().to_vec());
        assert!(writer.add_frame(&encoded, timestamp).unwrap());
    }

    let (output, length) = writer.finish(120).unwrap();
    let bytes = output.into_inner();

    assert_eq!(u64::try_from(bytes.len()).unwrap(), length);
    assert_eq!(bytes[20] & 0x12, 0x12);
    let decoder = Decoder::new(&bytes).unwrap();

    assert_eq!(decoder.dimensions(), dimensions);
    let frames: Vec<_> = decoder.into_iter().collect();

    assert_eq!(frames.len(), 3);

    for ((frame, expected), timestamp) in frames.iter().zip(expected_pixels).zip([20, 55, 120]) {
        assert_eq!(frame.data(), expected);
        assert_eq!(frame.timestamp(), timestamp);
    }
}

#[test]
fn output_budget_keeps_a_valid_complete_first_frame() {
    let dimensions = (2, 2);
    let encoded = encode_frame(dimensions, &[255; 16]).unwrap();
    let mut probe =
        WebpWriter::new(Cursor::new(Vec::new()), dimensions, MAX_RECORDING_BYTES).unwrap();

    assert!(probe.add_frame(&encoded, 0).unwrap());
    let (_, first_length) = probe.finish(20).unwrap();
    let mut writer = WebpWriter::new(Cursor::new(Vec::new()), dimensions, first_length).unwrap();

    assert!(writer.add_frame(&encoded, 0).unwrap());
    assert!(!writer.add_frame(&encoded, 20).unwrap());
    let (output, length) = writer.finish(50).unwrap();
    let bytes = output.into_inner();
    let frames: Vec<_> = Decoder::new(&bytes).unwrap().into_iter().collect();

    assert_eq!(length, first_length);
    assert_eq!(u64::try_from(bytes.len()).unwrap(), first_length);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp(), 50);
}

#[test]
fn budget_stop_preserves_frames_and_reports_the_limit() {
    let dimensions = (2, 2);
    let encoded = encode_frame(dimensions, &[255; 16]).unwrap();
    let mut probe =
        WebpWriter::new(Cursor::new(Vec::new()), dimensions, MAX_RECORDING_BYTES).unwrap();

    assert!(probe.add_frame(&encoded, 0).unwrap());
    let (_, first_length) = probe.finish(20).unwrap();
    let writer = WebpWriter::new(Cursor::new(Vec::new()), dimensions, first_length).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let mut captures = 0;
    let animation = record_frames(writer, &receiver, settings(), advancing_clock(), || {
        captures += 1;

        Ok(vec![255; 16])
    })
    .unwrap();

    assert!(animation.stopped_at_limit);
    assert_eq!(animation.length, first_length);
    assert_eq!(captures, 2);
    let bytes = animation.output.into_inner();

    assert_eq!(Decoder::new(&bytes).unwrap().into_iter().count(), 1);
}

#[test]
fn requested_stop_keeps_repeated_frames_and_timing() {
    let writer = WebpWriter::new(Cursor::new(Vec::new()), (2, 2), MAX_RECORDING_BYTES).unwrap();
    let (sender, receiver) = mpsc::channel();
    let mut captures = 0;
    let animation = record_frames(writer, &receiver, settings(), advancing_clock(), || {
        captures += 1;

        if captures == 2 {
            sender.send(()).unwrap();
        }

        Ok(vec![255; 16])
    })
    .unwrap();
    let bytes = animation.output.into_inner();
    let frames: Vec<_> = Decoder::new(&bytes).unwrap().into_iter().collect();

    assert!(!animation.stopped_at_limit);
    assert_eq!(captures, 2);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data(), frames[1].data());
    assert_eq!(frames[0].timestamp(), 200);
    assert_eq!(frames[1].timestamp(), 400);
}

#[test]
fn duration_limit_stops_without_a_stop_message() {
    let writer = WebpWriter::new(Cursor::new(Vec::new()), (2, 2), MAX_RECORDING_BYTES).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let mut captures = 0;
    let animation = record_frames(writer, &receiver, settings(), advancing_clock(), || {
        captures += 1;

        Ok(vec![255; 16])
    })
    .unwrap();

    assert!(!animation.stopped_at_limit);
    assert_eq!(captures, 5);
    let bytes = animation.output.into_inner();

    assert_eq!(Decoder::new(&bytes).unwrap().into_iter().count(), 5);
}

#[test]
fn temporary_recording_remains_readable_until_owned_file_is_dropped() {
    let mut writer =
        WebpWriter::new(tempfile::tempfile().unwrap(), (2, 2), MAX_RECORDING_BYTES).unwrap();
    let encoded = encode_frame((2, 2), &[255; 16]).unwrap();

    assert!(writer.add_frame(&encoded, 0).unwrap());
    let (mut output, length) = writer.finish(20).unwrap();
    let mut bytes = Vec::new();

    output.read_to_end(&mut bytes).unwrap();
    assert_eq!(length, u64::try_from(bytes.len()).unwrap());
    assert_eq!(Decoder::new(&bytes).unwrap().into_iter().count(), 1);
}

#[test]
fn invalid_sizes_and_timestamps_fail_before_finishing() {
    let dimensions = (2, 2);

    assert!(encode_frame((16_383, 16_383), &[]).is_err());
    assert!(encode_frame(dimensions, &[0; 12]).is_err());
    let encoded = encode_frame(dimensions, &[255; 16]).unwrap();
    let mut writer =
        WebpWriter::new(Cursor::new(Vec::new()), dimensions, MAX_RECORDING_BYTES).unwrap();

    assert!(writer.add_frame(&encoded, 100).unwrap());
    assert!(writer.add_frame(&encoded, 100).is_err());
    assert!(writer.finish(99).is_err());
}
