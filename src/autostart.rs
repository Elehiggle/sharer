//! Per-user desktop login startup registration.

use anyhow::{Context as _, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

mod format;

fn registration() -> Result<AutoLaunch> {
    let executable = std::env::current_exe().context("could not locate the ShareR executable")?;
    let executable = executable
        .to_str()
        .context("ShareR's executable path is not valid Unicode")?;
    let mut builder = AutoLaunchBuilder::new();

    builder
        .set_app_name("ShareR")
        .set_app_path(executable)
        .set_args(&["--background"]);

    #[cfg(windows)]
    builder.set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser);
    #[cfg(target_os = "linux")]
    builder.set_linux_launch_mode(auto_launch::LinuxLaunchMode::XdgAutostart);
    #[cfg(target_os = "macos")]
    builder.set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);

    builder.build().context("could not configure login startup")
}

pub(super) fn set_enabled(enabled: bool) -> Result<()> {
    let registration = registration()?;

    if enabled {
        enable_registration(&registration).context("could not enable login startup")
    } else {
        registration
            .disable()
            .context("could not disable login startup")
    }
}

#[cfg(windows)]
fn enable_registration(registration: &AutoLaunch) -> Result<()> {
    // auto-launch concatenates this field verbatim into the Run command.
    let executable = format::windows_executable(registration.get_app_path())?;

    AutoLaunch::new(
        registration.get_app_name(),
        &executable,
        auto_launch::WindowsEnableMode::CurrentUser,
        &["--background"],
    )
    .enable()
    .context("could not write the login startup registry entry")
}

#[cfg(target_os = "linux")]
fn enable_registration(registration: &AutoLaunch) -> Result<()> {
    let executable = format::linux_argument(registration.get_app_path())?;
    let script = format::linux_argument(format::LINUX_LAUNCH_SCRIPT)?;

    // GLib checks the executable before expanding %% field codes. A fixed
    // launcher preserves percent signs; the path is only a positional argument
    // to the constant script, so shell syntax in filenames is never evaluated.
    AutoLaunch::new(
        registration.get_app_name(),
        "/bin/sh",
        auto_launch::LinuxLaunchMode::XdgAutostart,
        &["-c", &script, "ShareR", &executable, "--background"],
    )
    .enable()
    .context("could not write the login startup desktop entry")
}

#[cfg(target_os = "macos")]
fn enable_registration(registration: &AutoLaunch) -> Result<()> {
    use std::io::Write as _;

    let executable = std::path::Path::new(registration.get_app_path());

    anyhow::ensure!(
        executable.is_absolute(),
        "ShareR's executable path is not absolute"
    );
    anyhow::ensure!(executable.exists(), "ShareR's executable no longer exists");

    let data = format::launch_agent(registration.get_app_path())?;
    let home = directories::BaseDirs::new().context("could not find the home directory")?;
    let directory = home.home_dir().join("Library").join("LaunchAgents");

    cap_std::fs::Dir::create_ambient_dir_all(&directory, cap_std::ambient_authority())
        .context("could not create the login startup directory")?;

    // Publish a complete plist, so an unsuccessful write preserves the old entry.
    let mut file = tempfile::NamedTempFile::new_in(&directory)
        .context("could not create a temporary login startup entry")?;

    file.write_all(data.as_bytes())
        .context("could not write the login startup entry")?;
    file.persist(directory.join("ShareR.plist"))
        .context("could not replace the login startup entry")?;

    Ok(())
}

pub(super) fn is_enabled() -> Result<bool> {
    registration()?
        .is_enabled()
        .context("could not inspect login startup")
}
