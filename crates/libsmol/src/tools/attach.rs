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

/// Images and videos are copied into `copies` and the session keeps using the copy
pub(crate) fn attach(mention: &Mention, cwd: &Path, copies: &Path) -> Attachment {
    let (path, wanted) = match mention {
        Mention::Resource { uri, text } => {
            let text = truncated(text, MAX_OBSERVATION_CHARS);
            return Attachment {
                path: uri.clone(),
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
            path,
            lines,
            text: Some(text),
        } => {
            let text = truncated(text, MAX_OBSERVATION_CHARS);
            return Attachment {
                path: shown_path(path, cwd),
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

    let mut shown = shown_path(path, cwd);
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

    fn media(attached: Attachment) -> Option<Media> {
        match attached.content {
            Content::Media { media } => Some(media),
            _ => None,
        }
    }

    #[test]
    fn mentions_attached() {
        let dir = temp_dir("mentions");
        let copies = dir.join("copies");
        let path = dir.join("menu.txt");
        write_file(&path, "café\ncrème\r\nbrûlée\npiñata\n".as_bytes());
        let mention = |lines, text: Option<&str>| {
            let file = Mention::File {
                path: path.clone(),
                lines: Some(lines),
                text: text.map(str::to_owned),
            };
            attach(&file, &dir, &copies)
        };

        let middle = mention((2, 3), None);
        let picked = Content::File {
            text: "crème\nbrûlée".to_owned(),
            lines: Some((2, 3)),
        };
        assert_eq!(middle.content, picked);
        assert_eq!(text(&mention((4, 9), None)), "piñata");

        let edited = mention((1, 1), Some("café au lait"));
        let sent = Content::File {
            text: "café au lait".to_owned(),
            lines: Some((1, 1)),
        };
        assert_eq!((edited.path.as_str(), edited.content), ("menu.txt", sent));

        let uri = "zed:///agent/diagnostics?include_warnings=true";
        let diagnostics = Mention::Resource {
            uri: uri.to_owned(),
            text: "error: unused café".to_owned(),
        };
        let resource = attach(&diagnostics, &dir, &copies);
        let sent = Content::Resource {
            text: "error: unused café".to_owned(),
        };
        assert_eq!((resource.path.as_str(), resource.content), (uri, sent));

        fs::remove_dir_all(&dir).ok();
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
        let undecoded = "[not attached: 4 bytes, not UTF-8; decode it in code]";
        assert_eq!(shown("latin1.txt").1, undecoded);

        let (_, cut) = shown("long.txt");
        assert!(cut.contains("\n[10 characters cut]\n"), "{cut}");
        let (_, huge) = shown("huge.txt");
        let refused = "[not attached: 1200000 bytes, too large; read the parts you need in code]";
        assert_eq!(huge, refused);

        let gone = dir.join("gone.txt");
        let outside = attach_path(&gone, Path::new("/elsewhere"), &copies);
        let path = slashed(&gone);
        assert_eq!(outside.path, path);
        // Windows words the reason in the system language
        let missing = "[not attached: ";
        assert!(text(&outside).starts_with(missing), "{outside:?}");
        #[cfg(unix)]
        assert_eq!(text(&outside), "[not attached: no such file or directory]");

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
            assert_eq!(shown("pipe").1, "[not attached: not a regular file]");
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn media_copied() {
        let dir = temp_dir("media");
        let copies = dir.join("copies");
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), "pixels".as_bytes()].concat();
        write_file(&dir.join("screenshot.PNG"), &png);
        write_file(&dir.join("clip.mp4"), b"\0\0\0\x18ftypisom");
        let jpeg = [b"\xff\xd8\xff".as_slice(), &[0; 62]].concat();
        write_file(&dir.join("large.jpg"), &jpeg);
        write_file(&dir.join("renamed.jpg"), &png);
        write_file(&dir.join("fake.png"), "not an image".as_bytes());

        let image = attach_path(&dir.join("screenshot.PNG"), &dir, &copies);
        assert_eq!(image.path, "screenshot.PNG");
        let copy = media(image).expect("image");
        assert_eq!(
            (copy.kind, copy.mime.as_str()),
            (MediaKind::Image, "image/png")
        );
        assert_eq!(copy.file.components().count(), 1, "{copy:?}");
        assert_eq!(copy.file.extension(), Some("png".as_ref()));
        let copied = fs::read(copies.join(&copy.file)).expect("copy");
        assert_eq!(copied, png);

        let renamed = attach_path(&dir.join("renamed.jpg"), &dir, &copies);
        let copy = media(renamed).expect("renamed");
        assert_eq!(copy.mime, "image/png");
        assert_eq!(copy.file.extension(), Some("png".as_ref()));
        let fake = attach_path(&dir.join("fake.png"), &dir, &copies);
        assert_eq!(text(&fake), "not an image");

        let video = attach_path(&dir.join("clip.mp4"), &dir, &copies);
        let copy = media(video).expect("video");
        assert_eq!(
            (copy.kind, copy.mime.as_str()),
            (MediaKind::Video, "video/mp4")
        );

        let large = attach_path(&dir.join("large.jpg"), &dir, &copies);
        let refused = "65 bytes, over the 64-byte image limit";
        assert_eq!(text(&large), format!("[not attached: {refused}]"));

        assert!(returned(&copies, b"BM").is_none());
        assert!(returned(&copies, b"\0\0\0\x1cftypavif").is_none());
        let media = returned(&copies, &png).expect("known").expect("copied");
        assert!(media.from_call);
        let mut big = png.clone();
        big.resize(65, 0);
        let refused_big = returned(&copies, &big);
        assert_eq!(refused_big, Some(Err(refused.to_owned())));

        // A file in the way keeps the copies directory from being created
        let blocked = dir.join("screenshot.PNG");
        let failed = returned(&blocked, &png).expect("known");
        let failed = failed.expect_err("blocked");
        assert!(failed.starts_with("not copied: "), "{failed}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn images_pasted() {
        let dir = temp_dir("pasted");
        let copies = dir.join("copies");
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), "pixels".as_bytes()].concat();
        let pasted = |data: &[u8]| {
            let data = STANDARD.encode(data);
            attach(&Mention::Image { data }, &dir, &copies)
        };

        let image = pasted(&png);
        let path = image.path.clone();
        let copied = media(image).expect("image");
        assert_eq!(copied.mime, "image/png");
        let copy = copies.join(&copied.file);
        assert_eq!(path, shown_path(&copy, &dir));
        assert_eq!(fs::read(&copy).expect("copy"), png);

        let video = pasted(b"\0\0\0\x18ftypisom");
        let refused = "[not attached: not a PNG, JPEG, GIF or WebP image]";
        assert_eq!(
            (video.path.as_str(), text(&video)),
            ("pasted image", refused)
        );
        let mut big = png.clone();
        big.resize(65, 0);
        let refused = "[not attached: 65 bytes, over the 64-byte image limit]";
        assert_eq!(text(&pasted(&big)), refused);
        let garbled = Mention::Image {
            data: "café".to_owned(),
        };
        let garbled = attach(&garbled, &dir, &copies);
        assert_eq!(text(&garbled), "[not attached: not base64]");

        fs::remove_dir_all(&dir).ok();
    }

    /// The limit is the one of what the content is, not of what its extension names
    #[test]
    fn limits_by_content() {
        let dir = temp_dir("limits");
        let copies = dir.join("copies");
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(100, b'p');
        let mut mp4 = b"\0\0\0\x18ftypisom".to_vec();
        mp4.resize(100, b'v');
        write_file(&dir.join("café.mov"), &png);
        write_file(&dir.join("naïve.png"), &mp4);

        let image = attach_path(&dir.join("café.mov"), &dir, &copies);
        let refused = "100 bytes, over the 64-byte image limit";
        let unattached = Content::File {
            text: format!("[not attached: {refused}]"),
            lines: None,
        };
        assert_eq!(image.content, unattached);
        let video = attach_path(&dir.join("naïve.png"), &dir, &copies);
        let kind = media(video).map(|media| media.kind);
        assert_eq!(kind, Some(MediaKind::Video));

        let viewed_image = viewed(&dir.join("café.mov"), &copies).expect_err("image");
        let shown = slashed(&dir.join("café.mov"));
        let expected = format!("view: {shown}: {refused}");
        assert_eq!(viewed_image.to_string(), expected);
        let viewed_video = viewed(&dir.join("naïve.png"), &copies).expect("video");
        assert_eq!(viewed_video.kind, MediaKind::Video);
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
