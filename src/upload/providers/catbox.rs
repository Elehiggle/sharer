use anyhow::{Context, Result};
use reqwest::{Url, multipart};

use super::{Provider, ResponseMetadata};
use crate::upload::{
    UploadReceipt, UploadTarget, parse_uploader_url, plain_receipt, validate_response_url,
};

const UPLOAD_URL: &str = "https://catbox.moe/user/api.php";

#[derive(Debug)]
pub(super) struct Catbox;

impl Provider for Catbox {
    const NAME: &'static str = "Catbox";
    const FIELD: &'static str = "fileToUpload";

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn form(part: multipart::Part, _target: &UploadTarget) -> multipart::Form {
        multipart::Form::new()
            .text("reqtype", "fileupload")
            .part(Self::FIELD, part)
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let mut receipt =
            plain_receipt(body, metadata.filename, metadata.size_bytes, String::new())?;

        validate_response_url(&receipt.link, "Catbox public link")
            .context("Catbox did not return an upload link")?;
        "2 years without access".clone_into(&mut receipt.expires_at);
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::providers::tests::form_body;

    #[test]
    fn anonymous_request_uses_documented_api_fields_without_credentials() {
        let target = UploadTarget {
            credential: "must-not-leak".to_owned(),
            ..UploadTarget::default()
        };
        let body = form_body(Catbox::form(multipart::Part::text("capture"), &target));

        assert_eq!(Catbox::endpoint(&target, 60).unwrap().as_str(), UPLOAD_URL);
        assert!(body.contains("name=\"reqtype\"\r\n\r\nfileupload\r\n"));
        assert!(body.contains("name=\"fileToUpload\"\r\n\r\ncapture\r\n"));
        assert!(!body.contains("userhash"));
        assert!(!body.contains("must-not-leak"));
    }

    #[test]
    fn parses_plaintext_link_and_rejects_api_error_text() {
        let parse = |body: &[u8]| {
            Catbox::parse(
                body,
                &UploadTarget::default(),
                ResponseMetadata {
                    filename: "capture.png".to_owned(),
                    size_bytes: 42,
                    delete_header: "",
                },
            )
        };
        let receipt = parse(b"https://files.catbox.moe/example.png\n").unwrap();

        assert_eq!(receipt.link, "https://files.catbox.moe/example.png");
        assert_eq!(receipt.expires_at, "2 years without access");
        assert!(receipt.delete_url.is_empty());
        assert!(parse(b"File is too large").is_err());
        assert!(parse(b"javascript:alert(1)").is_err());
    }
}
