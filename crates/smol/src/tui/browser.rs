use reaper::cause;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
#[cfg(not(windows))]
use std::time::{Duration, Instant};

#[cfg(not(windows))]
const OPEN_WAIT: Duration = Duration::from_millis(500);
#[cfg(not(windows))]
const OPEN_EVERY: Duration = Duration::from_millis(20);

/// /tmp is shared on Linux, while XDG_RUNTIME_DIR is private to the user and cleared at logout
pub(crate) fn private_dir() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => env::temp_dir(),
    }
}

/// Makes the file anew, since where there is no private folder another user
/// may have put a link or a file of their own under its name
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

pub(crate) fn open(target: impl AsRef<OsStr>) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };

    // The browser inherits stderr and would hang once an unread pipe filled up
    let spawned = Command::new(program)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("open: {program}: {reason}")
    };
    let mut child = spawned.map_err(failed)?;

    // Without a desktop or on a remote machine it fails right away,
    // and explorer returns 1 even on success
    #[cfg(not(windows))]
    {
        let deadline = Instant::now() + OPEN_WAIT;
        while Instant::now() < deadline {
            let Ok(exited) = child.try_wait() else {
                break;
            };
            let Some(status) = exited else {
                thread::sleep(OPEN_EVERY);
                continue;
            };
            if status.success() {
                return Ok(());
            }

            let ended = match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "was ended by a signal".to_owned(),
            };
            return Err(format!("open: {program}: {ended}"));
        }
    }

    thread::spawn(move || child.wait());

    Ok(())
}
