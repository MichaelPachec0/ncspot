#![allow(dead_code)]

use std::{fmt::Write, path::PathBuf};

/// Returns a human readable String of a Duration
///
/// Example: `3h 12m 53s`
pub fn format_duration(d: &std::time::Duration) -> String {
    let mut s = String::new();
    let mut append_unit = |value, unit| {
        if value > 0 {
            let _ = write!(s, "{value}{unit}");
        }
    };

    let seconds = d.as_secs() % 60;
    let minutes = (d.as_secs() / 60) % 60;
    let hours = (d.as_secs() / 60) / 60;

    append_unit(hours, "h ");
    append_unit(minutes, "m ");
    append_unit(seconds, "s ");

    s.trim_end().to_string()
}

/// Returns a human readable String of milliseconds in the HH:MM:SS format.
pub fn ms_to_hms(duration: u32) -> String {
    let mut formated_time = String::new();

    let total_seconds = duration / 1000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;

    if hours > 0 {
        formated_time.push_str(&format!("{hours}:{minutes:02}:"));
    } else {
        formated_time.push_str(&format!("{minutes}:"));
    }
    formated_time.push_str(&format!("{seconds:02}"));

    formated_time
}

pub fn cache_path_for_url(url: String) -> std::path::PathBuf {
    let mut path = crate::config::cache_path("covers");
    path.push(url.split('/').next_back().unwrap());
    path
}

/// Download `url` to `path`. An HTTP error status is an error, and the body is streamed into
/// a `<path>.part` sibling that only replaces `path` once complete, so a failed download never
/// leaves a file at `path` for the cache to keep. Errors do not carry the URL.
pub fn download(url: String, path: std::path::PathBuf) -> Result<(), std::io::Error> {
    let mut resp = reqwest::blocking::get(url)
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| std::io::Error::other(e.without_url()))?;

    std::fs::create_dir_all(path.parent().unwrap())?;
    write_via_part(&path, |file| std::io::copy(&mut resp, file).map(|_| ()))
}

/// The `<path>.part` sibling a download is written to before it is complete.
fn part_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// Run `write` on a fresh `<path>.part` file and rename it to `path` when it succeeds; on any
/// failure the part file is removed and `path` is left as it was.
fn write_via_part(
    path: &std::path::Path,
    write: impl FnOnce(&mut std::fs::File) -> Result<(), std::io::Error>,
) -> Result<(), std::io::Error> {
    let part = part_path(path);
    let result = std::fs::File::create(&part)
        .and_then(|mut file| write(&mut file))
        .and_then(|()| std::fs::rename(&part, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Create the application specific runtime directory and return the path to it.
///
/// If the directory already exists and has the correct permissions, this function just returns the
/// existing directory. The contents stored in this directory are not necessarily persisted across
/// reboots. Stored files should be small since they could reside in memory (like on a tmpfs mount).
#[cfg(unix)]
pub fn create_runtime_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::{
        fs::{self, Permissions},
        os::unix::prelude::PermissionsExt,
    };

    let user_runtime_directory = user_runtime_directory().ok_or("no runtime directory found")?;

    let creation_result = fs::create_dir(&user_runtime_directory);

    if creation_result.is_ok()
        || matches!(
            creation_result.as_ref().unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        )
    {
        // Needed when created inside a world readable directory, to prevent unauthorized access.
        // Doesn't hurt otherwise.
        fs::set_permissions(&user_runtime_directory, Permissions::from_mode(0o700))?;

        Ok(user_runtime_directory)
    } else {
        #[allow(clippy::unnecessary_unwrap)]
        Err(Box::new(creation_result.unwrap_err()))
    }
}

/// Return the path to the current user's runtime directory, or None if it couldn't be found.
/// This function does not guarantee correct ownership or permissions of the directory.
#[cfg(unix)]
pub fn user_runtime_directory() -> Option<PathBuf> {
    let linux_runtime_directory =
        PathBuf::from(format!("/run/user/{}/", unsafe { libc::getuid() }));
    let unix_runtime_directory = PathBuf::from("/tmp/");

    if let Some(xdg_runtime_directory) = xdg_runtime_directory() {
        Some(xdg_runtime_directory.join("ncspot"))
    } else if cfg!(target_os = "linux") && linux_runtime_directory.exists() {
        Some(linux_runtime_directory.join("ncspot"))
    } else if unix_runtime_directory.exists() {
        Some(unix_runtime_directory.join(format!("ncspot-{}", unsafe { libc::getuid() })))
    } else {
        None
    }
}

#[cfg(unix)]
fn xdg_runtime_directory() -> Option<PathBuf> {
    std::env::var("XDG_RUNTIME_DIR").ok().map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ncspot-utils-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn part_path_appends_to_the_file_name() {
        assert_eq!(
            part_path(std::path::Path::new("/c/covers/ab12")),
            PathBuf::from("/c/covers/ab12.part")
        );
    }

    #[test]
    fn write_via_part_renames_a_complete_write() {
        let dir = scratch("ok");
        let path = dir.join("cover");
        write_via_part(&path, |file| file.write_all(b"image")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"image");
        assert!(!part_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_via_part_leaves_nothing_behind_on_failure() {
        let dir = scratch("fail");
        let path = dir.join("cover");
        let result = write_via_part(&path, |file| {
            file.write_all(b"half")?;
            Err(std::io::Error::other("connection reset"))
        });
        assert!(result.is_err());
        assert!(!path.exists(), "a failed download is not cached");
        assert!(!part_path(&path).exists(), "the part file is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
