use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use crate::unix::{guard, kill_all, prepare, Group, Mark};
#[cfg(windows)]
pub use crate::windows::{guard, kill_all, prepare, Group, Mark};

/// Searches PATH by hand because commands like `npx` are .cmd files on Windows
/// and the standard library only finds .exe
pub fn program(name: &Path) -> PathBuf {
    let paths = env::var_os("PATH").unwrap_or_default();
    let extensions = match cfg!(windows) && name.extension().is_none() {
        true => env::var("PATHEXT").unwrap_or_default(),
        false => String::new(),
    };
    let found = searched(name, &paths, &extensions);
    found.unwrap_or_else(|| name.to_owned())
}

/// Only bare names are searched, and an empty `extensions` tries the name as it is
fn searched(name: &Path, paths: &OsStr, extensions: &str) -> Option<PathBuf> {
    if name.components().count() != 1 {
        return None;
    }

    let suffixes: Vec<&str> = match extensions {
        "" => vec![""],
        listed => listed
            .split(';')
            .filter(|suffix| !suffix.is_empty())
            .collect(),
    };

    for dir in env::split_paths(paths) {
        for extension in &suffixes {
            let mut file = name.as_os_str().to_owned();
            file.push(extension);
            let candidate = dir.join(file);
            let Ok(metadata) = fs::metadata(&candidate) else {
                continue;
            };

            #[cfg(unix)]
            let executable = metadata.permissions().mode() & 0o111 != 0;
            #[cfg(not(unix))]
            let executable = true;
            if metadata.is_file() && executable {
                return Some(candidate);
            }
        }
    }

    None
}

/// Drops the ` (os error N)` that `io::Error` appends
pub fn described(error: &dyn Error) -> String {
    let text = error.to_string();
    let text = match text.rfind(" (os error ") {
        Some(at) if text.ends_with(')') => &text[..at],
        _ => &text,
    };

    // Windows system messages end with a period, a full-width one in Chinese editions
    text.trim_end_matches(['.', '。']).to_owned()
}

/// Goes after a colon, so the first letter is lowercased unless the word is an acronym
pub fn cause(error: &dyn Error) -> String {
    let text = described(error);

    let word = text.split_whitespace().next().unwrap_or_default();
    if word.chars().skip(1).any(char::is_uppercase) {
        return text;
    }

    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().chain(chars).collect()
}

pub fn terminal_view(text: &str) -> String {
    if !text.contains(['\x1b', '\r']) {
        return text.to_owned();
    }

    let mut kept = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            kept.push(c);
            continue;
        }
        match chars.next() {
            // CSI runs to its final byte
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC runs to BEL or ESC \
            Some(']') => loop {
                match chars.next() {
                    Some('\x07') | None => break,
                    Some('\x1b') => {
                        chars.next();
                        break;
                    }
                    Some(_) => {}
                }
            },
            // Intermediate bytes are followed by one final byte
            Some(' '..='/') => {
                for c in chars.by_ref() {
                    if ('0'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some('0'..='~') | None => {}
            // An ESC that starts no sequence is dropped on its own
            Some(c) => kept.push(c),
        }
    }

    let mut lines = Vec::new();
    for line in kept.split('\n') {
        // A \r with nothing after it leaves the line alone, which keeps \r\n a plain newline
        let mut frames = line.split('\r').filter(|frame| !frame.is_empty());
        let last = frames.next_back().unwrap_or_default();
        lines.push(last);
    }
    lines.join("\n")
}

pub fn tilde(path: &Path) -> String {
    let home = env::home_dir();
    let rest = home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok());

    match rest {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

/// The text on one line, with `cwd` as `.` and paths in it relative, and the home directory as `~`
pub fn one_line(text: &str, cwd: &Path) -> String {
    let cwd = cwd.display().to_string();
    let mut text = shortened(text, &cwd, ".", "");
    if let Some(home) = env::home_dir() {
        let home = home.display().to_string();
        let inside = format!("~{MAIN_SEPARATOR}");
        text = shortened(&text, &home, "~", &inside);
    }

    let words: Vec<&str> = text.split_whitespace().collect();
    words.join(" ")
}

/// Writes `root` as `name` and paths in it starting with `inside`,
/// except where `root` only begins or ends a longer path or word
fn shortened(text: &str, root: &str, name: &str, inside: &str) -> String {
    // An empty root would match at every position and never advance
    if root.is_empty() {
        return text.to_owned();
    }

    let pathy = |c: char| c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '-' | '_' | '~');
    let mut kept = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(root) {
        let (before, after) = (&rest[..at], &rest[at + root.len()..]);
        kept.push_str(before);
        let joined = before.chars().next_back().is_some_and(pathy);
        rest = after;
        match after.chars().next() {
            _ if joined => kept.push_str(root),
            Some(MAIN_SEPARATOR) => {
                kept.push_str(inside);
                rest = &after[MAIN_SEPARATOR.len_utf8()..];
            }
            Some(next) if pathy(next) => kept.push_str(root),
            _ => kept.push_str(name),
        }
    }
    kept.push_str(rest);
    kept
}

#[cfg(test)]
mod tests {
    use crate::{cause, one_line, searched, shortened, terminal_view};
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::fs::Permissions;
    use std::io;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::path::MAIN_SEPARATOR;
    use std::process;

    #[test]
    fn programs_searched() {
        let name = format!("smol-programs-{}", process::id());
        let base = env::temp_dir().join(name);
        let (first, second) = (base.join("first"), base.join("second"));
        for dir in [&first, &second] {
            fs::create_dir_all(dir).expect("dir");
        }
        let executable = |name: &str| {
            let path = second.join(name);
            fs::write(&path, "@echo hello").expect("write");
            #[cfg(unix)]
            {
                let mode = Permissions::from_mode(0o755);
                fs::set_permissions(&path, mode).expect("chmod");
            }
        };
        for name in ["café.CMD", "café.EXE", "résumé"] {
            executable(name);
        }
        fs::write(second.join("notes"), "").expect("write");
        let paths = env::join_paths([&first, &second]).expect("paths");
        let extensions = ".COM;.EXE;.BAT;.CMD";

        let found = |name: &str, extensions: &str| searched(Path::new(name), &paths, extensions);
        assert_eq!(found("café", extensions), Some(second.join("café.EXE")));
        assert_eq!(found("café", ".CMD"), Some(second.join("café.CMD")));
        assert_eq!(found("café.cmd", extensions), None);
        assert_eq!(found("café.EXE", ""), Some(second.join("café.EXE")));
        assert_eq!(found("résumé", ""), Some(second.join("résumé")));
        assert_eq!(found("second/café", extensions), None);
        assert_eq!(found("missing", extensions), None);
        #[cfg(unix)]
        assert_eq!(found("notes", ""), None);

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cause_lowercased() {
        let lowered = |text: &str| {
            let error = io::Error::other(text.to_owned());
            cause(&error)
        };

        assert_eq!(lowered("Connection refused"), "connection refused");
        assert_eq!(lowered("I/O error"), "I/O error");
        assert_eq!(lowered("JSON error"), "JSON error");
        assert_eq!(lowered("Über-proxy refused"), "über-proxy refused");
        assert_eq!(lowered("Access is denied。"), "access is denied");
        let windows = "The system cannot find the file specified.";
        assert_eq!(
            lowered(windows),
            "the system cannot find the file specified"
        );
        assert_eq!(lowered(""), "");

        #[cfg(unix)]
        {
            let missing = io::Error::from_raw_os_error(2);
            assert_eq!(cause(&missing), "no such file or directory");
        }
    }

    #[test]
    fn terminal_view_cleaned() {
        let cases = [
            (
                "\x1b[31mcafé\x1b[0m and \x1b]0;title\x07naïve\x1b]8;;link\x1b\\ text",
                "café and naïve text",
            ),
            ("\x1b(B\x1b[mreset \x1b7saved", "reset saved"),
            ("lone\x1b\nnext line", "lone\nnext line"),
            (
                "\x1b[31mrésumé\x1b[0m\nstep 1\rstep 2\rstep 3\n",
                "résumé\nstep 3\n",
            ),
            ("\rstart\r\r", "start"),
            ("plain\ntwo lines", "plain\ntwo lines"),
        ];
        for (text, expected) in cases {
            assert_eq!(terminal_view(text), expected, "{text:?}");
        }
    }

    #[test]
    fn paths_shortened() {
        let sep = MAIN_SEPARATOR;
        let root = format!("{sep}work{sep}menus");
        let text = format!("In {root}, read {root}{sep}café.txt ({root}) but not {root}2, {root}.bak or {sep}var{root}{sep}naïve");
        let expected = format!(
            "In ., read café.txt (.) but not {root}2, {root}.bak or {sep}var{root}{sep}naïve"
        );
        assert_eq!(shortened(&text, &root, ".", ""), expected);
        assert_eq!(shortened("résumé", "", ".", ""), "résumé");

        let home = env::home_dir().expect("home");
        let cwd = home.join("menus");
        let (inside, under) = (cwd.display(), home.display());
        let task =
            format!("Work in {inside}: read {inside}{sep}café.txt\nand  {under}{sep}naïve.md");
        let expected = format!("Work in .: read café.txt and ~{sep}naïve.md");
        assert_eq!(one_line(&task, &cwd), expected);
    }
}
