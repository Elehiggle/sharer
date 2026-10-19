//! Regression tests for human and structured headless output.

use super::super::{
    HistoryEntry, persist_and_write_upload_result, write_error, write_history_entries,
};

fn untrusted_entry() -> HistoryEntry {
    let mut entry = HistoryEntry {
        id: 42,
        created_at: 0,
        filename: String::new(),
        expires_at: String::new(),
        link: String::new(),
        delete_url: String::new(),
        local_path: String::new(),
        size_bytes: 42,
    };

    entry.filename = "capture\u{1b}[2J\r\nforged.png".to_owned();
    entry.expires_at = "soon\t\u{202e}never".to_owned();
    entry.link = "https://public.example/capture\nforged".to_owned();
    entry.delete_url = "https://files.example/delete/\u{1b}]52;c;fake\u{7}".to_owned();
    entry
}

#[test]
fn human_history_escapes_remote_fields_without_changing_its_layout() {
    let entry = untrusted_entry();
    let mut output = Vec::new();

    write_history_entries(std::slice::from_ref(&entry), false, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();

    assert_eq!(output.matches('\t').count(), 2);
    assert_eq!(output.matches('\n').count(), 4);
    assert!(!output.contains(['\u{1b}', '\r', '\u{7}', '\u{202e}']));
    assert!(output.contains("capture\\u{1b}[2J\\r\\nforged.png"));
    assert!(output.contains("https://public.example/capture\\nforged"));
    assert!(output.contains("https://files.example/delete/\\u{1b}]52;c;fake\\u{7}"));
    assert!(entry.filename.contains('\u{1b}'));
}

#[test]
fn structured_history_preserves_original_values_and_escapes_terminal_controls() {
    let entry = untrusted_entry();
    let mut output = Vec::new();

    write_history_entries(std::slice::from_ref(&entry), true, &mut output).unwrap();
    let decoded: Vec<HistoryEntry> = serde_json::from_slice(&output).unwrap();

    assert_eq!(decoded, [entry]);
    assert!(!output.contains(&0x1b));
    assert!(!output.contains(&0x07));
}

#[test]
fn upload_output_and_http_error_presentation_escape_terminal_controls() {
    let entry = untrusted_entry();
    let mut output = Vec::new();

    persist_and_write_upload_result(&mut output, &entry, |_| Ok(())).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "https://public.example/captureforged\n"
    );
    let mut error_output = Vec::new();
    let error = anyhow::anyhow!("upload failed with HTTP 400: \u{1b}[2J\nforged\u{202e}");

    write_error(&mut error_output, &error).unwrap();
    assert_eq!(
        String::from_utf8(error_output).unwrap(),
        "ShareR: upload failed with HTTP 400: \\u{1b}[2J\\nforged\\u{202e}\n"
    );
}

#[test]
fn pipeable_upload_links_remain_complete_and_url_encoded() {
    let mut entry = untrusted_entry();

    entry.link = format!(
        "https://public.example/{}\u{1b}[2J\u{202e}gnp.exe",
        "long-path/".repeat(600)
    );
    let mut output = Vec::new();

    persist_and_write_upload_result(&mut output, &entry, |_| Ok(())).unwrap();
    let output = String::from_utf8(output).unwrap();
    let link = output.trim_end_matches('\n');

    assert!(link.len() > 4_096);
    assert!(link.ends_with("%1B[2J%E2%80%AEgnp.exe"));
    assert_eq!(link, url::Url::parse(&entry.link).unwrap().as_str());
    assert!(!link.chars().any(char::is_control));
    assert!(!link.contains('\u{202e}'));
}
