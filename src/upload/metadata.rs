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
    let result = {
        let mut buffered = BufferedImage::new(reader)?;

        strip_buffered_to_file(&mut buffered, length, format)
    };
    // Release read-ahead before rewinding the underlying upload source.
    let rewind = reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image upload source");

    result.and_then(|output| {
        rewind?;
        Ok(output)
    })
}

fn strip_buffered_to_file(
    reader: &mut dyn ReadSeek,
    length: u64,
    format: MetadataFormat,
) -> Result<Option<(std::fs::File, u64)>> {
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
    GainMapLength(u64),
    UnsupportedReferences,
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
    let mut references = ContainerReferences::default();

    while let Some(piece) = next_piece(reader, length, format, &mut position)? {
        changed |= matches!(piece.kind, PieceKind::Drop);
        references.observe(&piece)?;

        if matches!(piece.kind, PieceKind::Mpf) {
            reader.seek(SeekFrom::Start(piece.start))?;
            let mut bytes = vec![0; usize::try_from(piece.end - piece.start)?];

            reader.read_exact(&mut bytes)?;
            let (ifd, count, little) = mpf_directory(&bytes)?;
            let has_index = (0..count).try_fold(false, |found, index| {
                Ok::<_, anyhow::Error>(
                    found
                        || matches!(
                            read_u16(&bytes, ifd + 2 + index * 12, little)?,
                            0xb001 | 0xb002
                        ),
                )
            })?;

            if !has_index {
                anyhow::ensure!(mpf.is_some(), "JPEG MP Attribute table has no MP Index");
                validate_mpf_attributes(&bytes, references.image_count + 1)?;
                continue;
            }

            anyhow::ensure!(
                mpf.is_none(),
                "multiple JPEG MPF tables cannot be safely rewritten"
            );
            mpf = Some(Mpf {
                start: piece.start,
                bytes,
            });
        }
    }

    if changed {
        references.validate()?;

        if let Some(mpf) = &mut mpf {
            repair_mpf(reader, length, mpf)?;
        }
    }

    Ok(Prepared { changed, mpf })
}

#[derive(Default)]
struct ContainerReferences {
    gainmap_length: Option<u64>,
    unsupported: bool,
    image_count: usize,
    secondary_start: u64,
    secondary_length: u64,
    secondary_changed: bool,
}

impl ContainerReferences {
    fn observe(&mut self, piece: &Piece) -> Result<()> {
        match piece.kind {
            PieceKind::GainMapLength(length) => {
                anyhow::ensure!(
                    self.gainmap_length.is_none(),
                    "multiple HDR XMP image-length references cannot be safely rewritten"
                );
                self.gainmap_length = Some(length);
            }

            PieceKind::UnsupportedReferences => self.unsupported = true,

            PieceKind::Drop if self.image_count > 0 => self.secondary_changed = true,

            PieceKind::JpegEnd => {
                if self.image_count == 0 {
                    self.secondary_start = piece.end;
                }

                if self.image_count == 1 {
                    self.secondary_length = piece.end - self.secondary_start;
                }

                self.image_count += 1;
            }

            _ => {}
        }

        Ok(())
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.unsupported,
            "HDR XMP has unsupported image lengths or padding; metadata cannot be removed without changing structural references"
        );

        if let Some(length) = self.gainmap_length {
            anyhow::ensure!(
                !self.secondary_changed,
                "HDR XMP references the secondary image length; removing secondary-image metadata would invalidate HDR rendering"
            );
            anyhow::ensure!(
                self.image_count == 2 && length == self.secondary_length,
                "HDR XMP gain-map length does not match the supported two-image JPEG container"
            );
        }

        Ok(())
    }
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

                return Ok((end, xmp_kind(&xml)?));
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
/// Mixed packets fail closed to preserve rendering and avoid retaining location data.
fn xmp_kind(xml: &[u8]) -> Result<PieceKind> {
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
        return Ok(PieceKind::Drop);
    }

    let mut references = PacketReferences::default();

    for node in document.descendants() {
        anyhow::ensure!(
            !node.is_comment() && !node.is_pi(),
            "HDR XMP contains comments or processing instructions; identifying metadata cannot be safely removed"
        );

        if node.is_element() {
            validate_hdr_element(node)?;
            references.observe(node)?;
        } else if node.is_text() {
            validate_hdr_text(node)?;
        }
    }

    Ok(references.kind())
}

#[derive(Default)]
struct PacketReferences {
    gainmap_length: Option<u64>,
    unsupported: bool,
}

impl PacketReferences {
    fn observe(&mut self, node: roxmltree::Node<'_, '_>) -> Result<()> {
        let name = node.tag_name();

        self.unsupported |=
            name.namespace() == Some(ITEM) && matches!(name.name(), "Length" | "Padding");

        for attribute in node.attributes().filter(|attribute| {
            attribute.namespace() == Some(ITEM) && matches!(attribute.name(), "Length" | "Padding")
        }) {
            let supported = attribute.name() == "Length"
                && name.namespace() == Some(CONTAINER)
                && name.name() == "Item"
                && node.attribute((ITEM, "Semantic")) == Some("GainMap")
                && node.attribute((ITEM, "Mime")) == Some("image/jpeg")
                && self.gainmap_length.is_none();

            if supported {
                self.gainmap_length = Some(attribute.value().parse::<u64>()?);
            } else {
                self.unsupported = true;
            }
        }

        Ok(())
    }

    fn kind(self) -> PieceKind {
        if self.unsupported {
            PieceKind::UnsupportedReferences
        } else if let Some(length) = self.gainmap_length {
            PieceKind::GainMapLength(length)
        } else {
            PieceKind::Keep
        }
    }
}

fn validate_hdr_element(node: roxmltree::Node<'_, '_>) -> Result<()> {
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
            | (Some(CONTAINER), "Directory" | "Item")
            | (Some(ITEM), "Item")
    );

    anyhow::ensure!(
        wrapper || safe_hdr_value(name.namespace(), name.name(), node.text().unwrap_or("")),
        "HDR XMP contains unsupported fields; identifying metadata cannot be safely removed without changing HDR rendering"
    );

    for attribute in node.attributes() {
        let safe = matches!(
            (attribute.namespace(), attribute.name(), attribute.value()),
            (Some(RDF), "about", "")
                | (Some(RDF), "parseType", "Resource")
                | (
                    Some(META),
                    "xmptk",
                    "XMP Core 6.0.0" | "Adobe XMP Core 5.1.2"
                )
        ) || safe_hdr_value(attribute.namespace(), attribute.name(), attribute.value());

        anyhow::ensure!(
            safe,
            "HDR XMP contains unsupported attributes; identifying metadata cannot be safely removed without changing HDR rendering"
        );
    }

    Ok(())
}

fn validate_hdr_text(node: roxmltree::Node<'_, '_>) -> Result<()> {
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

    Ok(())
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
    let (ifd, count, little) = mpf_directory(&mpf.bytes)?;
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

fn mpf_directory(bytes: &[u8]) -> Result<(usize, usize, bool)> {
    let tiff = 8_usize;

    anyhow::ensure!(bytes.len() >= tiff + 8, "MPF TIFF header is truncated");
    let little = match &bytes[tiff..tiff + 4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => bail!("MPF TIFF header is invalid"),
    };
    let ifd = tiff + usize::try_from(read_u32(bytes, tiff + 4, little)?)?;
    let count = usize::from(read_u16(bytes, ifd, little)?);

    anyhow::ensure!(
        ifd.checked_add(2 + count * 12 + 4)
            .is_some_and(|end| end <= bytes.len()),
        "MPF directory is truncated"
    );
    Ok((ifd, count, little))
}

fn validate_mpf_attributes(bytes: &[u8], image_number: usize) -> Result<()> {
    let (ifd, count, little) = mpf_directory(bytes)?;

    // Only retain the standard version and this image's ordinal. Reject opaque
    // fields, padding and linked directories rather than retaining identifying data.
    anyhow::ensure!(
        image_number > 1
            && image_number <= MAX_MPF_IMAGES
            && ifd == 16
            && count == 2
            && bytes.len() == ifd + 2 + count * 12 + 4
            && read_u32(bytes, ifd + 2 + count * 12, little)? == 0,
        "JPEG MP Attribute table has unsupported fields or directories"
    );
    let mut version = false;
    let mut individual_number = false;

    for index in 0..count {
        let tag = ifd + 2 + index * 12;
        let valid = match read_u16(bytes, tag, little)? {
            0xb000 if !version => {
                version = true;
                read_u16(bytes, tag + 2, little)? == 7
                    && read_u32(bytes, tag + 4, little)? == 4
                    && &bytes[tag + 8..tag + 12] == b"0100"
            }

            0xb101 if !individual_number => {
                individual_number = true;
                read_u16(bytes, tag + 2, little)? == 4
                    && read_u32(bytes, tag + 4, little)? == 1
                    && usize::try_from(read_u32(bytes, tag + 8, little)?)? == image_number
            }

            _ => false,
        };

        anyhow::ensure!(
            valid,
            "JPEG MP Attribute table has unsupported fields or values"
        );
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
#[path = "metadata_tests.rs"]
mod tests;
