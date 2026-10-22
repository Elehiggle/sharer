//! Bounded, single-line presentation of untrusted text.

use std::borrow::Cow;

const MAX_DISPLAY_CHARACTERS: usize = 4_096;

/// Escape controls and direction overrides, retaining ordinary Unicode text.
///
/// Display at most 4,096 input characters, followed by an ellipsis if truncated.
/// Use the original value for storage, structured output, copying, and opening.
#[must_use]
pub fn safe_text(value: &str) -> Cow<'_, str> {
    let mut characters = value.char_indices();
    let mut escaped = false;
    let mut end = value.len();

    for index in 0..=MAX_DISPLAY_CHARACTERS {
        let Some((offset, character)) = characters.next() else {
            break;
        };

        if index == MAX_DISPLAY_CHARACTERS {
            end = offset;
            break;
        }

        escaped |= requires_escape(character);
    }

    if !escaped && end == value.len() {
        return Cow::Borrowed(value);
    }

    let mut display = String::with_capacity(end);

    for character in value[..end].chars() {
        if requires_escape(character) {
            display.extend(character.escape_default());
        } else {
            display.push(character);
        }
    }

    if end != value.len() {
        display.push('\u{2026}');
    }

    Cow::Owned(display)
}

fn requires_escape(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}'
        )
}

/// Format a saved shortcut for compact display in action cards and fields.
#[must_use]
pub fn display_shortcut(value: &str) -> String {
    if value.trim().is_empty() {
        return "No shortcut".to_owned();
    }

    value
        .split('+')
        .map(|part| match part.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => "Ctrl".to_owned(),
            "shift" => "Shift".to_owned(),
            "alt" | "option" => "Alt".to_owned(),
            "super" | "meta" | "command" | "cmd" => "Win".to_owned(),
            "printscreen" | "print" => "PrtSc".to_owned(),
            "return" | "enter" => "Enter".to_owned(),
            "escape" | "esc" => "Esc".to_owned(),
            "backspace" => "Backspace".to_owned(),
            "delete" => "Delete".to_owned(),
            _ => part.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_commands_line_breaks_and_direction_overrides() {
        let text = "image\u{1b}]52;c;payload\u{7}\r\n\t\u{85}\u{2028}\u{202e}gnp.exe\u{2069}";

        assert_eq!(
            safe_text(text),
            "image\\u{1b}]52;c;payload\\u{7}\\r\\n\\t\\u{85}\\u{2028}\\u{202e}gnp.exe\\u{2069}"
        );
    }

    #[test]
    fn ordinary_unicode_is_borrowed_without_changing_its_spelling() {
        let text = "Urlaub – 東京 – שלום – صورة.png";

        assert!(matches!(safe_text(text), Cow::Borrowed(value) if value == text));
    }

    #[test]
    fn long_unicode_is_truncated_at_a_character_boundary() {
        let text = "é".repeat(MAX_DISPLAY_CHARACTERS + 1);
        let display = safe_text(&text);

        assert_eq!(display.chars().count(), MAX_DISPLAY_CHARACTERS + 1);
        assert!(display.ends_with('\u{2026}'));
        assert!(matches!(
            safe_text(&"x".repeat(MAX_DISPLAY_CHARACTERS)),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn shortcuts_are_compact_but_keep_unknown_keys() {
        assert_eq!(
            display_shortcut("control+shift+PrintScreen"),
            "Ctrl+Shift+PrtSc"
        );
        assert_eq!(display_shortcut("PrintScreen"), "PrtSc");
        assert_eq!(display_shortcut(""), "No shortcut");
        assert_eq!(display_shortcut("control+shift+v"), "Ctrl+Shift+v");
    }
}
