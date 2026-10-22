//! Headless subprocess output must remain usable by scripts and pipelines.

use std::process::{Command, Output, Stdio};

fn command(profile: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ShareR"));

    command.env("SHARER_CONFIG_DIR", profile);

    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn initialization_and_history_preserve_piped_stdout() {
    let profile = tempfile::tempdir().unwrap();
    let initialized = command(profile.path())
        .args(["--init", "https://uploads.example.invalid"])
        .output()
        .unwrap();

    assert_success(&initialized);
    assert!(
        String::from_utf8(initialized.stdout)
            .unwrap()
            .contains("Uploader configured:")
    );
    let history = command(profile.path())
        .args(["--history", "--json"])
        .output()
        .unwrap();

    assert_success(&history);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&history.stdout).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn headless_failure_preserves_piped_stderr() {
    let profile = tempfile::tempdir().unwrap();
    let output = command(profile.path())
        .args(["--init", "not-a-provider"])
        .stdout(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("ShareR:")
    );
}

#[test]
fn history_preserves_stdout_when_stderr_is_not_piped() {
    let profile = tempfile::tempdir().unwrap();
    let output = command(profile.path())
        .args(["--history", "--json"])
        .stderr(Stdio::null())
        .output()
        .unwrap();

    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!([])
    );
}
