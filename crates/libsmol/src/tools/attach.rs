use crate::memory::{Attachment, MAX_OBSERVATION_CHARS};
use crate::model::{Media, MediaKind};
use crate::prompt;
use crate::tools::files::{ls, slashed};
use crate::tools::ToolError;
use crate::utils::{new_id, truncated};
use reaper::cause;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const MAX_ATTACH_BYTES: u64 = 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = match cfg!(test) {
    true => 64,
    false => 10 << 20,
};
const MAX_VIDEO_BYTES: u64 = match cfg!(test) {
    true => 64,
    false => 20 << 20,
};
/// Extension, kind and mime type
type MediaType = (&'static str, MediaKind, &'static str);
/// Extensions are lowercase and the first one listed for a mime type names the copies
const MEDIA: [MediaType; 8] = [
    ("png", MediaKind::Image, "image/png"),
    ("jpg", MediaKind::Image, "image/jpeg"),
    ("jpeg", MediaKind::Image, "image/jpeg"),
    ("gif", MediaKind::Image, "image/gif"),
    ("webp", MediaKind::Image, "image/webp"),
    ("mp4", MediaKind::Video, "video/mp4"),
    ("webm", MediaKind::Video, "video/webm"),
    ("mov", MediaKind::Video, "video/quicktime"),
];

/// Images and videos are copied into `copies` and the session keeps using the copy
pub(crate) fn attach(path: &Path, cwd: &Path, copies: &Path) -> Attachment {
    let mut shown = shown_path(path, cwd);
    let kind = kind_of(path);
    let limit = kind.map_or(MAX_ATTACH_BYTES, media_limit);

    let mut media = None;
    let content = match fs::metadata(path) {
        Err(e) => ToolError::os("read", &shown, e).to_string(),
        Ok(meta) if meta.is_dir() => {
            if !shown.ends_with('/') {
                shown.push('/');
            }
            let going = AtomicBool::new(false);
            match ls(path, false, &going) {
                Ok(names) => {
                    let names: Vec<_> = names.iter().map(|name| name.to_string_lossy()).collect();
                    names.join("\n")
                }
                Err(e) => e.to_string(),
            }
        }
        // Reading a pipe or a device would block forever
        Ok(meta) if !meta.is_file() => prompt::failure("read", &shown, prompt::IRREGULAR),
        Ok(meta) if meta.len() > limit => match kind {
            Some(kind) => oversized(kind, meta.len()),
            None => prompt::too_large(meta.len()),
        },
        Ok(meta) => match fs::read(path) {
            Err(e) => ToolError::os("read", &shown, e).to_string(),
            Ok(data) => {
                let found = sniffed(&data);
                // A media extension on content that is not media is read as a plain file
                match kind.and(found) {
                    Some(found) => match copied(copies, &data, found, false) {
                        Ok(copy) => {
                            media = Some(copy);
                            String::new()
                        }
                        Err(e) => ToolError::os("copy", &shown, e).to_string(),
                    },
                    None => match String::from_utf8(data) {
                        Ok(text) => text,
                        Err(_) => prompt::undecoded(meta.len()),
                    },
                }
            }
        },
    };
    let content = truncated(&content, MAX_OBSERVATION_CHARS);

    Attachment {
        path: shown,
        content,
        media,
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
        let irregular = prompt::failure("read", &shown, prompt::IRREGULAR);
        return Err(ToolError::Failed(irregular));
    }
    if meta.len() > media_limit(kind) {
        let refused = oversized(kind, meta.len());
        let refused = prompt::failure("view", &shown, &refused);
        return Err(ToolError::Failed(refused));
    }

    let data = fs::read(path).map_err(unreadable)?;
    let Some(found) = sniffed(&data) else {
        let refused = unviewable(&shown);
        return Err(ToolError::Failed(refused));
    };
    // Copying now keeps later writes to the file from changing what this step viewed
    let copy = copied(copies, &data, found, false);
    copy.map_err(|e| ToolError::os("copy", &shown, e))
}

/// For media in MCP results, an unknown format gives None and the reason names no file
pub(crate) fn kept(copies: &Path, data: &[u8]) -> Option<Result<Media, String>> {
    let found = sniffed(data)?;
    let (_, kind, _) = found;
    let size = data.len() as u64;
    if size > media_limit(kind) {
        let refused = oversized(kind, size);
        return Some(Err(refused));
    }

    let uncopied = |e: io::Error| {
        let reason = cause(&e);
        prompt::uncopied(&reason)
    };
    let copy = copied(copies, data, found, true);
    Some(copy.map_err(uncopied))
}

fn copied(copies: &Path, data: &[u8], found: MediaType, from_call: bool) -> io::Result<Media> {
    let (extension, kind, mime) = found;
    let id = new_id();
    let name = format!("{id}.{extension}");
    fs::create_dir_all(copies)?;
    let copy = copies.join(&name);
    fs::write(copy, data)?;

    let file = PathBuf::from(name);
    let mime = mime.to_owned();
    Ok(Media {
        kind,
        mime,
        file,
        from_call,
    })
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
    for (extension, kind, _) in MEDIA {
        match kind {
            MediaKind::Image => images.push(extension),
            MediaKind::Video => videos.push(extension),
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

/// Providers decode by the declared type and a mismatched type decodes to a blank image
fn sniffed(data: &[u8]) -> Option<MediaType> {
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

    let found = MEDIA.iter().find(|(_, _, known)| *known == mime);
    found.copied()
}

fn kind_of(path: &Path) -> Option<MediaKind> {
    let extension = path.extension().and_then(|extension| extension.to_str());
    let extension = extension.unwrap_or_default().to_ascii_lowercase();
    let found = MEDIA.iter().find(|(known, _, _)| *known == extension);
    found.map(|&(_, kind, _)| kind)
}

fn media_limit(kind: MediaKind) -> u64 {
    match kind {
        MediaKind::Image => MAX_IMAGE_BYTES,
        MediaKind::Video => MAX_VIDEO_BYTES,
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::MAX_OBSERVATION_CHARS;
    use crate::tests::{temp_dir, write_file};
    use crate::tools::attach::*;
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;

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
            let attached = attach(&dir.join(path), &dir, &copies);
            (attached.path, attached.content)
        };

        let main = ("src/main.rs".to_owned(), "fn main() {}\n".to_owned());
        assert_eq!(shown("src/main.rs"), main);
        assert_eq!(shown("src"), ("src/".to_owned(), "main.rs".to_owned()));
        assert_eq!(shown("").0, "./");
        let undecoded =
            "4 bytes, not UTF-8 text; if it is text in another encoding, decode it in code";
        assert_eq!(shown("latin1.txt").1, undecoded);

        let (_, cut) = shown("long.txt");
        assert!(cut.contains("\n[10 characters cut]\n"), "{cut}");
        let (_, huge) = shown("huge.txt");
        let refused = "1200000 bytes, too large to attach; read the parts you need in code";
        assert_eq!(huge, refused);

        let gone = dir.join("gone.txt");
        let outside = attach(&gone, Path::new("/elsewhere"), &copies);
        let path = slashed(&gone);
        assert_eq!(outside.path, path);
        // Windows words the reason in the system language
        let missing = format!("read: {path}: ");
        assert!(outside.content.starts_with(&missing), "{}", outside.content);
        #[cfg(unix)]
        assert!(outside.content.ends_with(": no such file or directory"));

        let trailing = format!("{}/src/", dir.display());
        let trailing = PathBuf::from(trailing);
        let trailing = attach(&trailing, Path::new("/elsewhere"), &copies);
        assert!(trailing.path.ends_with("/src/"), "{}", trailing.path);
        assert!(!trailing.path.ends_with("//"), "{}", trailing.path);

        #[cfg(unix)]
        {
            let pipe = dir.join("pipe");
            let named = CString::new(pipe.as_os_str().as_bytes()).expect("path");
            // SAFETY: The path is a NUL-terminated C string
            let made = unsafe { libc::mkfifo(named.as_ptr(), 0o600) };
            assert_eq!(made, 0);
            assert_eq!(shown("pipe").1, "read: pipe: not a regular file");
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
        write_file(&dir.join("large.jpg"), &[0; 65]);
        write_file(&dir.join("renamed.jpg"), &png);
        write_file(&dir.join("fake.png"), "not an image".as_bytes());

        let image = attach(&dir.join("screenshot.PNG"), &dir, &copies);
        let media = image.media.expect("image");
        assert_eq!(image.path, "screenshot.PNG");
        assert_eq!(
            (media.kind, media.mime.as_str()),
            (MediaKind::Image, "image/png")
        );
        assert_eq!(media.file.components().count(), 1, "{media:?}");
        assert_eq!(media.file.extension(), Some("png".as_ref()));
        let copied = fs::read(copies.join(&media.file)).expect("copy");
        assert_eq!(copied, png);

        let renamed = attach(&dir.join("renamed.jpg"), &dir, &copies);
        let media = renamed.media.expect("renamed");
        assert_eq!(media.mime, "image/png");
        assert_eq!(media.file.extension(), Some("png".as_ref()));
        let fake = attach(&dir.join("fake.png"), &dir, &copies);
        assert!(fake.media.is_none());
        assert_eq!(fake.content, "not an image");

        let video = attach(&dir.join("clip.mp4"), &dir, &copies);
        let media = video.media.expect("video");
        assert_eq!(
            (media.kind, media.mime.as_str()),
            (MediaKind::Video, "video/mp4")
        );

        let large = attach(&dir.join("large.jpg"), &dir, &copies);
        assert!(large.media.is_none());
        let refused = "65 bytes, over the 64-byte limit for an attached image";
        assert_eq!(large.content, refused);

        assert!(kept(&copies, b"BM").is_none());
        assert!(kept(&copies, b"\0\0\0\x1cftypavif").is_none());
        let media = kept(&copies, &png).expect("known").expect("kept");
        assert!(media.from_call);
        let mut big = png.clone();
        big.resize(65, 0);
        let refused_big = kept(&copies, &big);
        assert_eq!(refused_big, Some(Err(refused.to_owned())));
        // A file in the way keeps the copies directory from being created
        let blocked = dir.join("screenshot.PNG");
        let failed = kept(&blocked, &png).expect("known");
        let failed = failed.expect_err("blocked");
        assert!(failed.starts_with("not copied: "), "{failed}");

        fs::remove_dir_all(&dir).ok();
    }
}
