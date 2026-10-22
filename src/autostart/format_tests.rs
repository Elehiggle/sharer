use super::{LINUX_LAUNCH_SCRIPT, launch_agent, linux_argument, windows_executable};

#[test]
fn windows_disabled_state_is_not_inferred_from_timestamp() {
    assert!(!super::windows_startup_approved(&[
        3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
    ]));
    assert!(super::windows_startup_approved(&[
        2, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8
    ]));
    assert!(!super::windows_startup_approved(&[]));
}

#[test]
fn linux_status_rejects_disabled_or_outdated_entries() {
    let executable = "/opt/Share R/ShareR";
    let data = super::linux_desktop_entry(executable).unwrap();

    assert!(super::linux_is_enabled(&data, executable).unwrap());
    assert!(!super::linux_is_enabled(&data, "/opt/old/ShareR").unwrap());
    assert!(!super::linux_is_enabled(&format!("{data}Hidden=true\n"), executable).unwrap());
    assert!(
        !super::linux_is_enabled(
            &format!("{data}X-GNOME-Autostart-enabled=false\n"),
            executable
        )
        .unwrap()
    );
}

#[test]
fn macos_status_requires_matching_enabled_program() {
    let executable = "/Applications/Share R & <x>/ShareR";
    let data = launch_agent(executable).unwrap();

    assert!(super::launch_agent_is_enabled(&data, executable));
    assert!(!super::launch_agent_is_enabled(
        &data,
        "/Applications/old/ShareR"
    ));
    assert!(!super::launch_agent_is_enabled(
        &data.replace(
            "<key>RunAtLoad</key><true/>",
            "<key>RunAtLoad</key><false/>"
        ),
        executable
    ));
    assert!(!super::launch_agent_is_enabled(
        &data.replace("</dict>", "<key>Disabled</key><true/></dict>"),
        executable
    ));
    assert!(!super::launch_agent_is_enabled("not a plist", executable));
}

#[test]
fn macos_status_accepts_apple_plist_doctype() {
    let executable = "/Applications/Share R & <x>/ShareR";
    let data = launch_agent(executable).unwrap().replace(
        "<plist version=\"1.0\">",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">",
    );

    assert!(super::launch_agent_is_enabled(&data, executable));
}

#[test]
fn macos_status_checks_program_override_and_plist_structure() {
    let executable = "/Applications/ShareR";
    let data = launch_agent(executable).unwrap();

    assert!(!super::launch_agent_is_enabled(
        &data.replace(
            "</dict>",
            "<key>Program</key><string>/Applications/old/ShareR</string></dict>"
        ),
        executable
    ));
    assert!(super::launch_agent_is_enabled(
        &data.replace(
            "</dict>",
            "<key>Program</key><string>/Applications/ShareR</string></dict>"
        ),
        executable
    ));
    assert!(!super::launch_agent_is_enabled(
        &data.replace("</dict>", "<key>Disabled</key></dict>"),
        executable
    ));
    assert!(!super::launch_agent_is_enabled(
        &data.replace("plist", "not-a-plist"),
        executable
    ));
    assert!(!super::launch_agent_is_enabled(
        &data.replace("<string>ShareR</string>", "<integer>ShareR</integer>"),
        executable
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_native_plist_tools_preserve_startup_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ShareR.plist");
    let executable = "/Applications/Share R & <x> \"quoted\" 'single' \\ 100%/ShareR";

    std::fs::write(&path, launch_agent(executable).unwrap()).unwrap();
    let output = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "xml1"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "native plist conversion failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = std::fs::read_to_string(&path).unwrap();

    assert!(super::launch_agent_is_enabled(&data, executable));
    let output = std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :ProgramArguments:0"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end_matches('\n'),
        executable
    );
}

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
