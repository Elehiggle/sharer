//! Locate an optional `FFmpeg` installation without relying on the working directory.

use std::path::{Path, PathBuf};

pub(crate) mod install;

#[cfg(windows)]
const EXECUTABLE: &str = "ffmpeg.exe";
#[cfg(not(windows))]
const EXECUTABLE: &str = "ffmpeg";

pub(crate) fn find() -> Option<PathBuf> {
    let application = std::env::current_exe().ok();
    let path = std::env::var_os("PATH");
    let path_directories = path.as_ref().into_iter().flat_map(std::env::split_paths);

    managed_directory()
        .ok()
        .and_then(|directory| find_in(Some(&directory), [], []))
        .or_else(|| {
            find_in(
                application.as_deref().and_then(Path::parent),
                path_directories,
                standard_directories(),
            )
        })
}

pub(crate) fn managed_directory() -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;

    directories::ProjectDirs::from("re", "", "sharer")
        .map(|directories| directories.data_local_dir().join("tools").join("ffmpeg"))
        .context("FFmpeg installation directory unavailable")
}

fn find_in(
    application_directory: Option<&Path>,
    path_directories: impl IntoIterator<Item = PathBuf>,
    standard_directories: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    application_directory
        .into_iter()
        .map(Path::to_path_buf)
        .chain(path_directories)
        .chain(standard_directories)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(EXECUTABLE))
        .filter(|candidate| is_executable(candidate))
        .find_map(|candidate| candidate.canonicalize().ok())
}

fn is_executable(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| {
        if !metadata.is_file() {
            return false;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        true
    })
}

fn standard_directories() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    return ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
        .map(PathBuf::from)
        .into();

    #[cfg(target_os = "linux")]
    return [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/home/linuxbrew/.linuxbrew/bin",
    ]
    .map(PathBuf::from)
    .into();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let executable = directory.join(EXECUTABLE);

        std::fs::write(&executable, []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        executable.canonicalize().unwrap()
    }

    #[test]
    fn portable_copy_works_without_path_and_takes_precedence() {
        let temporary = tempfile::tempdir().unwrap();
        let application = temporary.path().join("application");
        let portable = executable(&application);
        let path = temporary.path().join("path");

        executable(&path);
        assert_eq!(find_in(Some(&application), [], []), Some(portable.clone()));
        assert_eq!(find_in(Some(&application), [path], []), Some(portable));
    }

    #[test]
    fn path_and_standard_locations_are_searched_in_order() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("path");
        let standard = temporary.path().join("standard");
        let from_path = executable(&path);
        let from_standard = executable(&standard);

        assert_eq!(find_in(None, [path], [standard.clone()]), Some(from_path));
        assert_eq!(find_in(None, [], [standard]), Some(from_standard));
    }

    #[test]
    fn missing_files_directories_and_implicit_working_directory_are_rejected() {
        let temporary = tempfile::tempdir().unwrap();

        std::fs::create_dir(temporary.path().join(EXECUTABLE)).unwrap();
        assert!(
            find_in(
                Some(temporary.path()),
                [PathBuf::new(), PathBuf::from(".")],
                []
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_without_execute_permission_are_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let path = executable(temporary.path());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(find_in(Some(temporary.path()), [], []).is_none());
    }
}
