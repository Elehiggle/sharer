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

impl<W> WebpWriter<W>
where
    W: Write + Seek,
{
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
        let frame = frame_data(encoded, self.dimensions)?;
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

        header[..12].copy_from_slice(&frame.rectangle);
        // Each independently encoded crop has a transparent canvas outside its rectangle.
        // Clear the preceding rectangle to transparent, then overwrite the new rectangle.
        // This also preserves transparency when the encoder optimizes a frame to a crop.
        header[15] = 0x03; // Do not blend; dispose to the transparent background.
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
    rectangle: [u8; 12],
    has_alpha: bool,
}

/// Extract a static frame or the native encoder's optimized single-frame animation.
fn frame_data(encoded: &[u8], dimensions: (u32, u32)) -> Result<EncodedFrame<'_>> {
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
    let mut features = None;
    let mut animation_params = false;
    let mut image = None;
    let mut has_alpha = false;
    let mut rectangle = [0_u8; 12];

    rectangle[6..9].copy_from_slice(&uint24(dimensions.0 - 1)?);
    rectangle[9..12].copy_from_slice(&uint24(dimensions.1 - 1)?);

    while offset < encoded.len() {
        let chunk = read_chunk(encoded, offset)?;

        match chunk.kind {
            b"VP8X" if offset == 12 && chunk.body.len() == 10 => {
                anyhow::ensure!(
                    read_uint24(&chunk.body[4..7]) + 1 == dimensions.0
                        && read_uint24(&chunk.body[7..10]) + 1 == dimensions.1,
                    "encoder returned unexpected WebP canvas dimensions"
                );
                features = Some(chunk.body[0]);
                has_alpha = chunk.body[0] & 0x10 != 0;
            }

            b"ANIM"
                if !animation_params
                    && image.is_none()
                    && features.is_some_and(|flags| flags & 0x02 != 0)
                    && chunk.body.len() == 6 =>
            {
                animation_params = true;
            }

            b"ANMF" if animation_params && image.is_none() && chunk.body.len() >= 16 => {
                rectangle.copy_from_slice(&chunk.body[..12]);
                has_alpha |= validate_rectangle(&rectangle, dimensions)?;
                image = Some(image_chunks(&chunk.body[16..])?);
            }

            b"ALPH" | b"VP8 " | b"VP8L" if !animation_params && image.is_none() => {
                image = Some(image_chunks(&encoded[offset..])?);

                break;
            }

            _ => bail!("encoder returned an unexpected WebP frame chunk"),
        }

        offset = chunk.end;
    }

    let image = image.context("WebP frame has no pixels")?;

    Ok(EncodedFrame {
        bytes: image.bytes,
        rectangle,
        has_alpha: has_alpha || image.has_alpha,
    })
}

struct Chunk<'a> {
    kind: &'a [u8],
    body: &'a [u8],
    end: usize,
}

fn read_chunk(encoded: &[u8], offset: usize) -> Result<Chunk<'_>> {
    let header_end = offset.checked_add(8).context("WebP frame is too large")?;
    let header = encoded
        .get(offset..header_end)
        .context("encoder returned a truncated WebP chunk")?;
    let length = usize::try_from(u32::from_le_bytes(header[4..8].try_into()?))?;
    let body_end = header_end
        .checked_add(length)
        .context("WebP frame is too large")?;
    let end = body_end
        .checked_add(length % 2)
        .filter(|end| *end <= encoded.len())
        .context("encoder returned a truncated WebP chunk")?;

    Ok(Chunk {
        kind: &header[..4],
        body: &encoded[header_end..body_end],
        end,
    })
}

struct EncodedImage<'a> {
    bytes: &'a [u8],
    has_alpha: bool,
}

/// Extract the padded ALPH + VP8 or VP8L chunks, retaining no pixel or byte copies.
fn image_chunks(chunks: &[u8]) -> Result<EncodedImage<'_>> {
    let mut offset = 0;
    let mut has_alpha = false;
    let mut has_pixels = false;

    while offset < chunks.len() {
        let chunk = read_chunk(chunks, offset)?;

        match chunk.kind {
            b"ALPH" if !has_alpha && !has_pixels => has_alpha = true,

            b"VP8 " if !has_pixels => has_pixels = true,

            b"VP8L" if !has_alpha && !has_pixels && chunk.body.len() >= 5 => {
                has_alpha = chunk.body[4] & 0x10 != 0;
                has_pixels = true;
            }

            _ => bail!("encoder returned an unexpected WebP image chunk"),
        }

        offset = chunk.end;
    }

    anyhow::ensure!(has_pixels, "WebP frame has no pixels");

    Ok(EncodedImage {
        bytes: chunks,
        has_alpha,
    })
}

fn read_uint24(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

/// Return whether a cropped frame leaves part of its independent canvas transparent.
fn validate_rectangle(rectangle: &[u8; 12], dimensions: (u32, u32)) -> Result<bool> {
    let x = read_uint24(&rectangle[..3]) * 2;
    let y = read_uint24(&rectangle[3..6]) * 2;
    let width = read_uint24(&rectangle[6..9]) + 1;
    let height = read_uint24(&rectangle[9..12]) + 1;

    anyhow::ensure!(
        x + width <= dimensions.0 && y + height <= dimensions.1,
        "encoder returned an invalid WebP frame rectangle"
    );

    Ok(x != 0 || y != 0 || width != dimensions.0 || height != dimensions.1)
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
            assert!(frame_data(bytes, (2, 2)).is_err());
        }
    }
}
