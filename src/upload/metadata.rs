//! Bounded, lossless removal of identifying image metadata.

use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};

use anyhow::{Context, Result, bail};

use super::ReadSeek;

const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const MAX_XMP_BYTES: usize = 64 * 1024;
const MAX_MPF_IMAGES: usize = 32;
const IO_BUFFER_BYTES: usize = 1024;

/// Keep small container reads and forward skips inside one bounded input buffer.
/// `BufReader::seek(Start(_))` would otherwise discard that buffer for every chunk.
struct BufferedImage<'a> {
    reader: BufReader<&'a mut dyn ReadSeek>,
    position: u64,
}

impl<'a> BufferedImage<'a> {
    fn new(reader: &'a mut dyn ReadSeek) -> Result<Self> {
        reader.seek(SeekFrom::Start(0))?;
        Ok(Self {
            reader: BufReader::with_capacity(IO_BUFFER_BYTES, reader),
            position: 0,
        })
    }
}

impl Read for BufferedImage<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(bytes)?;
        self.position += u64::try_from(count).map_err(io::Error::other)?;
        Ok(count)
    }
}

impl Seek for BufferedImage<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(target) => target,
            SeekFrom::Current(offset) => self
                .position
                .checked_add_signed(offset)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid image seek"))?,
            SeekFrom::End(offset) => {
                self.position = self.reader.seek(SeekFrom::End(offset))?;
                return Ok(self.position);
            }
        };
        let distance = i128::from(target) - i128::from(self.position);
        if let Ok(offset) = i64::try_from(distance) {
            self.reader.seek_relative(offset)?;
        } else {
            self.reader.seek(SeekFrom::Start(target))?;
        }
        self.position = target;
        Ok(target)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MetadataFormat {
    Jpeg,
    Png,
    Webp,
}

impl MetadataFormat {
    pub(super) const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }
}

pub(super) fn sniff(bytes: &[u8]) -> Option<MetadataFormat> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        Some(MetadataFormat::Jpeg)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(MetadataFormat::Png)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(MetadataFormat::Webp)
    } else {
        None
    }
}

pub(super) fn sniff_reader(
    reader: &mut dyn ReadSeek,
    length: u64,
) -> Result<Option<MetadataFormat>> {
    verify_length(reader, length)?;
    let mut header = [0; 12];
    let count = usize::try_from(length.min(12)).unwrap_or(12);

    reader
        .read_exact(&mut header[..count])
        .context("failed to inspect image signature")?;
    reader.seek(SeekFrom::Start(0))?;
    Ok(sniff(&header[..count]))
}

/// Copy only changed file/reader payloads to a private, automatically deleted file.
pub(super) fn strip_to_file(
    reader: &mut dyn ReadSeek,
    length: u64,
    format: MetadataFormat,
) -> Result<Option<(std::fs::File, u64)>> {
    let mut buffered = BufferedImage::new(reader)?;
    let reader: &mut dyn ReadSeek = &mut buffered;
    verify_length(reader, length)?;
    let prepared = prepare(reader, length, format)?;
    reader.seek(SeekFrom::Start(0))?;
    if !prepared.changed {
        return Ok(None);
    }

    let output = tempfile::tempfile().context("failed to create private image temporary file")?;
    let mut output = BufWriter::with_capacity(IO_BUFFER_BYTES, output);
    let mut position = 0;
    while let Some(piece) = next_piece(reader, length, format, &mut position)? {
        if matches!(piece.kind, PieceKind::Drop) {
            continue;
        }
        if let Some(mpf) = prepared.mpf.as_ref().filter(|mpf| mpf.start == piece.start) {
            output.write_all(&mpf.bytes)?;
            continue;
        }
        reader.seek(SeekFrom::Start(piece.start))?;
        if matches!(piece.kind, PieceKind::WebpFlags) {
            let mut prefix = read_exact::<9>(reader)?;
            prefix[8] &= !0x0c; // EXIF and XMP flags; ICC and animation flags are retained.
            output.write_all(&prefix)?;
            copy_exact(reader, &mut output, piece.end - piece.start - 9)?;
        } else {
            copy_exact(reader, &mut output, piece.end - piece.start)?;
        }
    }
    if format == MetadataFormat::Webp {
        let size = u32::try_from(output.stream_position()?.saturating_sub(8))
            .context("WebP upload is too large")?;
        output.seek(SeekFrom::Start(4))?;
        output.write_all(&size.to_le_bytes())?;
    }
    let output_length = output.seek(SeekFrom::End(0))?;
    output.seek(SeekFrom::Start(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let output = output
        .into_inner()
        .map_err(io::IntoInnerError::into_error)
        .context("failed to finish cleaned image temporary file")?;
    Ok(Some((output, output_length)))
}

pub(super) fn strip_in_place(bytes: &mut Vec<u8>, format: MetadataFormat) -> Result<bool> {
    let length = u64::try_from(bytes.len()).context("image is too large")?;
    // Validate everything, including HDR metadata and MPF references, before changing bytes.
    let prepared = prepare(&mut Cursor::new(bytes.as_slice()), length, format)?;
    if !prepared.changed {
        return Ok(false);
    }
    let mut position = 0;
    let mut write_position = 0;
    while let Some(piece) = next_piece(
        &mut Cursor::new(bytes.as_slice()),
        length,
        format,
        &mut position,
    )? {
        let start = usize::try_from(piece.start)?;
        let end = usize::try_from(piece.end)?;
        if matches!(piece.kind, PieceKind::Drop) {
            continue;
        }
        if let Some(mpf) = prepared.mpf.as_ref().filter(|mpf| mpf.start == piece.start) {
            bytes[write_position..write_position + mpf.bytes.len()].copy_from_slice(&mpf.bytes);
        } else {
            if matches!(piece.kind, PieceKind::WebpFlags) {
                bytes[start + 8] &= !0x0c;
            }
            bytes.copy_within(start..end, write_position);
        }
        write_position += end - start;
    }
    bytes.truncate(write_position);
    if format == MetadataFormat::Webp {
        let size =
            u32::try_from(bytes.len().saturating_sub(8)).context("WebP upload is too large")?;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
    }
    Ok(true)
}

fn verify_length(reader: &mut dyn ReadSeek, length: u64) -> Result<()> {
    anyhow::ensure!(
        reader.seek(SeekFrom::End(0))? == length,
        "image length changed during inspection"
    );
    reader.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn copy_exact(reader: &mut dyn ReadSeek, writer: &mut impl Write, length: u64) -> Result<()> {
    // A fixed stack buffer keeps heap usage independent of image/chunk size.
    let mut remaining = length;
    let mut buffer = [0; 16 * 1024];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(u64::try_from(buffer.len())?))?;
        reader
            .read_exact(&mut buffer[..count])
            .context("image ended during metadata removal")?;
        writer.write_all(&buffer[..count])?;
        remaining -= u64::try_from(count)?;
    }
    Ok(())
}

fn read_exact<const N: usize>(reader: &mut dyn ReadSeek) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    reader
        .read_exact(&mut bytes)
        .context("image has truncated metadata or container data")?;
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum PieceKind {
    Keep,
    Drop,
    Mpf,
    WebpFlags,
    JpegEnd,
}
struct Piece {
    start: u64,
    end: u64,
    kind: PieceKind,
}
struct Mpf {
    start: u64,
    bytes: Vec<u8>,
}
struct Prepared {
    changed: bool,
    mpf: Option<Mpf>,
}

fn prepare(reader: &mut dyn ReadSeek, length: u64, format: MetadataFormat) -> Result<Prepared> {
    let mut position = 0;
    let mut changed = false;
    let mut mpf = None;
    while let Some(piece) = next_piece(reader, length, format, &mut position)? {
        changed |= matches!(piece.kind, PieceKind::Drop);
        if matches!(piece.kind, PieceKind::Mpf) {
            anyhow::ensure!(
                mpf.is_none(),
                "multiple JPEG MPF tables cannot be safely rewritten"
            );
            reader.seek(SeekFrom::Start(piece.start))?;
            let mut bytes = vec![0; usize::try_from(piece.end - piece.start)?];
            reader.read_exact(&mut bytes)?;
            mpf = Some(Mpf {
                start: piece.start,
                bytes,
            });
        }
    }
    if changed && let Some(mpf) = &mut mpf {
        repair_mpf(reader, length, mpf)?;
    }
    Ok(Prepared { changed, mpf })
}

fn next_piece(
    reader: &mut dyn ReadSeek,
    length: u64,
    format: MetadataFormat,
    position: &mut u64,
) -> Result<Option<Piece>> {
    if *position == length {
        return Ok(None);
    }
    anyhow::ensure!(*position < length, "image container length overflow");
    reader.seek(SeekFrom::Start(*position))?;
    let start = *position;
    let (end, kind) = if start == 0 {
        let header_length = match format {
            MetadataFormat::Jpeg => 2,
            MetadataFormat::Png => 8,
            MetadataFormat::Webp => 12,
        };
        anyhow::ensure!(length >= header_length, "image has a truncated header");
        if format == MetadataFormat::Webp {
            let header = read_exact::<12>(reader)?;
            anyhow::ensure!(
                u64::from(u32::from_le_bytes(header[4..8].try_into()?)) + 8 == length,
                "WebP container length is inconsistent"
            );
        }
        (header_length, PieceKind::Keep)
    } else {
        match format {
            MetadataFormat::Jpeg => jpeg_piece(reader, length, start)?,
            MetadataFormat::Png => {
                let chunk = read_exact::<8>(reader)?;
                let size = u64::from(u32::from_be_bytes(chunk[..4].try_into()?));
                let end = checked_end(start, 12, size, length)?;
                let kind = if matches!(&chunk[4..], b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt") {
                    PieceKind::Drop
                } else {
                    PieceKind::Keep
                };
                (end, kind)
            }
            MetadataFormat::Webp => {
                let chunk = read_exact::<8>(reader)?;
                let size = u64::from(u32::from_le_bytes(chunk[4..].try_into()?));
                let end = checked_end(start, 8, size + size % 2, length)?;
                let kind = match &chunk[..4] {
                    b"EXIF" | b"XMP " => PieceKind::Drop,
                    b"VP8X" => {
                        anyhow::ensure!(size >= 10, "WebP VP8X chunk is truncated");
                        PieceKind::WebpFlags
                    }
                    _ => PieceKind::Keep,
                };
                (end, kind)
            }
        }
    };
    *position = end;
    Ok(Some(Piece { start, end, kind }))
}

fn checked_end(start: u64, overhead: u64, data: u64, length: u64) -> Result<u64> {
    let end = start
        .checked_add(overhead)
        .and_then(|end| end.checked_add(data))
        .context("image chunk length overflow")?;
    anyhow::ensure!(end <= length, "image has a truncated chunk or segment");
    Ok(end)
}

fn jpeg_piece(reader: &mut dyn ReadSeek, length: u64, start: u64) -> Result<(u64, PieceKind)> {
    anyhow::ensure!(
        read_exact::<1>(reader)?[0] == 0xff,
        "JPEG has unexpected data outside an image scan"
    );
    let mut marker = read_exact::<1>(reader)?[0];
    let mut marker_position = start + 1;
    while marker == 0xff {
        marker = read_exact::<1>(reader)?[0];
        marker_position += 1;
    }
    let after_marker = marker_position + 1;
    if marker == 0xd9 {
        return Ok((after_marker, PieceKind::JpegEnd));
    }
    if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
        return Ok((after_marker, PieceKind::Keep));
    }
    anyhow::ensure!(marker != 0, "JPEG has an escaped marker outside a scan");
    let segment_length = u64::from(u16::from_be_bytes(read_exact::<2>(reader)?));
    anyhow::ensure!(segment_length >= 2, "JPEG has an invalid segment length");
    let end = checked_end(after_marker, 0, segment_length, length)?;
    if marker == 0xda {
        return Ok((scan_end(reader, end, length)?, PieceKind::Keep));
    }
    if marker == 0xfe || marker == 0xed {
        return Ok((end, PieceKind::Drop));
    }
    if marker == 0xe1 || marker == 0xe2 {
        let mut prefix = [0; 35];
        let count = usize::try_from((segment_length - 2).min(u64::try_from(prefix.len())?))?;
        reader.read_exact(&mut prefix[..count])?;
        if marker == 0xe2 && prefix[..count].starts_with(b"MPF\0") {
            anyhow::ensure!(
                after_marker == start + 2,
                "padded MPF marker cannot be safely rewritten"
            );
            return Ok((end, PieceKind::Mpf));
        }
        if marker == 0xe1 {
            if prefix[..count].starts_with(b"Exif\0\0")
                || prefix[..count].starts_with(b"http://ns.adobe.com/xmp/extension/\0")
            {
                return Ok((end, PieceKind::Drop));
            }
            if prefix[..count].starts_with(XMP_HEADER) {
                let size = usize::try_from(segment_length - 2)?;
                anyhow::ensure!(
                    size <= MAX_XMP_BYTES,
                    "XMP packet is too large to inspect safely"
                );
                reader.seek(SeekFrom::Start(
                    after_marker + 2 + u64::try_from(XMP_HEADER.len())?,
                ))?;
                let mut xml = vec![0; size - XMP_HEADER.len()];
                reader.read_exact(&mut xml)?;
                return Ok((
                    end,
                    if is_rendering_xmp(&xml)? {
                        PieceKind::Keep
                    } else {
                        PieceKind::Drop
                    },
                ));
            }
        }
    }
    Ok((end, PieceKind::Keep))
}

fn scan_end(reader: &mut dyn ReadSeek, start: u64, length: u64) -> Result<u64> {
    reader.seek(SeekFrom::Start(start))?;
    let mut buffer = [0; 1024];
    let mut position = start;
    let mut marker_start = None;
    while position < length {
        let count = usize::try_from((length - position).min(u64::try_from(buffer.len())?))?;
        reader.read_exact(&mut buffer[..count])?;
        for (index, byte) in buffer[..count].iter().copied().enumerate() {
            if let Some(candidate) = marker_start {
                match byte {
                    0xff => {}
                    0x00 | 0xd0..=0xd7 => marker_start = None,
                    _ => return Ok(candidate),
                }
            } else if byte == 0xff {
                marker_start = Some(position + u64::try_from(index)?);
            }
        }
        position += u64::try_from(count)?;
    }
    bail!("JPEG image scan has no end marker")
}

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const META: &str = "adobe:ns:meta/";
const ADOBE_HDR: &str = "http://ns.adobe.com/hdr-gain-map/1.0/";
const APPLE_HDR: &str = "http://ns.apple.com/HDRGainMap/1.0/";
const APPLE_PIXEL: &str = "http://ns.apple.com/pixeldatainfo/1.0/";
const CONTAINER: &str = "http://ns.google.com/photos/1.0/container/";
const ITEM: &str = "http://ns.google.com/photos/1.0/container/item/";

fn hdr_namespace(namespace: Option<&str>) -> bool {
    matches!(
        namespace,
        Some(ADOBE_HDR | APPLE_HDR | APPLE_PIXEL | CONTAINER | ITEM)
    )
}

/// Keep complete HDR packets only when every field is recognized and constrained.
/// Mixed packets fail closed instead of dropping gain-map semantics or retaining location data.
fn is_rendering_xmp(xml: &[u8]) -> Result<bool> {
    let text = std::str::from_utf8(xml)
        .context("XMP is not UTF-8; cannot safely preserve image rendering metadata")?;
    let options = roxmltree::ParsingOptions {
        nodes_limit: 4096,
        ..Default::default()
    };
    let document = roxmltree::Document::parse_with_options(text, options)
        .context("XMP cannot be safely inspected")?;
    let has_hdr = document.descendants().any(|node| {
        hdr_namespace(node.tag_name().namespace())
            || node
                .attributes()
                .any(|attribute| hdr_namespace(attribute.namespace()))
    });
    if !has_hdr {
        return Ok(false);
    }
    for node in document.descendants() {
        if node.is_comment() || node.is_pi() {
            bail!(
                "HDR XMP contains comments or processing instructions; identifying metadata cannot be safely removed"
            );
        }
        if node.is_element() {
            for namespace in node.namespaces() {
                let known = matches!(
                    (namespace.name(), namespace.uri()),
                    (Some("x"), META)
                        | (Some("rdf"), RDF)
                        | (Some("hdrgm"), ADOBE_HDR)
                        | (Some("HDRGainMap"), APPLE_HDR)
                        | (Some("apdi"), APPLE_PIXEL)
                        | (Some("Container"), CONTAINER)
                        | (Some("Item"), ITEM)
                );
                anyhow::ensure!(
                    known,
                    "HDR XMP contains unsupported namespace declarations; identifying metadata cannot be safely removed"
                );
            }
            let name = node.tag_name();
            let wrapper = matches!(
                (name.namespace(), name.name()),
                (Some(META), "xmpmeta")
                    | (Some(RDF), "RDF" | "Description" | "Seq" | "Bag" | "li")
                    | (Some(CONTAINER), "Directory")
                    | (Some(ITEM), "Item")
            );
            anyhow::ensure!(
                wrapper || safe_hdr_value(name.namespace(), name.name(), node.text().unwrap_or("")),
                "HDR XMP contains unsupported fields; identifying metadata cannot be safely removed without changing HDR rendering"
            );
            for attribute in node.attributes() {
                let safe =
                    matches!(
                        (attribute.namespace(), attribute.name(), attribute.value()),
                        (Some(RDF), "about", "")
                            | (Some(RDF), "parseType", "Resource")
                            | (Some(META), "xmptk", "XMP Core 6.0.0")
                    ) || safe_hdr_value(attribute.namespace(), attribute.name(), attribute.value());
                anyhow::ensure!(
                    safe,
                    "HDR XMP contains unsupported attributes; identifying metadata cannot be safely removed without changing HDR rendering"
                );
            }
        } else if node.is_text() {
            let text = node.text().unwrap_or("");
            if !text.trim().is_empty() {
                let parent = node
                    .parent_element()
                    .context("HDR XMP has unexpected text")?;
                let name = parent.tag_name();
                anyhow::ensure!(
                    safe_hdr_value(name.namespace(), name.name(), text)
                        || (name.namespace() == Some(RDF) && name.name() == "li" && numeric(text)),
                    "HDR XMP contains unsupported text; identifying metadata cannot be safely removed"
                );
            }
        }
    }
    Ok(true)
}

fn numeric(value: &str) -> bool {
    value.trim().parse::<f64>().is_ok_and(f64::is_finite)
}
fn safe_hdr_value(namespace: Option<&str>, name: &str, value: &str) -> bool {
    match (namespace, name) {
        (Some(APPLE_PIXEL), "AuxiliaryImageType") => {
            value == "urn:com:apple:photo:2020:aux:hdrgainmap"
        }
        (Some(APPLE_PIXEL), "NativeFormat" | "StoredFormat")
        | (Some(APPLE_HDR), "HDRGainMapVersion" | "HDRGainMapHeadroom") => numeric(value),
        (Some(ADOBE_HDR), "BaseRenditionIsHDR") => matches!(value, "True" | "False"),
        (
            Some(ADOBE_HDR),
            "Version" | "GainMapMin" | "GainMapMax" | "Gamma" | "OffsetSDR" | "OffsetHDR"
            | "HDRCapacityMin" | "HDRCapacityMax",
        ) => value.trim().is_empty() || numeric(value),
        (Some(ITEM), "Mime") => value == "image/jpeg",
        (Some(ITEM), "Semantic") => matches!(value, "Primary" | "GainMap"),
        (Some(ITEM), "Length" | "Padding") => value.parse::<u64>().is_ok(),
        _ => false,
    }
}

fn removed_before(reader: &mut dyn ReadSeek, length: u64, cutoff: u64) -> Result<u64> {
    let mut position = 0;
    let mut removed = 0;
    while let Some(piece) = next_piece(reader, length, MetadataFormat::Jpeg, &mut position)? {
        if piece.start >= cutoff {
            break;
        }
        if matches!(piece.kind, PieceKind::Drop) {
            anyhow::ensure!(piece.end <= cutoff, "MPF reference points inside metadata");
            removed += piece.end - piece.start;
        }
    }
    Ok(removed)
}

fn jpeg_end(reader: &mut dyn ReadSeek, length: u64, start: u64) -> Result<u64> {
    reader.seek(SeekFrom::Start(start))?;
    anyhow::ensure!(
        read_exact::<2>(reader)? == [0xff, 0xd8],
        "MPF image reference does not point to a JPEG image"
    );
    let mut position = start + 2;
    while let Some(piece) = next_piece(reader, length, MetadataFormat::Jpeg, &mut position)? {
        if matches!(piece.kind, PieceKind::JpegEnd) {
            return Ok(piece.end);
        }
    }
    bail!("MPF image has no end marker")
}

fn repair_mpf(reader: &mut dyn ReadSeek, length: u64, mpf: &mut Mpf) -> Result<()> {
    // MPF offsets are relative to the TIFF header, whereas the first image offset is zero.
    let tiff = 8_usize;
    anyhow::ensure!(mpf.bytes.len() >= tiff + 8, "MPF TIFF header is truncated");
    let little = match &mpf.bytes[tiff..tiff + 4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => bail!("MPF TIFF header is invalid"),
    };
    let ifd = tiff + usize::try_from(read_u32(&mpf.bytes, tiff + 4, little)?)?;
    let count = usize::from(read_u16(&mpf.bytes, ifd, little)?);
    let mut entries = None;
    for index in 0..count {
        let tag = ifd
            .checked_add(2 + index * 12)
            .context("MPF directory overflow")?;
        if read_u16(&mpf.bytes, tag, little)? == 0xb002 {
            anyhow::ensure!(
                read_u16(&mpf.bytes, tag + 2, little)? == 7,
                "MPF entry table has an invalid type"
            );
            let size = usize::try_from(read_u32(&mpf.bytes, tag + 4, little)?)?;
            anyhow::ensure!(
                size % 16 == 0 && size / 16 <= MAX_MPF_IMAGES,
                "MPF image count exceeds the supported safe limit"
            );
            let offset = tiff + usize::try_from(read_u32(&mpf.bytes, tag + 8, little)?)?;
            anyhow::ensure!(
                offset
                    .checked_add(size)
                    .is_some_and(|end| end <= mpf.bytes.len()),
                "MPF entry table is truncated"
            );
            entries = Some((offset, size / 16));
        }
    }
    let (offset, count) = entries.context("MPF entry table is missing")?;
    let tiff_position = mpf.start + u64::try_from(tiff)?;
    let new_tiff = tiff_position - removed_before(reader, length, tiff_position)?;
    for index in 0..count {
        let entry = offset + index * 16;
        let old_offset = read_u32(&mpf.bytes, entry + 8, little)?;
        let start = if old_offset == 0 {
            0
        } else {
            tiff_position + u64::from(old_offset)
        };
        let end = jpeg_end(reader, length, start)?;
        let new_start = start - removed_before(reader, length, start)?;
        let new_end = end - removed_before(reader, length, end)?;
        write_u32(
            &mut mpf.bytes,
            entry + 4,
            u32::try_from(new_end - new_start)?,
            little,
        );
        let new_offset = if old_offset == 0 {
            0
        } else {
            u32::try_from(
                new_start
                    .checked_sub(new_tiff)
                    .context("MPF image precedes its table")?,
            )?
        };
        write_u32(&mut mpf.bytes, entry + 8, new_offset, little);
    }
    Ok(())
}

fn read_u16(bytes: &[u8], position: usize, little: bool) -> Result<u16> {
    let bytes: [u8; 2] = bytes
        .get(position..position + 2)
        .context("MPF directory is truncated")?
        .try_into()?;
    Ok(if little {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    })
}
fn read_u32(bytes: &[u8], position: usize, little: bool) -> Result<u32> {
    let bytes: [u8; 4] = bytes
        .get(position..position + 4)
        .context("MPF directory is truncated")?
        .try_into()?;
    Ok(if little {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}
fn write_u32(bytes: &mut [u8], position: usize, value: u32, little: bool) {
    bytes[position..position + 4].copy_from_slice(&if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::{PayloadBody, UploadPayload};

    const SDR: &[u8] = include_bytes!("../../tests/fixtures/images/minnie_sdr_with_exif.jpg");
    const HDR: &[u8] = include_bytes!("../../tests/fixtures/images/apple_gainmap_new.jpg");

    fn payload_bytes(payload: &mut UploadPayload) -> Vec<u8> {
        match &mut payload.body {
            PayloadBody::Bytes(bytes) => bytes.clone(),
            PayloadBody::File { file, .. } => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                bytes
            }
            PayloadBody::Reader { reader, .. } => {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).unwrap();
                bytes
            }
        }
    }

    fn jpeg_icc_payload(bytes: &[u8]) -> Vec<u8> {
        let mut position = 2;
        let mut profile = Vec::new();
        while position < bytes.len() {
            assert_eq!(bytes[position], 0xff);
            let marker = bytes[position + 1];
            if marker == 0xda || marker == 0xd9 {
                break;
            }
            let size = usize::from(u16::from_be_bytes([
                bytes[position + 2],
                bytes[position + 3],
            ]));
            let data = &bytes[position + 4..position + 2 + size];
            if marker == 0xe2 && data.starts_with(b"ICC_PROFILE\0") {
                profile.extend_from_slice(data);
            }
            position += 2 + size;
        }
        assert!(!profile.is_empty());
        profile
    }

    fn jpeg_segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, marker];
        bytes.extend_from_slice(&u16::try_from(data.len() + 2).unwrap().to_be_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    fn jpeg_with_xmp(xml: &str) -> Vec<u8> {
        let mut data = XMP_HEADER.to_vec();
        data.extend_from_slice(xml.as_bytes());
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend_from_slice(&jpeg_segment(0xe1, &data));
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        jpeg
    }

    fn png_chunk(bytes: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&[0; 4]);
    }

    #[test]
    fn renamed_and_extensionless_images_are_cleaned_for_all_payload_backings() {
        let directory = tempfile::tempdir().unwrap();
        for filename in ["photo.txt", "photo"] {
            let path = directory.path().join(filename);
            std::fs::write(&path, SDR).unwrap();
            let mut payloads = [
                UploadPayload::from_path(&path).unwrap(),
                UploadPayload::from_reader(
                    Cursor::new(SDR),
                    u64::try_from(SDR.len()).unwrap(),
                    filename.into(),
                    "application/octet-stream".into(),
                ),
                UploadPayload::from_bytes(SDR.to_vec(), filename.into(), "text/plain".into()),
            ];
            for payload in &mut payloads {
                payload.remove_exif().unwrap();
                assert_eq!(payload.mime, "image/jpeg");
                let stripped = payload_bytes(payload);
                let metadata = metastrip::extract_metadata(&stripped).unwrap();
                assert!(metadata.exif.is_none());
                assert!(metadata.icc_profile.is_some());
                assert_eq!(
                    image::load_from_memory(&stripped).unwrap().to_rgba8(),
                    image::load_from_memory(SDR).unwrap().to_rgba8()
                );
            }
        }
    }

    #[test]
    fn cleaned_file_and_reader_payloads_stay_streamed_and_source_is_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.jpg");
        std::fs::write(&path, HDR).unwrap();
        let mut payloads = [
            UploadPayload::from_path(&path).unwrap(),
            UploadPayload::from_reader(
                Cursor::new(HDR),
                u64::try_from(HDR.len()).unwrap(),
                "image.jpg".into(),
                "image/jpeg".into(),
            ),
        ];
        let mut expected = HDR.to_vec();
        strip_in_place(&mut expected, MetadataFormat::Jpeg).unwrap();
        for payload in &mut payloads {
            payload.remove_exif().unwrap();
            assert!(matches!(payload.body, PayloadBody::Reader { .. }));
            assert_eq!(payload.len(), u64::try_from(expected.len()).unwrap());
            assert_eq!(payload_bytes(payload), expected);
        }
        assert_eq!(std::fs::read(path).unwrap(), HDR);
    }

    #[test]
    fn ordinary_nonimage_payloads_are_unchanged_and_mislabeled_images_fail() {
        let bytes = vec![42; 100_000];
        let mut payload = UploadPayload::from_bytes(
            bytes.clone(),
            "data.bin".into(),
            "application/octet-stream".into(),
        );
        payload.remove_exif().unwrap();
        assert_eq!(payload.bytes().unwrap(), bytes);
        let mut invalid = UploadPayload::from_bytes(bytes, "image.png".into(), "image/png".into());
        assert!(invalid.remove_exif().is_err());
    }

    #[test]
    fn xmp_only_location_iptc_and_comments_are_removed() {
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/" exif:GPSLatitude="test-location"/></rdf:RDF></x:xmpmeta>"#;
        let mut jpeg = jpeg_with_xmp(xml);
        jpeg.splice(2..2, jpeg_segment(0xed, b"Photoshop 3.0\0IPTC location"));
        jpeg.splice(2..2, jpeg_segment(0xfe, b"author"));
        strip_in_place(&mut jpeg, MetadataFormat::Jpeg).unwrap();
        assert_eq!(jpeg, [0xff, 0xd8, 0xff, 0xd9]);
    }

    #[test]
    fn png_text_and_exif_are_removed_while_color_and_image_chunks_survive() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        for kind in [b"eXIf", b"tEXt", b"zTXt", b"iTXt"] {
            png_chunk(&mut png, kind, b"location");
        }
        let mut expected = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [
            (b"iCCP", b"profile".as_slice()),
            (b"cICP", b"color".as_slice()),
            (b"IDAT", b"pixels".as_slice()),
            (b"IEND", b"".as_slice()),
        ] {
            png_chunk(&mut png, kind, data);
            png_chunk(&mut expected, kind, data);
        }
        strip_in_place(&mut png, MetadataFormat::Png).unwrap();
        assert_eq!(png, expected);
    }

    #[test]
    fn webp_xmp_and_exif_are_removed_and_flags_and_length_are_updated() {
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        for (kind, data) in [
            (b"VP8X", &[0x2c, 0, 0, 0, 0, 0, 0, 0, 0, 0][..]),
            (b"EXIF", b"GPS".as_slice()),
            (b"XMP ", b"location".as_slice()),
            (b"ICCP", b"profile".as_slice()),
            (b"VP8 ", b"pixels".as_slice()),
        ] {
            webp.extend_from_slice(kind);
            webp.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            webp.extend_from_slice(data);
            if data.len() % 2 == 1 {
                webp.push(0);
            }
        }
        let size = u32::try_from(webp.len() - 8).unwrap();
        webp[4..8].copy_from_slice(&size.to_le_bytes());
        strip_in_place(&mut webp, MetadataFormat::Webp).unwrap();
        assert_eq!(webp[20], 0x20);
        assert_eq!(
            usize::try_from(u32::from_le_bytes(webp[4..8].try_into().unwrap())).unwrap() + 8,
            webp.len()
        );
        let mut position = 0;
        while let Some(piece) = next_piece(
            &mut Cursor::new(webp.as_slice()),
            u64::try_from(webp.len()).unwrap(),
            MetadataFormat::Webp,
            &mut position,
        )
        .unwrap()
        {
            assert!(!matches!(piece.kind, PieceKind::Drop));
        }
        assert!(webp.windows(7).any(|bytes| bytes == b"profile"));
        assert!(webp.windows(6).any(|bytes| bytes == b"pixels"));
    }

    #[test]
    fn hdr_gainmap_pixels_xmp_and_mpf_references_survive() {
        let mut stripped = HDR.to_vec();
        strip_in_place(&mut stripped, MetadataFormat::Jpeg).unwrap();
        assert_eq!(
            image::load_from_memory(&stripped).unwrap().to_rgba8(),
            image::load_from_memory(HDR).unwrap().to_rgba8()
        );
        let mut position = 0;
        let mut mpf = None;
        let mut image_starts = vec![0];
        while let Some(piece) = next_piece(
            &mut Cursor::new(stripped.as_slice()),
            u64::try_from(stripped.len()).unwrap(),
            MetadataFormat::Jpeg,
            &mut position,
        )
        .unwrap()
        {
            assert!(!matches!(piece.kind, PieceKind::Drop));
            if matches!(piece.kind, PieceKind::Mpf) {
                mpf = Some(usize::try_from(piece.start).unwrap());
            }
            if matches!(piece.kind, PieceKind::JpegEnd)
                && piece.end < u64::try_from(stripped.len()).unwrap()
            {
                image_starts.push(usize::try_from(piece.end).unwrap());
            }
        }
        let start = mpf.unwrap();
        let tiff = start + 8;
        let ifd = tiff + usize::try_from(read_u32(&stripped, tiff + 4, false).unwrap()).unwrap();
        let count = usize::from(read_u16(&stripped, ifd, false).unwrap());
        let table_tag = (0..count)
            .map(|index| ifd + 2 + index * 12)
            .find(|tag| read_u16(&stripped, *tag, false).unwrap() == 0xb002)
            .unwrap();
        let table =
            tiff + usize::try_from(read_u32(&stripped, table_tag + 8, false).unwrap()).unwrap();
        for (index, image_start) in image_starts.iter().copied().enumerate() {
            let entry = table + index * 16;
            let offset = usize::try_from(read_u32(&stripped, entry + 8, false).unwrap()).unwrap();
            assert_eq!(if index == 0 { 0 } else { tiff + offset }, image_start);
            let end = usize::try_from(
                jpeg_end(
                    &mut Cursor::new(stripped.as_slice()),
                    u64::try_from(stripped.len()).unwrap(),
                    u64::try_from(image_start).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                usize::try_from(read_u32(&stripped, entry + 4, false).unwrap()).unwrap(),
                end - image_start
            );
        }
        let before_secondary = &HDR[44821..];
        let after_secondary = &stripped[image_starts[1]..];
        assert_eq!(
            image::load_from_memory(before_secondary)
                .unwrap()
                .to_rgba8(),
            image::load_from_memory(after_secondary).unwrap().to_rgba8()
        );
        assert!(
            after_secondary
                .windows(13)
                .any(|bytes| bytes == b"HDRGainMapVer")
        );
        // Compare ICC payloads directly: the general metadata library does not parse
        // this concatenated HDR JPEG container reliably, although both images decode.
        assert_eq!(jpeg_icc_payload(&stripped), jpeg_icc_payload(HDR));
    }

    #[test]
    fn mixed_or_spoofed_hdr_xmp_is_rejected_before_changing_the_payload() {
        for extra in [
            r#"<evil:GPS xmlns:evil="http://ns.adobe.com/exif/1.0/">location</evil:GPS>"#,
            "<!-- location -->",
            r#"<rdf:Description rdf:about="personal-name"/>"#,
            r"<HDRGainMap:HDRGainMapHeadroom><![CDATA[location]]></HDRGainMap:HDRGainMapHeadroom>",
            r"<?private location?>",
        ] {
            let xml = format!(
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:HDRGainMap="http://ns.apple.com/HDRGainMap/1.0/"><HDRGainMap:HDRGainMapVersion>131072</HDRGainMap:HDRGainMapVersion>{extra}</rdf:Description></rdf:RDF></x:xmpmeta>"#
            );
            let mut jpeg = jpeg_with_xmp(&xml);
            jpeg.splice(2..2, jpeg_segment(0xe1, b"Exif\0\0location"));
            let original = jpeg.clone();
            assert!(strip_in_place(&mut jpeg, MetadataFormat::Jpeg).is_err());
            assert_eq!(jpeg, original);
        }
        let aliased = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:HDRGainMap="http://ns.adobe.com/exif/1.0/" HDRGainMap:GPSLatitude="location"/></rdf:RDF></x:xmpmeta>"#;
        let mut jpeg = jpeg_with_xmp(aliased);
        strip_in_place(&mut jpeg, MetadataFormat::Jpeg).unwrap();
        assert_eq!(jpeg, [0xff, 0xd8, 0xff, 0xd9]);
    }

    #[test]
    fn many_small_chunks_use_bounded_underlying_read_and_seek_operations() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Counted {
            cursor: Cursor<Vec<u8>>,
            reads: Arc<AtomicUsize>,
            seeks: Arc<AtomicUsize>,
        }
        impl Read for Counted {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                self.cursor.read(bytes)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.seeks.fetch_add(1, Ordering::Relaxed);
                self.cursor.seek(position)
            }
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png_chunk(&mut png, b"eXIf", b"location");
        for _ in 0..50_000 {
            png_chunk(&mut png, b"IDAT", &[0]);
        }
        png_chunk(&mut png, b"IEND", &[]);
        let length = u64::try_from(png.len()).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let seeks = Arc::new(AtomicUsize::new(0));
        let reader = Counted {
            cursor: Cursor::new(png),
            reads: Arc::clone(&reads),
            seeks: Arc::clone(&seeks),
        };
        let mut payload = UploadPayload::from_reader(
            reader,
            length,
            "image.data".into(),
            "application/octet-stream".into(),
        );
        payload.remove_exif().unwrap();
        assert!(
            reads.load(Ordering::Relaxed) < 2_000,
            "tiny chunks caused too many unbuffered reads"
        );
        assert!(
            seeks.load(Ordering::Relaxed) < 2_000,
            "tiny chunks caused {} underlying seeks",
            seeks.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn malformed_image_containers_fail_before_any_mutation() {
        for (mut bytes, format) in [
            (
                vec![0xff, 0xd8, 0xff, 0xe1, 0xff, 0xff],
                MetadataFormat::Jpeg,
            ),
            (
                b"\x89PNG\r\n\x1a\n\xff\xff\xff\xffeXIf".to_vec(),
                MetadataFormat::Png,
            ),
            (
                b"RIFF\x0c\0\0\0WEBPEXIF\xff\xff\xff\xff".to_vec(),
                MetadataFormat::Webp,
            ),
        ] {
            let original = bytes.clone();
            assert!(strip_in_place(&mut bytes, format).is_err());
            assert_eq!(bytes, original);
        }
    }
}
