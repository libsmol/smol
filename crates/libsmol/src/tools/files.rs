use crate::prompt;
use crate::tools::ToolError;
use grep_matcher::LineTerminator;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex_syntax::Parser;
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_GREP_BYTES: usize = 32 * 1024 * 1024;

/// Shows a link as a folder only when `follows` allows following it,
/// and once `stop` is set returns early with a result nobody reads
pub(crate) fn ls(
    cwd: &Path,
    path: &Path,
    recursive: bool,
    follows: &dyn Fn(&Path) -> bool,
    stop: &AtomicBool,
) -> Result<Vec<String>, ToolError> {
    let root = cwd.join(path);
    let shown = path.to_string_lossy();
    let failed = |e| ToolError::os("list", &shown, e);
    // Read it even when recursing to report a missing path or a non-directory
    let entries = fs::read_dir(&root).map_err(failed)?;
    let named = |relative: &Path, full: &Path| {
        let mut name = slashed(relative);
        let followed = !full.is_symlink() || follows(full);
        if followed && full.is_dir() {
            name.push('/');
        }
        name
    };

    let mut names = Vec::new();
    if recursive {
        for entry in walker(&root).build().flatten() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let full = entry.path();
            let relative = full.strip_prefix(&root).unwrap_or(full);
            if relative.as_os_str().is_empty() {
                continue;
            }
            let name = named(relative, full);
            names.push(name);
        }
    } else {
        for entry in entries {
            let entry = entry.map_err(failed)?;
            let file = entry.file_name();
            let relative = Path::new(&file);
            let full = entry.path();
            let name = named(relative, &full);
            names.push(name);
        }
    }

    names.sort();
    Ok(names)
}

/// Names files the way `path` is written,
/// and once `stop` is set returns early with a result nobody reads
pub(crate) fn grep(
    pattern: &str,
    cwd: &Path,
    path: &Path,
    glob: Option<&str>,
    stop: &AtomicBool,
) -> Result<Vec<(String, u64, String)>, ToolError> {
    let invalid = |argument: &str, e: &dyn Display| {
        let reason = e.to_string();
        let message = prompt::failure("grep", argument, &reason);
        ToolError::Failed(message)
    };
    // In CRLF files `$` must still match at the line end without `\s` eating the `\r`
    let built = RegexMatcherBuilder::new().crlf(true).build(pattern);
    let matcher = match built {
        Ok(matcher) => matcher,
        // Errors from grep-regex include the `(?:…)` it wraps the pattern in, hence the reparse
        Err(e) => {
            let reparsed = Parser::new().parse(pattern);
            return match reparsed {
                Err(own) => Err(invalid("pattern", &own)),
                Ok(_) => Err(invalid("pattern", &e)),
            };
        }
    };

    let root = cwd.join(path);
    let shown = path.to_string_lossy();
    let failed = |e| ToolError::os("search", &shown, e);
    let meta = fs::metadata(&root).map_err(failed)?;
    // The walk skips unreadable directories but an unreadable root is an error
    if meta.is_dir() {
        fs::read_dir(&root).map_err(failed)?;
    }

    // As a walker override the glob would let matching files past `.gitignore`
    let mut globs = OverrideBuilder::new(&root);
    if let Some(glob) = glob {
        globs.add(glob).map_err(|e| invalid("glob", &e))?;
    }
    let globs = globs.build().map_err(|e| invalid("glob", &e))?;

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_terminator(LineTerminator::crlf())
        .build();
    let mut found = Vec::new();
    let mut bytes = 0;

    for entry in walker(&root).build().flatten() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }

        let full = entry.path();
        if full != root && globs.matched(full, false).is_ignore() {
            continue;
        }
        let below = full.strip_prefix(&root).unwrap_or(full);
        // Joining an empty path would add a trailing slash to a file searched by itself
        let named = match below.as_os_str().is_empty() {
            true => path.to_owned(),
            false => path.join(below),
        };
        let named = named.strip_prefix(".").unwrap_or(&named);
        let mut matches = Matches {
            name: slashed(named),
            lines: Vec::new(),
            bytes,
            binary: false,
        };

        let searched = match File::open(full) {
            Ok(inner) => {
                let reader = Stopping { inner, stop };
                searcher.search_reader(&matcher, reader, &mut matches)
            }
            Err(e) => Err(e),
        };
        // Unreadable files in a directory are skipped but an unreadable root file is an error
        if full == root {
            searched.map_err(failed)?;
        }
        if matches.binary {
            continue;
        }

        bytes = matches.bytes;
        for (number, line) in matches.lines {
            found.push((matches.name.clone(), number, line));
        }
        if bytes > MAX_GREP_BYTES {
            let limit = MAX_GREP_BYTES >> 20;
            let message = prompt::too_many(limit);
            return Err(ToolError::Failed(message));
        }
    }

    Ok(found)
}

/// Matching lines of one file, all dropped when a later NUL shows the file is binary
struct Matches {
    name: String,
    lines: Vec<(u64, String)>,
    /// Bytes collected so far, earlier files included
    bytes: usize,
    binary: bool,
}

impl Sink for Matches {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> io::Result<bool> {
        let number = matched.line_number().unwrap_or_default();
        let line = String::from_utf8_lossy(matched.bytes());
        let line = line.trim_end_matches(['\n', '\r']);
        self.bytes += self.name.len() + line.len();
        self.lines.push((number, line.to_owned()));
        Ok(self.bytes <= MAX_GREP_BYTES)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> io::Result<bool> {
        self.binary = true;
        Ok(false)
    }
}

/// Reports end of file once stopped and spares searching the rest of a large file
struct Stopping<'a, R> {
    inner: R,
    stop: &'a AtomicBool,
}

impl<R: Read> Read for Stopping<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Ok(0);
        }
        self.inner.read(buffer)
    }
}

/// Uses `/` on Windows too to match the trailing `/` on directories, and Python still opens it
pub(crate) fn slashed(path: &Path) -> String {
    let text = path.to_string_lossy();
    // Long paths starting with `\\?\` only accept `\`
    match cfg!(windows) && !text.starts_with(r"\\?\") {
        true => text.replace('\\', "/"),
        false => text.into_owned(),
    }
}

fn walker(root: &Path) -> WalkBuilder {
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_name(|a, b| a.cmp(b));
    walk
}

#[cfg(test)]
mod tests {
    use crate::tests::{temp_dir, write_file};
    use crate::tools::files::*;
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn listed(root: &Path, recursive: bool) -> Vec<String> {
        let going = AtomicBool::new(false);
        ls(root, Path::new("."), recursive, &|_| true, &going).expect("ls")
    }

    #[test]
    fn walks_like_ripgrep() {
        let dir = temp_dir("files");
        write_file(&dir.join("deep/résumé.txt"), "first\nsecond".as_bytes());
        write_file(&dir.join("a.txt"), b"");
        assert_eq!(listed(&dir, false), ["a.txt", "deep/"]);

        let expected = ["a.txt", "deep/", "deep/résumé.txt"];
        assert_eq!(listed(&dir, true), expected);

        write_file(&dir.join(".gitignore"), b"build/\n");
        write_file(&dir.join("build/out.o"), b"");
        write_file(&dir.join(".git/HEAD"), b"");
        let expected = [".gitignore", "a.txt", "deep/", "deep/résumé.txt"];
        assert_eq!(listed(&dir, true), expected);

        let top = listed(&dir, false);
        assert!(top.contains(&"build/".to_owned()), "{top:?}");
        assert!(top.contains(&".git/".to_owned()), "{top:?}");

        let build = dir.join("build");
        assert_eq!(listed(&build, true), ["out.o"]);
        fs::remove_dir_all(&dir).ok();
    }

    /// Once set, the walk stops at once and file reads see end of file
    #[test]
    fn stop_honored() {
        let dir = temp_dir("stop");
        write_file(&dir.join("deep/one.txt"), "one\n".as_bytes());
        // Reads see end of file anyway, but a walk that goes on hangs opening this FIFO
        #[cfg(unix)]
        {
            let pipe = dir.join("deep/.gitignore");
            let named = CString::new(pipe.as_os_str().as_bytes()).expect("path");
            // SAFETY: The path is a NUL-terminated C string
            let made = unsafe { libc::mkfifo(named.as_ptr(), 0o600) };
            assert_eq!(made, 0);
        }
        let stopped = AtomicBool::new(true);
        let here = Path::new(".");
        assert!(ls(&dir, here, true, &|_| true, &stopped)
            .expect("ls")
            .is_empty());

        let (sender, receiver) = mpsc::channel();
        let root = dir.clone();
        thread::spawn(move || {
            let stopped = AtomicBool::new(true);
            let found = grep("one", &root, Path::new("."), None, &stopped);
            let empty = found.is_ok_and(|rows| rows.is_empty());
            sender.send(empty).ok();
        });
        let limit = Duration::from_secs(5);
        assert_eq!(receiver.recv_timeout(limit), Ok(true));

        let mut reader = Stopping {
            inner: "one two".as_bytes(),
            stop: &stopped,
        };
        let mut buffer = [0; 16];
        assert_eq!(reader.read(&mut buffer).expect("read"), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grep_walks() {
        let dir = temp_dir("grep");
        let write = |path: &str, data: &[u8]| write_file(&dir.join(path), data);
        write(".gitignore", "build/\ngenerated.rs\n".as_bytes());
        write("deep/code.rs", b"fn main() {}\nconst A: u8 = 1;\n");
        write("deep/generated.rs", b"fn main() {}\n");
        write("build/gen.rs", b"const B: u8 = 2;\n");
        write("blob.bin", b"const\0binary");
        // The NUL comes after the first chunk read in
        let late = [b"const Z: u8 = 0;\n".repeat(20_000), b"\0".to_vec()].concat();
        write("deep/late.log", &late);
        write("deep/crlf.txt", "déjà vu \r\nthe end:\r\n".as_bytes());
        let going = AtomicBool::new(false);
        let found = |pattern: &str, root: &Path, glob: Option<&str>| {
            let rows = grep(pattern, &dir, root, glob, &going).expect("grep");
            let mut shown = Vec::new();
            for (name, number, line) in rows {
                let path = Path::new(&name);
                let relative = path.strip_prefix(&dir).unwrap_or(path);
                let relative = slashed(relative);
                shown.push(format!("{relative}:{number}:{line}"));
            }
            shown
        };

        let code = dir.join("deep/code.rs");
        let crlf = dir.join("deep/crlf.txt");
        let constant = ["deep/code.rs:2:const A: u8 = 1;"];
        assert_eq!(found("const [A-Z]", &dir, None), constant);
        assert!(found("snake|const", &dir, Some("*.txt")).is_empty());
        let main = ["deep/code.rs:1:fn main() {}"];
        assert_eq!(found("(?i)FN MAIN", &dir, Some("*.rs")), main);
        assert_eq!(found("main", &code, Some("*.txt")), main);
        assert_eq!(found(":$", &crlf, None), ["deep/crlf.txt:2:the end:"]);
        assert_eq!(found(r"\s+$", &crlf, None), ["deep/crlf.txt:1:déjà vu "]);

        // Relative paths name files from where the path starts, without a leading `./`
        assert_eq!(found("const [A-Z]", Path::new("."), None), constant);
        assert_eq!(found("const [A-Z]", Path::new("./deep"), None), constant);
        let single = found("main", Path::new("deep/code.rs"), None);
        assert_eq!(single, main);

        // Points at the parenthesis as written, not the one grep-regex adds
        let unclosed = grep("café(", &dir, &dir, None, &going).expect_err("invalid");
        let message = unclosed.to_string();
        let expected =
            "grep: pattern: regex parse error:\n    café(\n        ^\nerror: unclosed group";
        assert!(message.starts_with(expected), "{message}");
        fs::remove_dir_all(&dir).ok();
    }

    /// Creating symlinks on Windows needs privileges
    #[test]
    #[cfg(unix)]
    fn link_loop_skipped() {
        let dir = temp_dir("link");
        write_file(&dir.join("deep/file.txt"), b"");
        symlink(&dir, dir.join("loop")).expect("symlink");

        let expected = ["deep/", "deep/file.txt", "loop/"];
        assert_eq!(listed(&dir, true), expected);

        // Links that may not be followed aren't shown as folders
        let going = AtomicBool::new(false);
        let unfollowed = ls(&dir, Path::new("."), false, &|_| false, &going).expect("ls");
        assert_eq!(unfollowed, ["deep/", "loop"]);
        fs::remove_dir_all(&dir).ok();
    }
}
