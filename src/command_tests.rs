use super::*;

fn saved_custom_header() -> AppConfig {
    AppConfig {
        uploader_url: "https://uploads.example/upload".to_owned(),
        upload_header_name: "X-Api-Key".to_owned(),
        upload_header_value: "fake-saved-secret".to_owned(),
        upload_header_endpoint: Some("https://uploads.example/upload".to_owned()),
        ..AppConfig::default()
    }
}

#[test]
fn cli_endpoint_overrides_drop_saved_headers_including_during_initialization() {
    let config = saved_custom_header();
    let cli =
        Cli::try_parse_from(["ShareR", "--uploader-url", "https://other.example/upload"]).unwrap();
    let target = upload_target_for_command(&cli, &config).unwrap();

    assert!(target.header_name.is_empty());
    assert!(target.header_value.is_empty());
    assert!(target.header_endpoint.is_empty());

    let cli = Cli::try_parse_from(["ShareR", "--init", "https://other.example/upload"]).unwrap();
    let target = initialized_upload_target(cli.init.as_deref().unwrap(), &cli, &config).unwrap();

    assert!(target.header_name.is_empty());
    assert!(target.header_value.is_empty());
    assert_eq!(config.upload_header_value, "fake-saved-secret");
}

#[test]
fn cli_explicit_header_authorizes_the_new_endpoint() {
    let config = saved_custom_header();
    let cli = Cli::try_parse_from([
        "ShareR",
        "--uploader-url",
        "https://other.example/upload",
        "--header",
        "X-Api-Key: fake-explicit-secret",
    ])
    .unwrap();
    let target = upload_target_for_command(&cli, &config).unwrap();

    assert_eq!(target.header_value, "fake-explicit-secret");
    assert_eq!(target.header_endpoint, "https://other.example/upload");
    assert!(target.validate().is_ok());
}

#[test]
fn cli_equivalent_endpoint_keeps_its_authorized_header() {
    let config = saved_custom_header();
    let cli = Cli::try_parse_from([
        "ShareR",
        "--uploader-url",
        "HTTPS://Uploads.Example:443/upload#display",
    ])
    .unwrap();

    assert_eq!(
        upload_target_for_command(&cli, &config)
            .unwrap()
            .header_value,
        "fake-saved-secret"
    );
}
