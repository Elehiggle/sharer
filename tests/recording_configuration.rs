use sharer::config::{AppConfig, RecordingFormat};

#[test]
fn existing_configs_keep_webp_and_mp4_is_persisted_as_an_opt_in() {
    let mut config = serde_json::from_str::<AppConfig>("{}").unwrap();

    assert_eq!(config.recording_format, RecordingFormat::Webp);
    config.recording_format = RecordingFormat::Mp4;
    let saved = serde_json::to_string(&config).unwrap();

    assert_eq!(
        serde_json::from_str::<AppConfig>(&saved)
            .unwrap()
            .recording_format,
        RecordingFormat::Mp4
    );
}
