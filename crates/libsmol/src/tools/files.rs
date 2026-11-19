use crate::prompt;
use crate::tools::ToolError;
use grep_matcher::LineTerminator;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex_syntax::Parser;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_GREP_BYTES: usize = 32 * 1024 * 1024;

/// Once `stop` is set it returns early with a result nobody reads
pub(crate) fn ls(
    root: &Path,
    recursive: bool,
    stop: &AtomicBool,
) -> Result<Vec<OsString>, ToolError> {
    let shown = root.to_string_lossy();
    let failed = |e| ToolError::os("list", &shown, e);
    // Read it even when recursing to report a missing path or a non-directory
    let entries = fs::read_dir(root).map_err(failed)?;
    let named = |relative: &Path, full: &Path| {
        let mut name = raw(relative);
        if full.is_dir() {
            name.push("/");
        }
        name
    };

    let mut names = Vec::new();
    if recursive {
        for entry in walker(root).build().flatten() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let full = entry.path();
            let relative = full.strip_prefix(root).unwrap_or(full);
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

/// Rows are (path, line number, line, whether it matched) for the matching lines and the `context`
/// lines around them, and once `stop` is set it returns early with a result nobody reads
pub(crate) fn grep(
    pattern: &str,
    root: &Path,
    glob: Option<&str>,
    context: usize,
    stop: &AtomicBool,
) -> Result<Vec<(OsString, u64, String, bool)>, ToolError> {
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

    let shown = root.to_string_lossy();
    let failed = |e| ToolError::os("search", &shown, e);
    let meta = fs::metadata(root).map_err(failed)?;
    // The walk skips unreadable directories but an unreadable root is an error
    if meta.is_dir() {
        fs::read_dir(root).map_err(failed)?;
    }

    // As a walker override the glob would let matching files past `.gitignore`
    let mut globs = OverrideBuilder::new(root);
    if let Some(glob) = glob {
        globs.add(glob).map_err(|e| invalid("glob", &e))?;
    }
    let globs = globs.build().map_err(|e| invalid("glob", &e))?;

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_terminator(LineTerminator::crlf())
        .before_context(context)
        .after_context(context)
        .build();
    let mut found = Vec::new();
    let mut bytes = 0;

    for entry in walker(root).build().flatten() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }

        let path = entry.path();
        if path != root && globs.matched(path, false).is_ignore() {
            continue;
        }
        let relative = path.strip_prefix(".").unwrap_or(path);
        let mut matches = Matches {
            name: raw(relative),
            lines: Vec::new(),
            bytes,
            binary: false,
        };

        let searched = match File::open(path) {
            Ok(inner) => {
                let reader = Stopping { inner, stop };
                searcher.search_reader(&matcher, reader, &mut matches)
            }
            Err(e) => Err(e),
        };
        // Unreadable files in a directory are skipped but an unreadable root file is an error
        if path == root {
            searched.map_err(failed)?;
        }
        if matches.binary {
            continue;
        }

        bytes = matches.bytes;
        for (number, line, matched) in matches.lines {
            found.push((matches.name.clone(), number, line, matched));
        }
        if bytes > MAX_GREP_BYTES {
            let limit = MAX_GREP_BYTES >> 20;
            let reason = prompt::too_many(limit);
            let message = prompt::failure("grep", &shown, &reason);
            return Err(ToolError::Failed(message));
        }
    }

    Ok(found)
}

/// Lines of one file, all dropped when a later NUL shows the file is binary
struct Matches {
    name: OsString,
    lines: Vec<(u64, String, bool)>,
    /// Bytes collected so far, earlier files included
    bytes: usize,
    binary: bool,
}

impl Matches {
    /// Returns false once the lines collected pass the limit
    fn add(&mut self, number: Option<u64>, bytes: &[u8], matched: bool) -> bool {
        let line = String::from_utf8_lossy(bytes);
        let line = line.trim_end_matches(['\n', '\r']);
        self.bytes += self.name.len() + line.len();
        let number = number.unwrap_or_default();
        self.lines.push((number, line.to_owned(), matched));
        self.bytes <= MAX_GREP_BYTES
    }
}

impl Sink for Matches {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> io::Result<bool> {
        let added = self.add(matched.line_number(), matched.bytes(), true);
        Ok(added)
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        let added = self.add(context.line_number(), context.bytes(), false);
        Ok(added)
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

/// Names for Python keep non-UTF-8 bytes as they are for Python to decode with surrogateescape
fn raw(path: &Path) -> OsString {
    match cfg!(windows) {
        true => slashed(path).into(),
        false => path.as_os_str().to_owned(),
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
    use std::os::unix::fs::symlink;

    fn listed(root: &Path, recursive: bool) -> Vec<OsString> {
        let going = AtomicBool::new(false);
        ls(root, recursive, &going).expect("ls")
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
        assert!(top.contains(&OsString::from("build/")), "{top:?}");
        assert!(top.contains(&OsString::from(".git/")), "{top:?}");

        let build = dir.join("build");
        assert_eq!(listed(&build, true), ["out.o"]);
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
        fs::remove_dir_all(&dir).ok();
    }
}
