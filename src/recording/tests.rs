//! Decoder roundtrips and stopping conditions without a native desktop session.

use std::{
    io::{Cursor, Read as _},
    sync::mpsc,
    time::{Duration, Instant},
};

use webp_animation::Decoder;

use super::{
    RecordingCallbacks, RecordingSettings, duration_timestamp, encode_frame, record_frames,
    webp::WebpWriter,
};
use sharer::config::CaptureResolution;

const TEST_MAX_BYTES: u64 = 256 * 1024 * 1024;

fn settings() -> RecordingSettings {
    RecordingSettings {
        frames_per_second: 60,
        max_seconds: 1,
        max_bytes: TEST_MAX_BYTES,
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
    let mut writer = WebpWriter::new(Cursor::new(Vec::new()), dimensions, TEST_MAX_BYTES).unwrap();
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
fn optimized_sparse_frames_preserve_the_full_independent_canvas() {
    for size in [128_u32, 512, 2000] {
        let dimensions = (size, size);
        let pixels = usize::try_from(size * size).unwrap();
        let mut writer =
            WebpWriter::new(Cursor::new(Vec::new()), dimensions, TEST_MAX_BYTES).unwrap();
        let mut expected_pixels = Vec::new();

        for (index, crop) in [None, Some((size / 2, size / 2)), Some((2, 6)), None]
            .into_iter()
            .enumerate()
        {
            let mut rgba = if index == 0 {
                [0, 0, 255, 255].repeat(pixels)
            } else {
                vec![0_u8; pixels * 4]
            };

            if let Some((left, top)) = crop {
                for y in top..top + 4 {
                    for x in left..left + 4 {
                        let offset = usize::try_from((y * size + x) * 4).unwrap();

                        rgba[offset..offset + 4].copy_from_slice(&[255, 0, 0, 255]);
                    }
                }
            }

            let encoded = encode_frame(dimensions, &rgba).unwrap();

            if index == 1 {
                // The pinned native encoder retains a cropped one-frame animation.
                assert_eq!(encoded.get(30..34), Some(b"ANIM".as_slice()));
                assert_eq!(encoded.get(44..48), Some(b"ANMF".as_slice()));
            }

            let expected = Decoder::new(&encoded).unwrap().into_iter().next().unwrap();

            expected_pixels.push(expected.data().to_vec());
            assert!(
                writer
                    .add_frame(&encoded, i32::try_from(index * 40).unwrap())
                    .unwrap()
            );
        }

        let (output, _) = writer.finish(160).unwrap();
        let bytes = output.into_inner();
        let frames: Vec<_> = Decoder::new(&bytes).unwrap().into_iter().collect();

        assert_eq!(frames.len(), 4);

        for (index, (frame, expected)) in frames.iter().zip(expected_pixels).enumerate() {
            assert_eq!(frame.data(), expected, "canvas {size}, frame {index}");
            assert_eq!(frame.timestamp(), i32::try_from((index + 1) * 40).unwrap());
        }
    }
}

#[test]
fn output_budget_keeps_a_valid_complete_first_frame() {
    let dimensions = (2, 2);
    let encoded = encode_frame(dimensions, &[255; 16]).unwrap();
    let mut probe = WebpWriter::new(Cursor::new(Vec::new()), dimensions, TEST_MAX_BYTES).unwrap();

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
    let mut probe = WebpWriter::new(Cursor::new(Vec::new()), dimensions, TEST_MAX_BYTES).unwrap();

    assert!(probe.add_frame(&encoded, 0).unwrap());
    let (_, first_length) = probe.finish(20).unwrap();
    let writer = WebpWriter::new(Cursor::new(Vec::new()), dimensions, first_length).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let mut captures = 0;
    let animation = record_frames(
        writer,
        &receiver,
        settings(),
        RecordingCallbacks {
            now: advancing_clock(),
            capture: || {
                captures += 1;

                Ok(vec![255; 16])
            },
        },
    )
    .unwrap();

    assert!(animation.stopped_at_limit);
    assert_eq!(animation.length, first_length);
    assert_eq!(captures, 2);
    let bytes = animation.output.into_inner();

    assert_eq!(Decoder::new(&bytes).unwrap().into_iter().count(), 1);
}

#[test]
fn requested_stop_keeps_repeated_frames_and_timing() {
    let writer = WebpWriter::new(Cursor::new(Vec::new()), (2, 2), TEST_MAX_BYTES).unwrap();
    let (sender, receiver) = mpsc::channel();
    let mut captures = 0;
    let animation = record_frames(
        writer,
        &receiver,
        settings(),
        RecordingCallbacks {
            now: advancing_clock(),
            capture: || {
                captures += 1;

                if captures == 2 {
                    sender.send(()).unwrap();
                }

                Ok(vec![255; 16])
            },
        },
    )
    .unwrap();
    let bytes = animation.output.into_inner();
    let frames: Vec<_> = Decoder::new(&bytes).unwrap().into_iter().collect();

    assert!(!animation.stopped_at_limit);
    assert_eq!(captures, 2);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data(), frames[1].data());
    assert_eq!(frames[0].timestamp(), 300);
    assert_eq!(frames[1].timestamp(), 500);
}

#[test]
fn duration_limit_stops_without_a_stop_message() {
    let writer = WebpWriter::new(Cursor::new(Vec::new()), (2, 2), TEST_MAX_BYTES).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let mut captures = 0;
    let animation = record_frames(
        writer,
        &receiver,
        settings(),
        RecordingCallbacks {
            now: advancing_clock(),
            capture: || {
                captures += 1;

                Ok(vec![255; 16])
            },
        },
    )
    .unwrap();

    assert!(!animation.stopped_at_limit);
    assert_eq!(captures, 5);
    let bytes = animation.output.into_inner();

    let frames: Vec<_> = Decoder::new(&bytes).unwrap().into_iter().collect();

    assert_eq!(frames.len(), 5);
    assert_eq!(frames.last().unwrap().timestamp(), 1_000);
}

#[test]
fn slow_first_capture_still_finishes_within_the_duration_limit() {
    let writer = WebpWriter::new(Cursor::new(Vec::new()), (2, 2), TEST_MAX_BYTES).unwrap();
    let (_sender, receiver) = mpsc::channel();
    let started = Instant::now();
    let elapsed = std::cell::Cell::new(Duration::ZERO);
    let animation = record_frames(
        writer,
        &receiver,
        settings(),
        RecordingCallbacks {
            now: || started + elapsed.get(),
            capture: || {
                elapsed.set(Duration::from_secs(10));

                Ok(vec![255; 16])
            },
        },
    )
    .unwrap();
    let bytes = animation.output.into_inner();
    let frames: Vec<_> = Decoder::new(&bytes).unwrap().into_iter().collect();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp(), 1_000);
}

#[test]
fn temporary_recording_remains_readable_until_owned_file_is_dropped() {
    let mut writer =
        WebpWriter::new(tempfile::tempfile().unwrap(), (2, 2), TEST_MAX_BYTES).unwrap();
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
    let mut writer = WebpWriter::new(Cursor::new(Vec::new()), dimensions, TEST_MAX_BYTES).unwrap();

    assert!(writer.add_frame(&encoded, 100).unwrap());
    assert!(writer.add_frame(&encoded, 100).is_err());
    assert!(writer.finish(99).is_err());
}
