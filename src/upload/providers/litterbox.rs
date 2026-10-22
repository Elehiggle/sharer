use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use reqwest::{Url, multipart};

use super::{Provider, ResponseMetadata};
use crate::upload::{
    UploadReceipt, UploadTarget, parse_uploader_url, plain_receipt, validate_response_url,
};

const UPLOAD_URL: &str = "https://litterbox.catbox.moe/resources/internals/api.php";
const RETENTION_HOURS: i64 = 72;

#[derive(Debug)]
pub(super) struct Litterbox;

impl Provider for Litterbox {
    const NAME: &'static str = "Litterbox";
    const FIELD: &'static str = "fileToUpload";

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn form(part: multipart::Part, _target: &UploadTarget) -> multipart::Form {
        multipart::Form::new()
            .text("reqtype", "fileupload")
            .text("time", "72h")
            .part(Self::FIELD, part)
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let mut receipt =
            plain_receipt(body, metadata.filename, metadata.size_bytes, String::new())?;

        validate_response_url(&receipt.link, "Litterbox public link")
            .context("Litterbox did not return an upload link")?;
        receipt.expires_at = (Utc::now() + Duration::hours(RETENTION_HOURS)).to_rfc3339();
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::providers::tests::form_body;

    #[test]
    fn request_uses_fixed_72_hour_retention_and_no_credential() {
        let target = UploadTarget {
            credential: "must-not-leak".to_owned(),
            ..UploadTarget::default()
        };
        let body = form_body(Litterbox::form(multipart::Part::text("capture"), &target));

        assert_eq!(
            Litterbox::endpoint(&target, 0).unwrap().as_str(),
            UPLOAD_URL
        );
        assert!(body.contains("name=\"reqtype\"\r\n\r\nfileupload\r\n"));
        assert!(body.contains("name=\"time\"\r\n\r\n72h\r\n"));
        assert!(body.contains("name=\"fileToUpload\"\r\n\r\ncapture\r\n"));
        assert!(!body.contains("must-not-leak"));
    }

    #[test]
    fn parses_plaintext_link_and_records_expiry() {
        let earliest = Utc::now() + Duration::hours(72);
        let receipt = Litterbox::parse(
            b"https://litter.catbox.moe/example.png\n",
            &UploadTarget::default(),
            ResponseMetadata {
                filename: "capture.png".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
        .unwrap();
        let latest = Utc::now() + Duration::hours(72);
        let expiry = chrono::DateTime::parse_from_rfc3339(&receipt.expires_at).unwrap();

        assert_eq!(receipt.link, "https://litter.catbox.moe/example.png");
        assert!(expiry >= earliest && expiry <= latest);
        assert!(receipt.delete_url.is_empty());
    }

    #[test]
    fn rejects_plaintext_failure_response() {
        assert!(
            Litterbox::parse(
                b"File is too large",
                &UploadTarget::default(),
                ResponseMetadata {
                    filename: "capture.png".to_owned(),
                    size_bytes: 42,
                    delete_header: "",
                },
            )
            .is_err()
        );
    }
}
