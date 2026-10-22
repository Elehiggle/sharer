use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Deserialize;

use super::{CredentialRequirement, Provider, ResponseMetadata};
use crate::upload::{UploadReceipt, UploadTarget, parse_uploader_url};

const UPLOAD_URL: &str = "https://pixeldrain.com/api/file";
const SHARE_URL: &str = "https://pixeldrain.com/u/";

#[derive(Debug)]
pub(super) struct Pixeldrain;

impl Provider for Pixeldrain {
    const NAME: &'static str = "Pixeldrain";
    const FIELD: &'static str = "file";
    const CREDENTIAL: CredentialRequirement = CredentialRequirement::Required;

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn headers(target: &UploadTarget) -> Result<HeaderMap> {
        // Pixeldrain uses an empty Basic-auth username and the API key as password.
        let encoded = STANDARD.encode(format!(":{}", target.credential));
        let mut value = HeaderValue::from_str(&format!("Basic {encoded}"))
            .context("invalid Pixeldrain API key")?;

        value.set_sensitive(true);
        let mut headers = HeaderMap::with_capacity(1);

        headers.insert(AUTHORIZATION, value);
        Ok(headers)
    }

    fn parse(
        body: &[u8],
        _target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let response: PixeldrainResponse =
            serde_json::from_slice(body).context("Pixeldrain returned invalid JSON")?;

        anyhow::ensure!(response.success, "Pixeldrain rejected the upload");
        anyhow::ensure!(
            !response.id.is_empty() && response.id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "Pixeldrain returned an invalid file ID"
        );
        Ok(UploadReceipt {
            original_name: metadata.filename,
            size_bytes: metadata.size_bytes,
            link: format!("{SHARE_URL}{}", response.id),
            // Deletion needs authenticated API access; it is not a shareable URL.
            delete_url: String::new(),
            expires_at: String::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct PixeldrainResponse {
    success: bool,
    #[serde(default)]
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::providers::tests::form_body;
    use reqwest::multipart;

    #[test]
    fn request_uses_sensitive_basic_auth_and_documented_file_field() {
        let target = UploadTarget {
            credential: "example-key".to_owned(),
            ..UploadTarget::default()
        };
        let headers = Pixeldrain::headers(&target).unwrap();
        let authorization = &headers[AUTHORIZATION];
        let body = form_body(Pixeldrain::form(multipart::Part::text("capture"), &target));

        assert_eq!(
            Pixeldrain::endpoint(&target, 60).unwrap().as_str(),
            UPLOAD_URL
        );
        assert_eq!(authorization.to_str().unwrap(), "Basic OmV4YW1wbGUta2V5");
        assert!(authorization.is_sensitive());
        assert!(body.contains("name=\"file\"\r\n\r\ncapture\r\n"));
        assert!(!body.contains("example-key"));
    }

    #[test]
    fn parses_share_page_without_exposing_api_credentials() {
        let receipt = parse(br#"{"success":true,"id":"AbCd1234"}"#).unwrap();

        assert_eq!(receipt.link, "https://pixeldrain.com/u/AbCd1234");
        assert!(receipt.delete_url.is_empty());
    }

    #[test]
    fn rejects_failed_uploads_and_invalid_file_ids() {
        for body in [
            br#"{"success":false,"message":"invalid key"}"#.as_slice(),
            br#"{"success":true}"#.as_slice(),
            br#"{"success":true,"id":"../../api/file"}"#.as_slice(),
            br#"{"success":true,"id":"abc?download"}"#.as_slice(),
        ] {
            assert!(parse(body).is_err());
        }
    }

    fn parse(body: &[u8]) -> Result<UploadReceipt> {
        Pixeldrain::parse(
            body,
            &UploadTarget::default(),
            ResponseMetadata {
                filename: "capture.png".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
    }
}
