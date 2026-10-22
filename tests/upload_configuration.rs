//! Saved upload settings remain compatible as providers and expiry options evolve.

use sharer::{
    MAX_LIFETIME_SECONDS,
    config::{AppConfig, ProviderCredentials},
    storage::PlatformStorage,
    upload::UploaderKind,
};

#[test]
fn new_provider_credentials_default_empty_and_stay_redacted() {
    let mut credentials: ProviderCredentials = serde_json::from_str(
        r#"{"imgur":"imgur-secret","vgy":"legacy-secret","sul":"sul-secret"}"#,
    )
    .unwrap();

    assert!(credentials.pixeldrain.is_empty());
    credentials.set_for_provider(UploaderKind::Pixeldrain, "pixel-secret".to_owned());
    assert_eq!(
        credentials.for_provider(UploaderKind::Pixeldrain),
        "pixel-secret"
    );
    assert_eq!(
        credentials.for_provider(UploaderKind::Imgur),
        "imgur-secret"
    );

    for kind in [
        UploaderKind::Custom,
        UploaderKind::Uguu,
        UploaderKind::Catbox,
        UploaderKind::Litterbox,
        UploaderKind::TransferSh,
    ] {
        credentials.set_for_provider(kind, "must-not-leak".to_owned());
        assert!(credentials.for_provider(kind).is_empty());
    }

    let debug = format!("{credentials:?}");

    assert!(!debug.contains("secret"));
    assert!(debug.contains("pixeldrain_configured: true"));
}

#[test]
fn saved_lifetimes_preserve_no_expiry_and_exact_seconds() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for lifetime_seconds in [
        0,
        1,
        90,
        5_400,
        AppConfig::default().lifetime_seconds,
        MAX_LIFETIME_SECONDS,
    ] {
        let saved = AppConfig {
            lifetime_seconds,
            ..AppConfig::default()
        };

        saved.save(&storage).unwrap();
        assert_eq!(
            AppConfig::load(&storage).unwrap().lifetime_seconds,
            lifetime_seconds
        );
    }
}
