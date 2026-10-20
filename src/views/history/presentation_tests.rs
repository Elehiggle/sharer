//! Regression tests for history presentation and stable action identities.

use std::{cell::RefCell, rc::Rc};

use sharer::history::HistorySort;
use slint::{Model as _, VecModel};

use super::{HistoryController, HistoryRow, clear_local_row, row_by_token, row_from_entry};

#[test]
fn history_presentation_retains_raw_action_values() {
    let entry = sharer::history::HistoryEntry {
        id: i64::MAX,
        created_at: 0,
        filename: "image\n\u{202e}gnp.exe".to_owned(),
        expires_at: "soon\u{1b}[2J".to_owned(),
        link: "https://public.example/\u{202e}link".to_owned(),
        delete_url: "https://files.example/delete/fake".to_owned(),
        local_path: "folder/image\n.png".to_owned(),
        size_bytes: 42,
    };
    let row = row_from_entry(&entry, 1);

    assert_eq!(row.entry_id, i64::MAX.to_string());
    assert_eq!(row.action_token, format!("1:{}", i64::MAX));
    assert_eq!(row.filename, "image\\n\\u{202e}gnp.exe");
    assert_eq!(row.expires, "soon\\u{1b}[2J");
    assert_eq!(row.display_link, "https://public.example/\\u{202e}link");
    assert_eq!(row.display_local_path, "folder/image\\n.png");
    assert_eq!(row.link, entry.link);
    assert_eq!(row.delete_link, entry.delete_url);
    assert_eq!(row.local_path, entry.local_path);
}

#[test]
fn action_tokens_survive_reorder_and_reject_missing_or_reused_ids() {
    let original = HistoryRow {
        entry_id: "2".into(),
        action_token: "1:2".into(),
        local_path: "original.png".into(),
        display_local_path: "original.png".into(),
        ..Default::default()
    };
    let replacement = HistoryRow {
        entry_id: "42".into(),
        action_token: "1:42".into(),
        local_path: "replacement.png".into(),
        display_local_path: "replacement.png".into(),
        ..Default::default()
    };
    let rows = Rc::new(VecModel::from(vec![original.clone(), replacement.clone()]));
    let controller = RefCell::new(HistoryController {
        local_deleted: Rc::new(|_, _, _| {}),
        generation: 1,
        page: 0,
        total: 2,
        rows: Rc::clone(&rows),
        sort: HistorySort::default(),
        suspended: false,
    });
    // Menus capture this token when they open, before a possible model change.
    let action_token = original.action_token.clone();
    let entry_id = original.entry_id.clone();

    rows.set_vec(vec![replacement.clone(), original]);
    assert_eq!(
        row_by_token(&controller, action_token.as_str())
            .unwrap()
            .local_path,
        "original.png"
    );
    clear_local_row(&controller, entry_id.as_str());
    assert_eq!(rows.row_data(0).unwrap().local_path, "replacement.png");
    assert!(rows.row_data(1).unwrap().local_path.is_empty());
    assert!(rows.row_data(1).unwrap().display_local_path.is_empty());

    rows.set_vec(vec![replacement]);
    assert!(row_by_token(&controller, action_token.as_str()).is_none());
    clear_local_row(&controller, entry_id.as_str());
    assert_eq!(rows.row_data(0).unwrap().local_path, "replacement.png");

    // SQLite can reuse an ID after deletion; a new model generation must reject old menus.
    rows.set_vec(vec![HistoryRow {
        entry_id: entry_id.clone(),
        action_token: format!("2:{entry_id}").into(),
        local_path: "reused-id.png".into(),
        ..Default::default()
    }]);
    assert!(row_by_token(&controller, action_token.as_str()).is_none());
    assert_eq!(
        row_by_token(&controller, &format!("2:{entry_id}"))
            .unwrap()
            .local_path,
        "reused-id.png"
    );
}
