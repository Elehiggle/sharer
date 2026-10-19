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

#[cfg(test)]
mod tests {
    use super::{LINUX_LAUNCH_SCRIPT, launch_agent, linux_argument, windows_executable};

    #[test]
    fn windows_keeps_the_complete_executable_as_the_first_argument() {
        for executable in [
            r"C:\ShareR\ShareR.exe",
            r"C:\Program Files\ShareR\ShareR.exe",
            r"\\?\C:\ShareR & <100%>\ShareR.exe",
        ] {
            assert_eq!(
                windows_executable(executable).unwrap(),
                format!("\"{executable}\""),
                "Windows must quote the executable without changing its backslashes"
            );
        }

        assert!(
            windows_executable("C:\\bad\" --other.exe").is_err(),
            "a quote cannot occur in a Windows executable filename"
        );
        assert!(
            windows_executable("C:\\bad\0.exe").is_err(),
            "NUL must not truncate the executable path"
        );
    }

    #[test]
    fn linux_escapes_both_desktop_entry_layers_and_literal_field_codes() {
        let cases = [
            ("/opt/ShareR", r#""/opt/ShareR""#),
            ("/opt/Share R/ShareR", r#""/opt/Share R/ShareR""#),
            ("/opt/\"ShareR\"", r#""/opt/\\"ShareR\\"""#),
            (r"/opt/Share\R", r#""/opt/Share\\\\R""#),
            ("/opt/Share%f & <R>", r#""/opt/Share%%f & <R>""#),
            ("/opt/$Share`R'", r#""/opt/\\$Share\\`R'""#),
            ("/opt/Share=R", r#""/opt/Share=R""#),
            ("/opt/Share%f= R & <x>", r#""/opt/Share%%f= R & <x>""#),
            (LINUX_LAUNCH_SCRIPT, r#""exec \\"\\$@\\"""#),
        ];

        for (executable, expected) in cases {
            assert_eq!(
                linux_argument(executable).unwrap(),
                expected,
                "the desktop entry must preserve the exact executable path"
            );
        }

        for executable in ["/opt/Share\nR", "/opt/Share\0R"] {
            assert!(
                linux_argument(executable).is_err(),
                "paths unsupported by the desktop entry format must fail explicitly"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_launcher_preserves_paths_without_evaluating_shell_syntax() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let names = [
            "Share=R",
            "Share%f= R & <x>",
            r#"Share "quoted" 'single' \ `touch injected` $(touch injected); & <x> %f="#,
        ];

        for name in names {
            let executable = directory.path().join(name);

            std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\"\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

            let output = std::process::Command::new("/bin/sh")
                .args(["-c", LINUX_LAUNCH_SCRIPT, "ShareR"])
                .arg(&executable)
                .arg("--background")
                .current_dir(directory.path())
                .output()
                .unwrap();

            assert!(output.status.success(), "the exact executable must launch");
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{}\n--background\n", executable.display()),
                "the shell must preserve the executable and background argument"
            );
            assert!(
                !directory.path().join("injected").exists(),
                "shell syntax from the path must never be evaluated"
            );
        }
    }

    #[test]
    fn macos_keeps_a_single_xml_string_for_the_executable() {
        let executable = "/Applications/Share R & <x> \"quoted\" 'single' \\ 100%/ShareR";
        let data = launch_agent(executable).unwrap();

        assert!(
            data.contains(
                "<array><string>/Applications/Share R &amp; &lt;x&gt; &quot;quoted&quot; \
                 &apos;single&apos; \\ 100%/ShareR</string><string>--background</string></array>"
            ),
            "XML markup in the filename must stay within the executable string"
        );
        assert!(
            data.contains("<key>Label</key><string>ShareR</string>"),
            "the existing registration label must stay compatible with disable/status"
        );
        assert!(
            launch_agent("/Applications/ShareR").unwrap().contains(
                "<array><string>/Applications/ShareR</string><string>--background</string></array>"
            ),
            "ordinary executable paths must be preserved"
        );
        assert!(
            launch_agent("/Share\t\n\rR")
                .unwrap()
                .contains("/Share&#9;&#10;&#13;R"),
            "XML whitespace normalization must not alter the path"
        );
        assert!(
            launch_agent("/Share\0R").is_err(),
            "invalid XML characters must fail before writing the registration"
        );
    }
}
