//! Append independent WebP frames without retaining an animation in the native encoder.
//!
//! RIFF/VP8X/ANIM/ANMF layout follows Google's WebP Container Specification:
//! <https://developers.google.com/speed/webp/docs/riff_container>.

use std::io::{Seek, SeekFrom, Write};

use anyhow::{Context as _, Result, bail};
use sharer::capture::recording_frame_bytes;

const HEADER_BYTES: u64 = 44;

struct FramePosition {
    duration_offset: u64,
    timestamp: i32,
}

pub(super) struct WebpWriter<W> {
    output: W,
    dimensions: (u32, u32),
    length: u64,
    max_bytes: u64,
    previous: Option<FramePosition>,
    has_alpha: bool,
}

impl<W: Write + Seek> WebpWriter<W> {
    pub(super) fn new(mut output: W, dimensions: (u32, u32), max_bytes: u64) -> Result<Self> {
        recording_frame_bytes(dimensions)?;
        anyhow::ensure!(
            max_bytes >= HEADER_BYTES,
            "recording file limit is too small"
        );
        let mut header = [0_u8; 44];

        header[..4].copy_from_slice(b"RIFF");
        header[8..16].copy_from_slice(b"WEBPVP8X");
        header[16..20].copy_from_slice(&10_u32.to_le_bytes());
        header[20] = 0x02; // Animation; the alpha flag is patched at finalization.
        header[24..27].copy_from_slice(&uint24(dimensions.0 - 1)?);
        header[27..30].copy_from_slice(&uint24(dimensions.1 - 1)?);
        header[30..34].copy_from_slice(b"ANIM");
        header[34..38].copy_from_slice(&6_u32.to_le_bytes());
        // Transparent background and infinite looping, matching EncoderOptions::default().
        output.write_all(&header)?;

        Ok(Self {
            output,
            dimensions,
            length: HEADER_BYTES,
            max_bytes,
            previous: None,
            has_alpha: false,
        })
    }

    pub(super) const fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    /// Return false without changing the file if the complete next frame will not fit.
    pub(super) fn add_frame(&mut self, encoded: &[u8], timestamp: i32) -> Result<bool> {
        let frame = frame_data(encoded)?;
        let chunk_size = frame_chunk_size(frame.bytes.len())?;
        let next_length = self
            .length
            .checked_add(u64::from(chunk_size) + 8)
            .context("recording file is too large")?;

        if next_length > self.max_bytes {
            return Ok(false);
        }

        // RIFF's size is patched only after recording; reject an impossible size now.
        u32::try_from(next_length - 8).context("recording exceeds the WebP file size limit")?;
        self.finish_previous_frame(timestamp)?;
        self.output.seek(SeekFrom::Start(self.length))?;
        self.output.write_all(b"ANMF")?;
        self.output.write_all(&chunk_size.to_le_bytes())?;
        let mut header = [0_u8; 16];

        header[6..9].copy_from_slice(&uint24(self.dimensions.0 - 1)?);
        header[9..12].copy_from_slice(&uint24(self.dimensions.1 - 1)?);
        // Full-canvas independent frames replace every pixel, including transparent ones.
        // Do not blend; do not dispose. No delta frame depends on an earlier frame.
        header[15] = 0x02;
        self.output.write_all(&header)?;
        self.output.write_all(frame.bytes)?;
        self.previous = Some(FramePosition {
            duration_offset: self.length + 20,
            timestamp,
        });
        self.length = next_length;
        self.has_alpha |= frame.has_alpha;

        Ok(true)
    }

    pub(super) fn finish(mut self, timestamp: i32) -> Result<(W, u64)> {
        anyhow::ensure!(self.previous.is_some(), "recording produced no frames");
        self.finish_previous_frame(timestamp)?;
        self.output.seek(SeekFrom::Start(20))?;
        self.output
            .write_all(&[0x02 | if self.has_alpha { 0x10 } else { 0 }])?;
        self.output.seek(SeekFrom::Start(4))?;
        self.output.write_all(
            &u32::try_from(self.length - 8)
                .context("recording exceeds the WebP file size limit")?
                .to_le_bytes(),
        )?;
        self.output.rewind()?;

        Ok((self.output, self.length))
    }

    fn finish_previous_frame(&mut self, timestamp: i32) -> Result<()> {
        let Some(previous) = &self.previous else {
            return Ok(());
        };
        let duration = timestamp
            .checked_sub(previous.timestamp)
            .and_then(|milliseconds| u32::try_from(milliseconds).ok())
            .filter(|milliseconds| *milliseconds > 0)
            .context("recording timestamps must increase")?;
        let duration_bytes = uint24(duration)?;

        self.output
            .seek(SeekFrom::Start(previous.duration_offset))?;
        self.output.write_all(&duration_bytes)?;

        Ok(())
    }
}

fn frame_chunk_size(data_length: usize) -> Result<u32> {
    u32::try_from(data_length)
        .ok()
        .and_then(|length| length.checked_add(16))
        .context("encoded recording frame is too large")
}

fn uint24(value: u32) -> Result<[u8; 3]> {
    anyhow::ensure!(value <= 0x00ff_ffff, "recording frame duration is too long");
    let bytes = value.to_le_bytes();

    Ok([bytes[0], bytes[1], bytes[2]])
}

struct EncodedFrame<'a> {
    bytes: &'a [u8],
    has_alpha: bool,
}

/// Extract the padded ALPH + VP8 or VP8L chunks of the bounded single-frame encoder.
fn frame_data(encoded: &[u8]) -> Result<EncodedFrame<'_>> {
    anyhow::ensure!(
        encoded.get(..4) == Some(b"RIFF") && encoded.get(8..12) == Some(b"WEBP"),
        "encoder returned an invalid WebP frame"
    );
    let riff_length = u32::from_le_bytes(encoded[4..8].try_into()?);

    anyhow::ensure!(
        u64::from(riff_length) + 8 == u64::try_from(encoded.len())?,
        "encoder returned an invalid WebP frame size"
    );
    let mut offset = 12_usize;
    let mut image_start = None;
    let mut image_end = None;
    let mut has_alpha = false;

    while offset < encoded.len() {
        let header_end = offset.checked_add(8).context("WebP frame is too large")?;
        let header = encoded
            .get(offset..header_end)
            .context("encoder returned a truncated WebP chunk")?;
        let length = usize::try_from(u32::from_le_bytes(header[4..8].try_into()?))?;
        let end = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(length % 2))
            .filter(|end| *end <= encoded.len())
            .context("encoder returned a truncated WebP chunk")?;

        match &header[..4] {
            b"VP8X" if image_start.is_none() && length == 10 => {}

            b"ALPH" if image_start.is_none() => {
                has_alpha = true;
                image_start = Some(offset);
            }

            b"VP8 " | b"VP8L" if image_end.is_none() => {
                if &header[..4] == b"VP8L" {
                    let alpha_byte = encoded
                        .get(header_end + 4)
                        .filter(|_| length >= 5)
                        .context("encoder returned a truncated lossless WebP frame")?;

                    has_alpha |= alpha_byte & 0x10 != 0;
                }

                image_start.get_or_insert(offset);
                image_end = Some(end);
            }

            _ => bail!("encoder returned an unexpected WebP frame chunk"),
        }

        offset = end;
    }

    Ok(EncodedFrame {
        bytes: &encoded[image_start.context("WebP frame has no pixels")?
            ..image_end.context("WebP frame has no pixels")?],
        has_alpha,
    })
}

#[cfg(test)]
mod tests {
    use super::{frame_chunk_size, frame_data, uint24};

    #[test]
    fn webp_field_arithmetic_rejects_overflow() {
        assert!(frame_chunk_size(usize::MAX).is_err());
        assert!(frame_chunk_size(usize::try_from(u32::MAX).unwrap()).is_err());
        assert_eq!(frame_chunk_size(20).unwrap(), 36);
        assert_eq!(uint24(0x00ff_ffff).unwrap(), [255; 3]);
        assert!(uint24(0x0100_0000).is_err());
    }

    #[test]
    fn malformed_encoder_output_is_rejected() {
        for bytes in [b"RIFF".as_slice(), b"RIFF\x04\0\0\0WEBPVP8 "] {
            assert!(frame_data(bytes).is_err());
        }
    }
}
