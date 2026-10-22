//! Serialization for the three login startup formats used by `ShareR`.

use anyhow::Result;

#[cfg(any(target_os = "linux", test))]
pub(super) const LINUX_LAUNCH_SCRIPT: &str = r#"exec "$@""#;

#[cfg(any(windows, test))]
pub(super) fn windows_executable(executable: &str) -> Result<String> {
    anyhow::ensure!(
        !executable.contains(['"', '\0']),
        "ShareR's executable path cannot be represented in a Windows command line"
    );

    // argv[0] has its own quoting rules; backslashes remain literal, including
    // the extended-length path prefix. No shell is involved in a Run entry.
    Ok(format!("\"{executable}\""))
}

#[cfg(any(windows, test))]
pub(super) fn windows_startup_approved(value: &[u8]) -> bool {
    let Some(state) = value.get(..4).and_then(|bytes| bytes.try_into().ok()) else {
        return false;
    };

    // StartupApproved stores state first, then a timestamp. A zero timestamp
    // does not make a disabled entry enabled.
    matches!(u32::from_le_bytes(state), 2 | 6)
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn linux_argument(argument: &str) -> Result<String> {
    anyhow::ensure!(
        !argument.chars().any(char::is_control),
        "ShareR's startup argument contains unsupported control characters"
    );

    // Desktop Entry Exec quoting and the string-value escape layer both apply.
    // Escape literal percent signs so filenames cannot become field codes.
    let mut encoded = String::with_capacity(argument.len() + 2);

    encoded.push('"');

    for character in argument.chars() {
        match character {
            '\\' => encoded.push_str(r"\\\\"),
            '"' => encoded.push_str(r#"\\""#),
            '$' => encoded.push_str(r"\\$"),
            '`' => encoded.push_str(r"\\`"),
            '%' => encoded.push_str("%%"),
            other => encoded.push(other),
        }
    }

    encoded.push('"');

    Ok(encoded)
}

#[cfg(any(target_os = "linux", test))]
fn linux_exec(executable: &str) -> Result<String> {
    Ok(format!(
        "/bin/sh -c {} ShareR {} --background",
        linux_argument(LINUX_LAUNCH_SCRIPT)?,
        linux_argument(executable)?
    ))
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn linux_desktop_entry(executable: &str) -> Result<String> {
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=ShareR\n\
         Comment=ShareR startup script\nExec={}\nStartupNotify=false\nTerminal=false\n",
        linux_exec(executable)?
    ))
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn linux_is_enabled(data: &str, executable: &str) -> Result<bool> {
    let expected = linux_exec(executable)?;
    let mut in_entry = false;
    let mut command_matches = false;
    let mut enabled = true;

    for line in data.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Exec" => command_matches = value.trim() == expected,
                "Hidden" => enabled &= value.trim() != "true",
                "X-GNOME-Autostart-enabled" => enabled &= value.trim() != "false",
                "Type" => enabled &= value.trim() == "Application",
                _ => {}
            }
        }
    }

    Ok(command_matches && enabled)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn launch_agent(executable: &str) -> Result<String> {
    let mut encoded = String::with_capacity(executable.len());

    for character in executable.chars() {
        match character {
            '&' => encoded.push_str("&amp;"),

            '<' => encoded.push_str("&lt;"),

            '>' => encoded.push_str("&gt;"),

            '"' => encoded.push_str("&quot;"),

            '\'' => encoded.push_str("&apos;"),

            '\t' => encoded.push_str("&#9;"),

            '\n' => encoded.push_str("&#10;"),

            '\r' => encoded.push_str("&#13;"),

            '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}' => {
                encoded.push(character);
            }

            _ => {
                anyhow::bail!("ShareR's executable path contains a character XML cannot represent")
            }
        }
    }

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>ShareR</string>\n\
         <key>ProgramArguments</key><array><string>{encoded}</string>\
         <string>--background</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         </dict></plist>\n"
    ))
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn launch_agent_is_enabled(data: &str, executable: &str) -> bool {
    // Apple's plist tools emit a public DOCTYPE. Allow it without installing an
    // external entity resolver; reading a startup entry must never fetch a DTD.
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let Ok(document) = roxmltree::Document::parse_with_options(data, options) else {
        return false;
    };
    let root = document.root_element();
    let mut values = root.children().filter(roxmltree::Node::is_element);
    let Some(dictionary) = values.next() else {
        return false;
    };

    if !root.has_tag_name("plist") || !dictionary.has_tag_name("dict") || values.next().is_some() {
        return false;
    }

    let mut nodes = dictionary.children().filter(roxmltree::Node::is_element);
    let mut command_matches = false;
    let mut run_at_load = false;
    let mut label_matches = false;
    let mut disabled = false;
    let mut program_matches = true;

    while let Some(key) = nodes.next() {
        let Some(value) = nodes.next() else {
            return false;
        };

        if !key.has_tag_name("key") {
            return false;
        }

        match key.text() {
            Some("Label") => {
                label_matches = value.has_tag_name("string") && value.text() == Some("ShareR");
            }

            Some("Program") => {
                program_matches = value.has_tag_name("string") && value.text() == Some(executable);
            }

            Some("RunAtLoad") => run_at_load = value.has_tag_name("true"),

            Some("Disabled") => disabled = value.has_tag_name("true"),

            Some("ProgramArguments") if value.has_tag_name("array") => {
                let mut arguments = value.children().filter(roxmltree::Node::is_element);

                command_matches = arguments.next().is_some_and(|argument| {
                    argument.has_tag_name("string") && argument.text() == Some(executable)
                }) && arguments.next().is_some_and(|argument| {
                    argument.has_tag_name("string") && argument.text() == Some("--background")
                }) && arguments.next().is_none();
            }

            _ => {}
        }
    }

    label_matches && program_matches && command_matches && run_at_load && !disabled
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
