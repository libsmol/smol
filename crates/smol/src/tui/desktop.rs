use crate::tui::render::Target;
use crate::tui::{diagram, picture};
use ratatui::crossterm::clipboard::CopyToClipboard;
use ratatui::crossterm::execute;
use reaper::cause;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, stdout, ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
#[cfg(not(windows))]
use std::time::{Duration, Instant};

#[cfg(not(windows))]
const OPEN_WAIT: Duration = Duration::from_millis(500);
#[cfg(not(windows))]
const OPEN_EVERY: Duration = Duration::from_millis(20);

/// Where what the user copies and opens goes, which is the device they look at the TUI on
pub(crate) trait Desktop {
    fn copy(&mut self, text: &str);
    fn open(&mut self, target: &Target) -> Result<(), String>;
}

/// The machine smol runs on, and the terminal it runs in
pub(crate) struct Local;

impl Desktop for Local {
    /// Some terminals ignore OSC 52
    fn copy(&mut self, text: &str) {
        let osc = CopyToClipboard::to_clipboard_from(text);
        execute!(stdout(), osc).ok();

        let commands: &[&[&str]] = if cfg!(target_os = "macos") {
            &[&["pbcopy"]]
        } else if cfg!(windows) {
            &[&["clip"]]
        } else if env::var_os("WAYLAND_DISPLAY").is_some() {
            &[&["wl-copy"]]
        } else if env::var_os("DISPLAY").is_some() {
            &[
                &["xclip", "-selection", "clipboard"],
                &["xsel", "--clipboard", "--input"],
            ]
        } else {
            &[]
        };

        // clip reads input in the console code page unless it is UTF-16 with a BOM
        let bytes = match cfg!(windows) {
            true => {
                let mut bytes = vec![0xFF, 0xFE];
                let units = text.encode_utf16().flat_map(u16::to_le_bytes);
                bytes.extend(units);
                bytes
            }
            false => text.as_bytes().to_vec(),
        };

        for command in commands {
            let child = Command::new(command[0])
                .args(&command[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();

            if let Ok(mut child) = child {
                // stdin must be closed before waiting or the child never sees the end of input
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(&bytes).ok();
                }
                thread::spawn(move || child.wait());
                return;
            }
        }
    }

    fn open(&mut self, target: &Target) -> Result<(), String> {
        match target {
            Target::Url(url) => open(url),
            Target::Diagram(code) => {
                let html = diagram::page(code)?;
                let path = saved("diagram", code, "html", html.as_bytes())?;
                open(&path)
            }
            // Named after the format the bytes really have, as the system opens a file by its name,
            // which the model chose
            Target::Picture(source) => {
                let (bytes, format) = picture::original(source)?;
                let extension = format.extensions_str().first().unwrap_or(&"png");
                let path = saved("image", source, extension, &bytes)?;
                open(&path)
            }
        }
    }
}

/// The same key always gets the same file, which only the user can read
fn saved(kind: &str, key: impl Hash, extension: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    let name = format!("smol-{kind}-{hash:016x}.{extension}");
    // /tmp is shared on Linux, while XDG_RUNTIME_DIR is private to the user and cleared at logout
    let dir = match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => env::temp_dir(),
    };
    let path = dir.join(name);

    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("{kind}: {}: {reason}", path.display())
    };
    // Made anew, since where there is no private folder another user
    // may have put a link or a file of their own under its name
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(failed(e)),
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).map_err(failed)?;
    file.write_all(bytes).map_err(failed)?;

    Ok(path)
}

fn open(target: impl AsRef<OsStr>) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use crate::tui::desktop::saved;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process;

    /// What another user planted under the name is replaced, and a link's target is left alone
    #[test]
    fn saved_private() {
        let key = format!("café 🐍 {}", process::id());
        let path = saved("test", &key, "txt", "naïve".as_bytes()).expect("save");
        let again = saved("test", &key, "txt", "naïve".as_bytes());
        assert_eq!(again, Ok(path.clone()));
        assert_eq!(fs::read_to_string(&path).expect("read"), "naïve");

        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");

            let target = path.with_extension("planted");
            fs::write(&target, "kept").expect("write");
            fs::remove_file(&path).expect("remove");
            symlink(&target, &path).expect("link");
            saved("test", &key, "txt", "crème".as_bytes()).expect("save");
            let kept = fs::read_to_string(&target).expect("read");
            fs::remove_file(&target).ok();
            assert_eq!(kept, "kept");
            let meta = fs::symlink_metadata(&path).expect("meta");
            assert!(meta.is_file(), "{meta:?}");
            assert_eq!(fs::read_to_string(&path).expect("read"), "crème");
        }
        fs::remove_file(&path).ok();
    }
}
