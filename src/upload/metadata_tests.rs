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
    let table = tiff + usize::try_from(read_u32(&stripped, table_tag + 8, false).unwrap()).unwrap();

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

fn mpf_attributes(little: bool) -> Vec<u8> {
    let mut bytes = b"MPF\0MM\0*\0\0\0\x08\0\x02".to_vec();

    bytes.extend_from_slice(&[0xb0, 0x00, 0, 7, 0, 0, 0, 4, b'0', b'1', b'0', b'0']);
    bytes.extend_from_slice(&[0xb1, 0x01, 0, 4, 0, 0, 0, 1, 0, 0, 0, 2]);
    bytes.extend_from_slice(&[0; 4]);

    if little {
        bytes[4..8].copy_from_slice(b"II*\0");
        bytes[8..12].reverse();
        bytes[12..14].reverse();

        for tag in [14, 26] {
            bytes[tag..tag + 2].reverse();
            bytes[tag + 2..tag + 4].reverse();
            bytes[tag + 4..tag + 8].reverse();
        }

        bytes[34..38].reverse();
    }

    jpeg_segment(0xe2, &bytes)
}

fn mpf_image_locations(bytes: &[u8]) -> (usize, usize, usize) {
    let tiff = bytes
        .windows(4)
        .position(|bytes| bytes == b"MPF\0")
        .unwrap()
        + 4;
    let ifd = tiff + usize::try_from(read_u32(bytes, tiff + 4, false).unwrap()).unwrap();
    let count = usize::from(read_u16(bytes, ifd, false).unwrap());
    let table_tag = (0..count)
        .map(|index| ifd + 2 + index * 12)
        .find(|tag| read_u16(bytes, *tag, false).unwrap() == 0xb002)
        .unwrap();
    let table = tiff + usize::try_from(read_u32(bytes, table_tag + 8, false).unwrap()).unwrap();
    let secondary = tiff + usize::try_from(read_u32(bytes, table + 24, false).unwrap()).unwrap();

    (tiff, table, secondary)
}

fn hdr_with_secondary_mpf(segment: &[u8]) -> Vec<u8> {
    let mut bytes = HDR.to_vec();

    strip_in_place(&mut bytes, MetadataFormat::Jpeg).unwrap();
    let (_, table, secondary) = mpf_image_locations(&bytes);
    let size = read_u32(&bytes, table + 20, false).unwrap();

    write_u32(
        &mut bytes,
        table + 20,
        size + u32::try_from(segment.len()).unwrap(),
        false,
    );
    bytes.splice(secondary + 2..secondary + 2, segment.iter().copied());
    bytes
}

#[test]
fn secondary_mpf_attributes_survive_inspection_and_cleaning_for_all_payload_backings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hdr.jpg");

    for little in [false, true] {
        let expected = hdr_with_secondary_mpf(&mpf_attributes(little));
        let (_, _, expected_secondary) = mpf_image_locations(&expected);
        let primary_pixels = image::load_from_memory(&expected).unwrap().to_rgba8();
        let secondary_pixels = image::load_from_memory(&expected[expected_secondary..])
            .unwrap()
            .to_rgba8();

        for add_metadata in [false, true] {
            let mut original = expected.clone();

            if add_metadata {
                let metadata = jpeg_segment(0xe1, b"Exif\0\0location");
                let (_, table, secondary) = mpf_image_locations(&original);
                let added = u32::try_from(metadata.len()).unwrap();

                for size in [table + 4, table + 20] {
                    let old = read_u32(&original, size, false).unwrap();

                    write_u32(&mut original, size, old + added, false);
                }

                original.splice(secondary + 2..secondary + 2, metadata.iter().copied());
                // Insert after the Index so the secondary offset also needs repair.
                original.splice(secondary - 2..secondary - 2, metadata.iter().copied());
                let old = read_u32(&original, table + 24, false).unwrap();

                write_u32(&mut original, table + 24, old + added, false);
            }

            std::fs::write(&path, &original).unwrap();
            let mut payloads = [
                UploadPayload::from_bytes(original.clone(), "hdr.jpg".into(), "image/jpeg".into()),
                UploadPayload::from_path(&path).unwrap(),
                UploadPayload::from_reader(
                    Cursor::new(original.clone()),
                    u64::try_from(original.len()).unwrap(),
                    "hdr.jpg".into(),
                    "image/jpeg".into(),
                ),
            ];

            for payload in &mut payloads {
                payload.remove_exif().unwrap();
                let cleaned = payload_bytes(payload);

                assert_eq!(cleaned, expected);
                assert_eq!(
                    image::load_from_memory(&cleaned).unwrap().to_rgba8(),
                    primary_pixels
                );
                assert_eq!(
                    image::load_from_memory(&cleaned[expected_secondary..])
                        .unwrap()
                        .to_rgba8(),
                    secondary_pixels
                );
            }

            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
}

#[test]
fn duplicate_mpf_indexes_and_opaque_attributes_fail_before_mutation() {
    let mut clean = HDR.to_vec();

    strip_in_place(&mut clean, MetadataFormat::Jpeg).unwrap();
    let (tiff, _, _) = mpf_image_locations(&clean);
    let start = tiff - 8;
    let size = usize::from(u16::from_be_bytes(
        clean[start + 2..start + 4].try_into().unwrap(),
    ));
    let mut segments = vec![clean[start..start + 2 + size].to_vec()];
    let attributes = mpf_attributes(false);

    for (offset, replacement) in [(30, 5), (41, 3), (45, 1)] {
        let mut invalid = attributes.clone();

        invalid[offset] = replacement;
        segments.push(invalid);
    }

    let mut trailing = attributes[4..].to_vec();

    trailing.extend_from_slice(b"location");
    segments.push(jpeg_segment(0xe2, &trailing));

    for segment in segments {
        let mut jpeg = hdr_with_secondary_mpf(&segment);
        let original = jpeg.clone();

        assert!(strip_in_place(&mut jpeg, MetadataFormat::Jpeg).is_err());
        assert_eq!(jpeg, original);
    }
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

fn small_image(format: image::ImageFormat) -> Vec<u8> {
    let image = image::DynamicImage::new_rgb8(1, 1);
    let mut encoded = Cursor::new(Vec::new());

    image.write_to(&mut encoded, format).unwrap();
    let bytes = encoded.into_inner();

    assert!(bytes.len() < IO_BUFFER_BYTES);
    bytes
}

fn serve_multipart_image(listener: &std::net::TcpListener, expected: &[u8]) {
    let (mut stream, _) = listener.accept().unwrap();

    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).unwrap();

        assert!(count > 0);
        request.extend_from_slice(&buffer[..count]);

        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = end + 4;
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();

            break (header_end, length);
        }
    };

    while request.len() - header_end < content_length {
        let count = stream.read(&mut buffer).unwrap();

        assert!(count > 0, "upload body ended early");
        request.extend_from_slice(&buffer[..count]);
    }

    let body = &request[header_end..];

    assert!(
        body.windows(expected.len()).any(|bytes| bytes == expected),
        "multipart body omitted or truncated the inspected image"
    );
    let response = format!(
        r#"{{"data":{{"originalName":"image","size":{},"link":"https://files.example/image","deleteLink":"https://files.example/delete/token","expiresAt":"2026-09-09T00:00:00Z"}}}}"#,
        expected.len()
    );

    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
}

#[test]
fn small_metadata_free_file_and_reader_images_upload_completely_after_inspection() {
    use crate::upload::{UploadClient, UploadRoute};
    let directory = tempfile::tempdir().unwrap();

    for format in [
        image::ImageFormat::Png,
        image::ImageFormat::Jpeg,
        image::ImageFormat::WebP,
    ] {
        let image = small_image(format);
        let path = directory.path().join("image.data");

        std::fs::write(&path, &image).unwrap();
        let mut payloads = [
            UploadPayload::from_path(&path).unwrap(),
            UploadPayload::from_reader(
                Cursor::new(image.clone()),
                u64::try_from(image.len()).unwrap(),
                "image.data".into(),
                "application/octet-stream".into(),
            ),
        ];

        for payload in &mut payloads {
            payload.remove_exif().unwrap();
        }

        for payload in payloads {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/upload", listener.local_addr().unwrap());
            let expected = image.clone();
            let server = std::thread::spawn(move || serve_multipart_image(&listener, &expected));
            let client = UploadClient::new(&endpoint, &UploadRoute::Direct, None).unwrap();

            client.upload(payload, 60).unwrap();
            server.join().unwrap();
        }
    }
}

// These packets follow libultrahdr's generateXmpForPrimaryImage and
// generateXmpForSecondaryImage schemas, including its fixed toolkit string.
const GOOGLE_SECONDARY_XMP: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 5.1.2"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" hdrgm:Version="1.0" hdrgm:GainMapMin="0" hdrgm:GainMapMax="2" hdrgm:Gamma="1" hdrgm:OffsetSDR="0.015625" hdrgm:OffsetHDR="0.015625" hdrgm:HDRCapacityMin="0" hdrgm:HDRCapacityMax="2" hdrgm:BaseRenditionIsHDR="False"/></rdf:RDF></x:xmpmeta>"#;

fn google_primary_xmp(length: usize) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 5.1.2"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:Container="http://ns.google.com/photos/1.0/container/" xmlns:Item="http://ns.google.com/photos/1.0/container/item/" xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" hdrgm:Version="1.0"><Container:Directory><rdf:Seq><rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="Primary" Item:Mime="image/jpeg"/></rdf:li><rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="GainMap" Item:Mime="image/jpeg" Item:Length="{length}"/></rdf:li></rdf:Seq></Container:Directory></rdf:Description></rdf:RDF></x:xmpmeta>"#
    )
}

fn add_jpeg_xmp(jpeg: &mut Vec<u8>, xml: &str) {
    let mut packet = XMP_HEADER.to_vec();

    packet.extend_from_slice(xml.as_bytes());
    jpeg.splice(2..2, jpeg_segment(0xe1, &packet));
}

fn google_hdr_images(secondary_metadata: bool, primary_extra: &str) -> (Vec<u8>, Vec<u8>) {
    let mut secondary = small_image(image::ImageFormat::Jpeg);

    add_jpeg_xmp(&mut secondary, GOOGLE_SECONDARY_XMP);

    if secondary_metadata {
        secondary.splice(2..2, jpeg_segment(0xe1, b"Exif\0\0location"));
    }

    let primary_xmp = google_primary_xmp(secondary.len()).replace(
        "Item:Semantic=\"Primary\"",
        &format!("Item:Semantic=\"Primary\" {primary_extra}"),
    );
    let mut primary = small_image(image::ImageFormat::Jpeg);

    add_jpeg_xmp(&mut primary, &primary_xmp);
    primary.splice(2..2, jpeg_segment(0xe1, b"Exif\0\0location"));
    primary.extend_from_slice(&secondary);
    (primary, secondary)
}

#[test]
fn libultrahdr_packets_survive_primary_cleaning_with_unchanged_gainmap_lengths() {
    let (mut jpeg, secondary) = google_hdr_images(false, "");
    let before = image::load_from_memory(&jpeg).unwrap().to_rgba8();

    strip_in_place(&mut jpeg, MetadataFormat::Jpeg).unwrap();
    assert_eq!(image::load_from_memory(&jpeg).unwrap().to_rgba8(), before);
    assert!(jpeg.ends_with(&secondary));
    let mut position = 0;
    let mut reference = None;

    while let Some(piece) = next_piece(
        &mut Cursor::new(jpeg.as_slice()),
        u64::try_from(jpeg.len()).unwrap(),
        MetadataFormat::Jpeg,
        &mut position,
    )
    .unwrap()
    {
        assert!(!matches!(piece.kind, PieceKind::Drop));

        if let PieceKind::GainMapLength(length) = piece.kind {
            reference = Some(length);
        }
    }

    assert_eq!(reference, Some(u64::try_from(secondary.len()).unwrap()));
}

#[test]
fn google_length_or_padding_references_are_never_left_stale_by_cleaning() {
    for (secondary_metadata, primary_extra) in [
        (true, ""),
        (false, "Item:Length=\"100\""),
        (false, "Item:Padding=\"1\""),
    ] {
        let (mut jpeg, _) = google_hdr_images(secondary_metadata, primary_extra);
        let original = jpeg.clone();
        let error = strip_in_place(&mut jpeg, MetadataFormat::Jpeg).unwrap_err();

        assert!(error.to_string().contains("HDR XMP"));
        assert_eq!(jpeg, original);
    }
}
