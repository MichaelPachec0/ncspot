//! Keeping the standard error stream off the screen while the UI owns the terminal.
//!
//! C libraries write there on their own: alsa-lib reports a buffer underrun
//! ("ALSA lib pcm.c ... underrun occurred"), PipeWire's client library its warnings. The
//! text lands in the middle of the UI, and since the UI redraws only the cells it knows
//! have changed, it stays there until something else happens to paint over it.

use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

/// File in the cache folder that takes the stream while the UI runs.
pub const LOG_FILE: &str = "stderr.log";

/// A descriptor pointed at a file; dropping it points the descriptor back where it was.
pub struct Redirected {
    fd: RawFd,
    saved: RawFd,
}

/// Point the standard error stream at `path` (emptied first) if it is a terminal. Anything
/// else (a file, a pipe) is where the user asked for it, and stays. `None` when it was left
/// alone, also on failure: the stream is then as before.
pub fn off_the_terminal(path: &Path) -> Option<Redirected> {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(libc::STDERR_FILENO) } != 1 {
        return None;
    }
    redirect(libc::STDERR_FILENO, &File::create(path).ok()?)
}

fn redirect(fd: RawFd, to: &File) -> Option<Redirected> {
    // SAFETY: dup and dup2 take plain descriptors; `saved` is owned by the returned value
    // and closed exactly once, in its Drop or right here.
    unsafe {
        let saved = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3);
        if saved < 0 {
            return None;
        }
        if libc::dup2(to.as_raw_fd(), fd) < 0 {
            libc::close(saved);
            return None;
        }
        Some(Redirected { fd, saved })
    }
}

impl Drop for Redirected {
    fn drop(&mut self) {
        // SAFETY: both descriptors are open; `saved` is closed here and nowhere else.
        unsafe {
            libc::dup2(self.saved, self.fd);
            libc::close(self.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};

    fn contents(file: &mut File) -> String {
        let mut text = String::new();
        file.rewind().unwrap();
        file.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn writes_go_to_the_file_until_the_redirection_is_dropped() {
        let dir = std::env::temp_dir().join(format!("ncspot-stderr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let open = |name: &str| {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(dir.join(name))
                .unwrap()
        };
        // `stream` stands in for the standard error stream: the test must not touch the
        // real one, which the test runner and the other tests share.
        let mut stream = open("stream");
        let mut log = open("log");

        let redirected = redirect(stream.as_raw_fd(), &log).expect("redirected");
        stream.write_all(b"underrun occurred").unwrap();
        drop(redirected);
        stream.write_all(b"after").unwrap();

        assert_eq!(contents(&mut log), "underrun occurred");
        assert_eq!(contents(&mut open_again(&dir.join("stream"))), "after");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn open_again(path: &Path) -> File {
        File::open(path).unwrap()
    }
}
