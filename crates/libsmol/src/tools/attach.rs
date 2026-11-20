use crate::model::{Media, MediaKind};
use crate::prompt;
use crate::steps::{Attachment, Content, MAX_OBSERVATION_CHARS};
use crate::tools::files::{ls, slashed};
use crate::tools::ToolError;
use crate::utils::{new_id, truncated};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reaper::cause;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const MAX_ATTACH_BYTES: u64 = 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = match cfg!(test) {
    true => 64,
    false => 10 << 20,
};
const MAX_VIDEO_BYTES: u64 = match cfg!(test) {
    true => 128,
    false => 20 << 20,
};
/// What the content turns out to be decides its limit, so a media extension gets the larger one first
const MAX_MEDIA_BYTES: u64 = match MAX_IMAGE_BYTES > MAX_VIDEO_BYTES {
    true => MAX_IMAGE_BYTES,
    false => MAX_VIDEO_BYTES,
};

#[derive(Clone, Copy)]
struct Format {
    extension: &'static str,
    kind: MediaKind,
    mime: &'static str,
}

/// Extensions are lowercase and the first one listed for a mime type names the copies
const MEDIA: [Format; 8] = [
    format("png", MediaKind::Image, "image/png"),
    format("jpg", MediaKind::Image, "image/jpeg"),
    format("jpeg", MediaKind::Image, "image/jpeg"),
    format("gif", MediaKind::Image, "image/gif"),
    format("webp", MediaKind::Image, "image/webp"),
    format("mp4", MediaKind::Video, "video/mp4"),
    format("webm", MediaKind::Video, "video/webm"),
    format("mov", MediaKind::Video, "video/quicktime"),
];

const fn format(extension: &'static str, kind: MediaKind, mime: &'static str) -> Format {
    Format {
        extension,
        kind,
        mime,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Mention {
    File {
        path: PathBuf,
        /// The first and last line, counted from 1
        lines: Option<(u32, u32)>,
        /// Sent along by the client and used instead of reading the file
        text: Option<String>,
    },
    /// Something that is not a local file, so its text always comes along
    Resource { uri: String, text: String },
    /// Base64 as the client sent it
    Image { data: String },
}

impl Mention {
    /// The name `attach` gives it before reading it, which an image lacks until it is copied
    pub(crate) fn name(&self, cwd: &Path) -> Option<String> {
        match self {
            Mention::File { path, .. } => Some(shown_path(path, cwd)),
            Mention::Resource { uri, .. } => Some(uri.clone()),
            Mention::Image { .. } => None,
        }
    }
}

/// Images and videos are copied into `copies` and the session keeps using the copy
pub(crate) fn attach(mention: &Mention, cwd: &Path, copies: &Path) -> Attachment {
    let named = mention.name(cwd).unwrap_or_default();
    let (path, wanted) = match mention {
        Mention::Resource { text, .. } => {
            let text = truncated(text, MAX_OBSERVATION_CHARS);
            return Attachment {
                path: named,
                content: Content::Resource { text },
            };
        }
        // Named by its copy, which the code can open
        Mention::Image { data } => {
            let unpasted = |reason: &str| Attachment {
                path: prompt::PASTED.to_owned(),
                content: Content::File {
                    text: prompt::unattached(reason),
                    lines: None,
                },
            };

            let Ok(decoded) = STANDARD.decode(data) else {
                return unpasted(prompt::NOT_BASE64);
            };
            let size = decoded.len() as u64;
            if size > MAX_IMAGE_BYTES {
                let refused = oversized(MediaKind::Image, size);
                return unpasted(&refused);
            }
            let found = sniffed(&decoded).filter(|found| found.kind == MediaKind::Image);
            let Some(found) = found else {
                return unpasted(prompt::NOT_IMAGE);
            };

            let media = match copied(copies, &decoded, found, false) {
                Ok(media) => media,
                Err(e) => {
                    let reason = cause(&e);
                    return unpasted(&reason);
                }
            };
            let copy = copies.join(&media.file);
            return Attachment {
                path: shown_path(&copy, cwd),
                content: Content::Media { media },
            };
        }
        // An editor sends what it shows, which may not be saved yet
        Mention::File {
            lines,
            text: Some(text),
            ..
        } => {
            let text = truncated(text, MAX_OBSERVATION_CHARS);
            return Attachment {
                path: named,
                content: Content::File {
                    text,
                    lines: *lines,
                },
            };
        }
        Mention::File {
            path,
            lines,
            text: None,
        } => (path, *lines),
    };

    let mut shown = named;
    let kind = kind_of(path);
    let limit = kind.map_or(MAX_ATTACH_BYTES, |_| MAX_MEDIA_BYTES);
    let metadata = fs::metadata(path);
    let directory = metadata.as_ref().is_ok_and(|meta| meta.is_dir());
    let mut lines = None;
    let mut media = None;
    // Err holds why the content could not be attached
    let read = match metadata {
        Err(e) => Err(cause(&e)),
        Ok(_) if directory => {
            if !shown.ends_with('/') {
                shown.push('/');
            }
            let going = AtomicBool::new(false);
            match ls(path, false, &going) {
                Ok(names) => {
                    let names: Vec<_> = names.iter().map(|name| name.to_string_lossy()).collect();
                    Ok(names.join("\n"))
                }
                Err(ToolError::Os { source, .. }) => Err(cause(&source)),
                Err(ToolError::Failed(reason)) => Err(reason),
            }
        }
        // Reading a pipe or a device would block forever
        Ok(meta) if !meta.is_file() => Err(prompt::IRREGULAR.to_owned()),
        Ok(meta) if meta.len() > limit => match kind {
            Some(kind) => Err(oversized(kind, meta.len())),
            None => Err(prompt::too_large(meta.len())),
        },
        Ok(meta) => match fs::read(path) {
            Err(e) => Err(cause(&e)),
            Ok(data) => {
                let found = kind.and_then(|_| checked(&data));
                let size = data.len() as u64;
                match found {
                    Some(Err(refused)) => Err(refused),
                    // A media extension on content that is not media is read as a plain file
                    None if size > MAX_ATTACH_BYTES => Err(prompt::too_large(size)),
                    Some(Ok(found)) => match copied(copies, &data, found, false) {
                        Ok(copy) => {
                            media = Some(copy);
                            Ok(String::new())
                        }
                        Err(e) => Err(cause(&e)),
                    },
                    None => match (String::from_utf8(data), wanted) {
                        (Ok(text), Some((first, last))) => {
                            lines = wanted;
                            let skipped = first.saturating_sub(1) as usize;
                            let count = last.saturating_sub(first) as usize + 1;
                            let picked = text.lines().skip(skipped).take(count);
                            let picked: Vec<&str> = picked.collect();
                            Ok(picked.join("\n"))
                        }
                        (Ok(text), None) => Ok(text),
                        (Err(_), _) => Err(prompt::undecoded(meta.len())),
                    },
                }
            }
        },
    };
    let text = match read {
        Ok(text) => truncated(&text, MAX_OBSERVATION_CHARS),
        Err(reason) => prompt::unattached(&reason),
    };
    let content = match (media, directory) {
        (Some(media), _) => Content::Media { media },
        (None, true) => Content::Directory { text },
        (None, false) => Content::File { text, lines },
    };

    Attachment {
        path: shown,
        content,
    }
}

pub(crate) fn viewed(path: &Path, copies: &Path) -> Result<Media, ToolError> {
    let shown = slashed(path);
    let Some(kind) = kind_of(path) else {
        let refused = unviewable(&shown);
        return Err(ToolError::Failed(refused));
    };

    let unreadable = |e| ToolError::os("read", &shown, e);
    let meta = fs::metadata(path).map_err(unreadable)?;
    // Reading a pipe or a device would block forever
    if !meta.is_file() {
        let irregular = prompt::failure("view", &shown, prompt::IRREGULAR);
        return Err(ToolError::Failed(irregular));
    }
    if meta.len() > MAX_MEDIA_BYTES {
        let refused = oversized(kind, meta.len());
        let refused = prompt::failure("view", &shown, &refused);
        return Err(ToolError::Failed(refused));
    }

    let data = fs::read(path).map_err(unreadable)?;
    let found = match checked(&data) {
        Some(Ok(found)) => found,
        Some(Err(refused)) => {
            let refused = prompt::failure("view", &shown, &refused);
            return Err(ToolError::Failed(refused));
        }
        None => {
            let refused = unviewable(&shown);
            return Err(ToolError::Failed(refused));
        }
    };
    // Copying now keeps later writes to the file from changing what this step viewed
    let copy = copied(copies, &data, found, false);
    copy.map_err(|e| ToolError::os("copy", &shown, e))
}

/// For media in MCP results, an unknown format gives None and the reason names no file
pub(crate) fn returned(copies: &Path, data: &[u8]) -> Option<Result<Media, String>> {
    let found = match checked(data)? {
        Ok(found) => found,
        Err(refused) => return Some(Err(refused)),
    };

    let uncopied = |e: io::Error| {
        let reason = cause(&e);
        prompt::uncopied(&reason)
    };
    let copy = copied(copies, data, found, true);
    Some(copy.map_err(uncopied))
}

fn copied(copies: &Path, data: &[u8], found: Format, from_call: bool) -> io::Result<Media> {
    let id = new_id();
    let name = format!("{id}.{}", found.extension);
    fs::create_dir_all(copies)?;
    let copy = copies.join(&name);
    fs::write(copy, data)?;

    Ok(Media {
        kind: found.kind,
        mime: found.mime.to_owned(),
        file: PathBuf::from(name),
        from_call,
    })
}

/// The code can write to the copies folder, so whatever stands in for a copy there is only read
/// when it is a plain file no larger than a copy of its kind can be
pub(crate) fn read_copy(path: &Path, kind: MediaKind) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    // Opening a pipe would block until something writes to it
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let meta = file.metadata()?;

    // A hard link reaches a file the code may not read where it is
    #[cfg(unix)]
    let linked = meta.nlink() > 1;
    #[cfg(not(unix))]
    let linked = false;
    let limit = media_limit(kind);
    if !meta.is_file() || linked || meta.len() > limit {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }

    // It may grow while it is read
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(data)
}

pub(crate) fn shown_path(path: &Path, cwd: &Path) -> String {
    let relative = path.strip_prefix(cwd).unwrap_or(path);
    let mut shown = slashed(relative);
    if shown.is_empty() {
        shown.push('.');
    }
    shown
}

fn unviewable(shown: &str) -> String {
    let mut images = Vec::new();
    let mut videos = Vec::new();
    for format in MEDIA {
        match format.kind {
            MediaKind::Image => images.push(format.extension),
            MediaKind::Video => videos.push(format.extension),
        }
    }

    let images = images.join(", ");
    let videos = videos.join(", ");
    prompt::unviewable(shown, &images, &videos)
}

fn oversized(kind: MediaKind, size: u64) -> String {
    let limit = media_limit(kind);
    let word = kind.word();
    prompt::media_too_large(word, size, limit)
}

/// The format of media within its kind's size limit, or why it can't be taken, and None for
/// content that is not media
fn checked(data: &[u8]) -> Option<Result<Format, String>> {
    let found = sniffed(data)?;
    let size = data.len() as u64;
    match size > media_limit(found.kind) {
        true => Some(Err(oversized(found.kind, size))),
        false => Some(Ok(found)),
    }
}

/// Providers decode by the declared type and a mismatched type decodes to a blank image
fn sniffed(data: &[u8]) -> Option<Format> {
    let boxed = data.get(4..8) == Some(b"ftyp".as_slice());
    let brand = data.get(8..12);
    // AVIF and HEIC images come in the same box
    let still = matches!(
        brand,
        Some(b"avif" | b"avis" | b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1")
    );
    let mime = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "image/gif"
    } else if data.starts_with(b"RIFF") && brand == Some(b"WEBP".as_slice()) {
        "image/webp"
    } else if data.starts_with(b"\x1a\x45\xdf\xa3") {
        "video/webm"
    } else if boxed && brand == Some(b"qt  ".as_slice()) {
        "video/quicktime"
    } else if boxed && !still {
        "video/mp4"
    } else {
        return None;
    };

    let found = MEDIA.iter().find(|format| format.mime == mime);
    found.copied()
}

fn kind_of(path: &Path) -> Option<MediaKind> {
    let extension = path.extension().and_then(|extension| extension.to_str());
    let extension = extension.unwrap_or_default().to_ascii_lowercase();
    let found = MEDIA.iter().find(|format| format.extension == extension);
    found.map(|format| format.kind)
}

fn media_limit(kind: MediaKind) -> u64 {
    match kind {
        MediaKind::Image => MAX_IMAGE_BYTES,
        MediaKind::Video => MAX_VIDEO_BYTES,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::steps::MAX_OBSERVATION_CHARS;
    use crate::tests::{temp_dir, write_file};
    use crate::tools::attach::*;
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    pub(crate) fn mentioned(path: &Path) -> Mention {
        Mention::File {
            path: path.to_owned(),
            lines: None,
            text: None,
        }
    }

    fn attach_path(path: &Path, cwd: &Path, copies: &Path) -> Attachment {
        let mention = mentioned(path);
        attach(&mention, cwd, copies)
    }

    /// Media has none
    fn text(attached: &Attachment) -> &str {
        match &attached.content {
            Content::File { text, .. }
            | Content::Directory { text }
            | Content::Resource { text } => text,
            Content::Media { .. } => "",
        }
    }

    #[test]
    fn attaches() {
        let dir = temp_dir("attach");
        write_file(&dir.join("src/main.rs"), b"fn main() {}\n");
        write_file(&dir.join("latin1.txt"), b"caf\xe9");
        let long = "é".repeat(MAX_OBSERVATION_CHARS + 10);
        write_file(&dir.join("long.txt"), long.as_bytes());
        let huge = "é".repeat(600_000);
        write_file(&dir.join("huge.txt"), huge.as_bytes());
        let copies = temp_dir("copies");
        let shown = |path: &str| {
            let attached = attach_path(&dir.join(path), &dir, &copies);
            let text = text(&attached).to_owned();
            (attached.path, text)
        };

        let main = ("src/main.rs".to_owned(), "fn main() {}\n".to_owned());
        assert_eq!(shown("src/main.rs"), main);
        let src = attach_path(&dir.join("src"), &dir, &copies);
        let listed = Content::Directory {
            text: "main.rs".to_owned(),
        };
        assert_eq!((src.path.as_str(), src.content), ("src/", listed));
        assert_eq!(shown("").0, "./");
        let undecoded = prompt::unattached(&prompt::undecoded(4));
        assert_eq!(shown("latin1.txt").1, undecoded);

        let (_, cut) = shown("long.txt");
        let marker = format!("\n{}\n", prompt::cut(10));
        assert!(cut.contains(&marker), "{cut}");
        let (_, huge) = shown("huge.txt");
        let refused = prompt::unattached(&prompt::too_large(1_200_000));
        assert_eq!(huge, refused);

        let gone = dir.join("gone.txt");
        let outside = attach_path(&gone, Path::new("/elsewhere"), &copies);
        let path = slashed(&gone);
        assert_eq!(outside.path, path);
        // Windows words the reason in the system language
        #[cfg(unix)]
        assert_eq!(
            text(&outside),
            prompt::unattached("no such file or directory")
        );

        let trailing = format!("{}/src/", dir.display());
        let trailing = PathBuf::from(trailing);
        let trailing = attach_path(&trailing, Path::new("/elsewhere"), &copies);
        assert!(trailing.path.ends_with("/src/"), "{}", trailing.path);
        assert!(!trailing.path.ends_with("//"), "{}", trailing.path);

        #[cfg(unix)]
        {
            let pipe = dir.join("pipe");
            let named = CString::new(pipe.as_os_str().as_bytes()).expect("path");
            // SAFETY: The path is a NUL-terminated C string
            let made = unsafe { libc::mkfifo(named.as_ptr(), 0o600) };
            assert_eq!(made, 0);
            assert_eq!(shown("pipe").1, prompt::unattached("not a regular file"));
        }

        fs::remove_dir_all(&dir).ok();
    }

    /// Links and pipes the code put in the copies folder are not read through
    #[cfg(unix)]
    #[test]
    fn copies_read_plainly() {
        let dir = temp_dir("read-copies");
        let copies = dir.join("copies");
        let secret = dir.join("secret/naïve.png");
        write_file(&secret, b"\x89PNG secret");
        write_file(&copies.join("café.png"), b"\x89PNG copy");
        let big = [b'x'; MAX_IMAGE_BYTES as usize + 1];
        write_file(&copies.join("big.png"), &big);
        symlink(&secret, copies.join("linked.png")).expect("link");
        fs::hard_link(&secret, copies.join("hard.png")).expect("link");
        let pipe = copies.join("pipe.png");
        let named = CString::new(pipe.as_os_str().as_bytes()).expect("path");
        // SAFETY: The path is a NUL-terminated C string
        let made = unsafe { libc::mkfifo(named.as_ptr(), 0o600) };
        assert_eq!(made, 0);

        let read = |name: &str| read_copy(&copies.join(name), MediaKind::Image).ok();
        let found = [
            read("café.png"),
            read("big.png"),
            read("linked.png"),
            read("hard.png"),
            read("pipe.png"),
        ];
        fs::remove_dir_all(&dir).ok();

        let copy = Some(b"\x89PNG copy".to_vec());
        assert_eq!(found, [copy, None, None, None, None]);
    }
}
