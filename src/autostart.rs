//! Per-user desktop login startup registration.

use anyhow::{Context as _, Result};

mod format;

fn executable() -> Result<String> {
    std::env::current_exe()
        .context("could not locate the ShareR executable")?
        .into_os_string()
        .into_string()
        .map_err(|path| {
            anyhow::anyhow!(
                "ShareR's executable path is not valid Unicode: {}",
                path.display()
            )
        })
}

pub(super) fn set_enabled(enabled: bool) -> Result<()> {
    set_registration(enabled).context(if enabled {
        "could not enable login startup"
    } else {
        "could not disable login startup"
    })
}

#[cfg(windows)]
fn set_registration(enabled: bool) -> Result<()> {
    if !enabled {
        return set_windows_registration(windows_registry::CURRENT_USER, false, "");
    }

    let command = format!(
        "{} --background",
        format::windows_executable(&executable()?)?
    );

    set_windows_registration(windows_registry::CURRENT_USER, enabled, &command)
}

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const APPROVAL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[cfg(windows)]
fn registry_optional<T>(value: windows_registry::Result<T>) -> Result<Option<T>> {
    match value {
        Ok(value) => Ok(Some(value)),

        Err(error)
            if error.code().0 == 0x8007_0002_u32.cast_signed()
                || error.code().0 == 0x8007_0003_u32.cast_signed() =>
        {
            Ok(None)
        }

        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn set_windows_registration(
    root: &windows_registry::Key,
    enabled: bool,
    command: &str,
) -> Result<()> {
    if !enabled {
        if let Some(run) = registry_optional(root.options().write().open(RUN_KEY))? {
            registry_optional(run.remove_value("ShareR"))?;
        }

        return Ok(());
    }

    let run = root.create(RUN_KEY)?;
    let approval = root.create(APPROVAL_KEY)?;
    let previous = registry_optional(run.get_value("ShareR"))?;

    run.set_string("ShareR", command)?;

    if let Err(error) = approval.set_bytes(
        "ShareR",
        windows_registry::Type::Bytes,
        &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ) {
        let restored = match previous {
            Some(value) => run.set_value("ShareR", &value),
            None => run.remove_value("ShareR"),
        };

        restored.context("login startup failed and its previous command could not be restored")?;

        return Err(error).context("could not enable the startup entry in Windows");
    }

    Ok(())
}

#[cfg(windows)]
fn windows_is_enabled(root: &windows_registry::Key, command: &str) -> Result<bool> {
    let Some(run) = registry_optional(root.open(RUN_KEY))? else {
        return Ok(false);
    };
    let Some(registered) = registry_optional(run.get_string("ShareR"))? else {
        return Ok(false);
    };

    if registered != command {
        return Ok(false);
    }

    let Some(approval) = registry_optional(root.open(APPROVAL_KEY))? else {
        return Ok(true);
    };
    let Some(value) = registry_optional(approval.get_value("ShareR"))? else {
        return Ok(true);
    };

    Ok(value.ty() == windows_registry::Type::Bytes && format::windows_startup_approved(&value))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn registration_path() -> Result<std::path::PathBuf> {
    let home = directories::BaseDirs::new().context("could not find the home directory")?;

    #[cfg(target_os = "linux")]
    {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.home_dir().join(".config"));

        Ok(config.join("autostart").join("ShareR.desktop"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home.home_dir().join("Library/LaunchAgents/ShareR.plist"))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_registration(enabled: bool) -> Result<()> {
    use std::io::Write as _;

    let path = registration_path()?;

    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    }

    let executable = executable()?;
    #[cfg(target_os = "linux")]
    let data = format::linux_desktop_entry(&executable)?;
    #[cfg(target_os = "macos")]
    let data = format::launch_agent(&executable)?;
    let directory = path
        .parent()
        .context("login startup directory is missing")?;

    cap_std::fs::Dir::create_ambient_dir_all(directory, cap_std::ambient_authority())
        .context("could not create the login startup directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .context("could not create a temporary login startup entry")?;

    file.write_all(data.as_bytes())
        .context("could not write the login startup entry")?;
    file.persist(path)
        .context("could not replace the login startup entry")?;

    Ok(())
}

pub(super) fn is_enabled() -> Result<bool> {
    let executable = executable()?;

    #[cfg(windows)]
    {
        let command = format!("{} --background", format::windows_executable(&executable)?);

        windows_is_enabled(windows_registry::CURRENT_USER, &command)
            .context("could not inspect login startup")
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let data = match std::fs::read_to_string(registration_path()?) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("could not inspect login startup"),
        };

        #[cfg(target_os = "linux")]
        return format::linux_is_enabled(&data, &executable);

        #[cfg(target_os = "macos")]
        return Ok(format::launch_agent_is_enabled(&data, &executable));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{APPROVAL_KEY, RUN_KEY, set_windows_registration, windows_is_enabled};

    struct TestKey(String);

    impl Drop for TestKey {
        fn drop(&mut self) {
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
        }
    }

    #[test]
    fn per_user_registration_tracks_command_and_windows_disable_state() {
        let directory = tempfile::tempdir().unwrap();
        let test_key = TestKey(format!(
            "Software\\ShareR.Tests\\{}",
            directory.path().file_name().unwrap().to_string_lossy()
        ));
        let root = windows_registry::CURRENT_USER.create(&test_key.0).unwrap();
        let command = r#""C:\Program Files\ShareR\ShareR.exe" --background"#;

        assert!(!windows_is_enabled(&root, command).unwrap());
        set_windows_registration(&root, true, command).unwrap();
        assert!(windows_is_enabled(&root, command).unwrap());
        assert!(!windows_is_enabled(&root, "other.exe --background").unwrap());
        let approval = root.create(APPROVAL_KEY).unwrap();

        approval
            .set_bytes(
                "ShareR",
                windows_registry::Type::Bytes,
                &[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            )
            .unwrap();
        assert!(!windows_is_enabled(&root, command).unwrap());
        set_windows_registration(&root, true, command).unwrap();
        assert!(windows_is_enabled(&root, command).unwrap());
        root.create(RUN_KEY)
            .unwrap()
            .set_string("OtherApp", "untouched")
            .unwrap();
        set_windows_registration(&root, false, command).unwrap();
        assert!(!windows_is_enabled(&root, command).unwrap());
        assert_eq!(
            root.open(RUN_KEY).unwrap().get_string("OtherApp").unwrap(),
            "untouched"
        );
        drop(approval);
        drop(root);
    }
}
