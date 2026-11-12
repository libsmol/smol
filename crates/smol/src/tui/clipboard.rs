use crate::tui::remote;
use arboard::{Clipboard, Error};
use image::{ImageFormat, RgbaImage};
use std::io::Cursor;

const NO_IMAGE: &str = "clipboard: no image";
pub(crate) const UNREACHABLE: &str =
    "clipboard: unreachable from here; smol web takes images pasted in a browser";
const UNREADABLE: &str = "clipboard: image unreadable";

/// The image on the user's clipboard
pub(crate) fn image() -> Result<Vec<u8>, String> {
    // Over SSH this machine's clipboard is someone else's or nobody's
    let read = match remote() {
        true => terminal::image().or_else(system),
        false => system().or_else(terminal::image),
    };
    read.unwrap_or_else(|| Err(UNREACHABLE.to_owned()))
}

/// None when this machine has no clipboard to read, as over SSH without a display
pub(crate) fn system() -> Option<Result<Vec<u8>, String>> {
    let mut clipboard = Clipboard::new().ok()?;
    let read = clipboard.get_image();
    let image = match read {
        Ok(image) => image,
        Err(Error::ClipboardNotSupported) => return None,
        Err(Error::ContentNotAvailable) => return Some(Err(NO_IMAGE.to_owned())),
        Err(Error::ClipboardOccupied) => {
            let busy = "clipboard: held by another program".to_owned();
            return Some(Err(busy));
        }
        Err(Error::Unknown { description }) => {
            let failed = format!("clipboard: {description}");
            return Some(Err(failed));
        }
        Err(_) => return Some(Err(UNREADABLE.to_owned())),
    };

    let width = image.width as u32;
    let height = image.height as u32;
    let bytes = image.bytes.into_owned();
    let Some(pixels) = RgbaImage::from_raw(width, height, bytes) else {
        return Some(Err(UNREADABLE.to_owned()));
    };
    let mut png = Cursor::new(Vec::new());
    let written = pixels.write_to(&mut png, ImageFormat::Png);
    let encoded = match written {
        Ok(()) => Ok(png.into_inner()),
        Err(e) => Err(format!("clipboard: {e}")),
    };
    Some(encoded)
}

/// Windows hands smol console events, not the terminal's bytes, so no reply can be read
#[cfg(not(unix))]
mod terminal {
    pub(crate) fn image() -> Option<Result<Vec<u8>, String>> {
        None
    }
}

/// Reads the clipboard of the terminal's machine through OSC 5522, which works over SSH
#[cfg(unix)]
mod terminal {
    use crate::chat::IMAGE_FORMATS;
    use crate::tui::clipboard::{NO_IMAGE, UNREADABLE};
    use crate::tui::multiplexed;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use reaper::cause;
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::{self, stdout, ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use std::{mem, ptr};

    /// Asks whether the terminal knows OSC 5522
    const QUERY: &str = "\x1b[?5522$p";
    /// Lists the clipboard's MIME types, which terminals do without asking the user
    const LIST: &str = "\x1b]5522;type=read;Lg==\x1b\\";
    /// "smol", the name the terminal shows when it asks the user
    const NAME: &str = "c21vbA==";
    const OSC_START: &[u8] = b"\x1b]5522;";
    const MODE_START: &[u8] = b"\x1b[?5522;";
    const ESC: u8 = 0x1b;
    const BEL: u8 = 0x07;
    const MAX_READ_BYTES: usize = 64 * 1024;
    /// For the answer to `QUERY`, and between reads while waiting on the terminal
    const REPLY_WAIT: Duration = match cfg!(test) {
        true => Duration::from_millis(50),
        false => Duration::from_secs(2),
    };

    /// Lets the user allow every read of this run at once
    static PASSWORD: OnceLock<String> = OnceLock::new();

    #[derive(Debug, PartialEq)]
    enum Reply {
        /// Whether the terminal knows the mode
        Mode(bool),
        /// With the payload still in base64
        Read {
            status: String,
            mime: String,
            payload: String,
        },
    }

    /// None when the terminal can't pass its clipboard on
    pub(crate) fn image() -> Option<Result<Vec<u8>, String>> {
        if multiplexed() {
            return None;
        }
        let tty = File::open("/dev/tty").ok()?;

        let send = |text: &str| {
            let mut out = stdout();
            out.write_all(text.as_bytes())?;
            out.flush()
        };
        // Reads past the event reader, which reads nothing while the app handles a key
        let receive = |wait: Duration| -> io::Result<Vec<u8>> {
            let fd = tty.as_raw_fd();
            // A set holds only the first FD_SETSIZE descriptors
            if fd as usize >= libc::FD_SETSIZE {
                return Err(io::Error::from(ErrorKind::InvalidInput));
            }

            // macOS can't poll a terminal device, which select can wait on
            // SAFETY: An all-zero fd_set is a valid empty set
            let mut ready: libc::fd_set = unsafe { mem::zeroed() };
            // SAFETY: The descriptor is below FD_SETSIZE
            unsafe { libc::FD_SET(fd, &mut ready) };
            let mut timeout = libc::timeval {
                tv_sec: wait.as_secs() as libc::time_t,
                tv_usec: wait.subsec_micros() as libc::suseconds_t,
            };
            let null = ptr::null_mut();
            // SAFETY: The set and the timeout outlive the call
            let count = unsafe { libc::select(fd + 1, &mut ready, null, null, &mut timeout) };
            match count {
                0 => return Ok(Vec::new()),
                count if count < 0 => {
                    let error = io::Error::last_os_error();
                    return match error.kind() {
                        ErrorKind::Interrupted => Ok(Vec::new()),
                        _ => Err(error),
                    };
                }
                _ => {}
            }

            let mut bytes = vec![0; MAX_READ_BYTES];
            let read = (&tty).read(&mut bytes)?;
            if read == 0 {
                return Err(io::Error::from(ErrorKind::UnexpectedEof));
            }
            bytes.truncate(read);
            Ok(bytes)
        };
        let read = exchanged(send, receive);
        read.transpose()
    }

    /// None when the terminal doesn't know OSC 5522, else lists the clipboard's types and reads
    /// the first image type among them, dropping whatever else arrives, such as keys typed meanwhile
    fn exchanged(
        mut send: impl FnMut(&str) -> io::Result<()>,
        mut receive: impl FnMut(Duration) -> io::Result<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, String> {
        let failed = |e: io::Error| {
            let reason = cause(&e);
            format!("clipboard: {reason}")
        };

        let mut buffer = Vec::new();
        let mut ready = VecDeque::new();
        // Without a deadline it waits as long as the terminal takes
        let mut next = |deadline: Option<Instant>| -> Result<Option<Reply>, String> {
            loop {
                if let Some(reply) = ready.pop_front() {
                    return Ok(Some(reply));
                }
                let wait = match deadline {
                    Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                    None => REPLY_WAIT,
                };
                if wait.is_zero() {
                    return Ok(None);
                }
                let bytes = receive(wait).map_err(failed)?;
                buffer.extend(bytes);
                let replies = parsed(&mut buffer);
                ready.extend(replies);
            }
        };

        // crossterm types out a reply that comes after smol stopped waiting, so only a terminal
        // that said it knows the mode is asked, and then waited for to the end
        send(QUERY).map_err(failed)?;
        let deadline = Instant::now() + REPLY_WAIT;
        let known = loop {
            match next(Some(deadline))? {
                None => return Ok(None),
                Some(Reply::Mode(known)) => break known,
                Some(Reply::Read { .. }) => {}
            }
        };
        if !known {
            return Ok(None);
        }

        let mut answer = |wanted: &str| -> Result<Vec<u8>, String> {
            let mut data = Vec::new();
            loop {
                let Some(Reply::Read {
                    status,
                    mime,
                    payload,
                }) = next(None)?
                else {
                    continue;
                };
                match status.as_str() {
                    "OK" => {}
                    "DATA" if mime == wanted => {
                        let Ok(part) = STANDARD.decode(payload) else {
                            return Err(UNREADABLE.to_owned());
                        };
                        data.extend(part);
                    }
                    "DATA" => {}
                    "DONE" => return Ok(data),
                    "EPERM" => return Err("clipboard: reading it was refused".to_owned()),
                    "EBUSY" => return Err("clipboard: the terminal is busy".to_owned()),
                    _ => return Err("clipboard: the terminal can't read it".to_owned()),
                }
            }
        };

        send(LIST).map_err(failed)?;
        let listed = answer(".")?;
        let listed = String::from_utf8_lossy(&listed);
        let offered: Vec<&str> = listed.split_whitespace().collect();
        let types = IMAGE_FORMATS.map(|format| format.to_mime_type());
        let Some(mime) = types.into_iter().find(|mime| offered.contains(mime)) else {
            return Err(NO_IMAGE.to_owned());
        };

        let made = || {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).ok();
            STANDARD.encode(bytes)
        };
        let password = STANDARD.encode(PASSWORD.get_or_init(made));
        let encoded = STANDARD.encode(mime);
        let asked = format!("\x1b]5522;type=read:name={NAME}:pw={password};{encoded}\x1b\\");
        send(&asked).map_err(failed)?;
        let image = answer(mime)?;

        match image.is_empty() {
            true => Err(NO_IMAGE.to_owned()),
            false => Ok(Some(image)),
        }
    }

    /// Takes the replies complete in `buffer`, and keeps only what may begin the next one
    fn parsed(buffer: &mut Vec<u8>) -> Vec<Reply> {
        let mut replies = Vec::new();
        let mut at = 0;
        let kept = loop {
            let Some(found) = buffer[at..].iter().position(|&byte| byte == ESC) else {
                break buffer.len();
            };
            let start = at + found;
            let rest = &buffer[start..];
            if OSC_START.starts_with(rest) || MODE_START.starts_with(rest) {
                break start;
            }

            if let Some(body) = rest.strip_prefix(MODE_START) {
                let Some(end) = body.windows(2).position(|pair| pair == b"$y") else {
                    break start;
                };
                let known = !matches!(&body[..end], b"0" | b"4");
                replies.push(Reply::Mode(known));
                at = start + MODE_START.len() + end + 2;
                continue;
            }
            let Some(body) = rest.strip_prefix(OSC_START) else {
                at = start + 1;
                continue;
            };

            let Some(end) = body.iter().position(|&byte| byte == BEL || byte == ESC) else {
                break start;
            };
            let terminator = match body[end] {
                BEL => 1,
                _ => 2,
            };
            if end + terminator > body.len() {
                break start;
            }
            at = start + OSC_START.len() + end + terminator;

            let inside = String::from_utf8_lossy(&body[..end]);
            let (metadata, payload) = inside.split_once(';').unwrap_or((&inside, ""));
            let mut read = false;
            let mut status = String::new();
            let mut mime = String::new();
            for pair in metadata.split(':') {
                match pair.split_once('=') {
                    Some(("type", "read")) => read = true,
                    Some(("status", value)) => status = value.to_owned(),
                    Some(("mime", value)) => {
                        let decoded = STANDARD.decode(value).unwrap_or_default();
                        mime = String::from_utf8_lossy(&decoded).into_owned();
                    }
                    _ => {}
                }
            }
            if read {
                let payload = payload.to_owned();
                replies.push(Reply::Read {
                    status,
                    mime,
                    payload,
                });
            }
        };

        buffer.drain(..kept);
        replies
    }

    #[cfg(test)]
    mod tests {
        use crate::tui::clipboard::terminal::*;
        use std::thread;

        fn read(status: &str, mime: &str, data: &[u8]) -> String {
            let mut metadata = format!("type=read:status={status}");
            if !mime.is_empty() {
                let mime = STANDARD.encode(mime);
                metadata += &format!(":mime={mime}");
            }
            let payload = match data.is_empty() {
                true => String::new(),
                false => format!(";{}", STANDARD.encode(data)),
            };
            format!("\x1b]5522;{metadata}{payload}\x1b\\")
        }

        /// Runs the exchange against replies that arrive in the pieces given, where None is a
        /// wait that passes in silence and the end of them is the end of input, and returns what
        /// was asked with the result
        fn exchange(pieces: &[Option<&[u8]>]) -> (Vec<String>, Result<Option<Vec<u8>>, String>) {
            let mut asked = Vec::new();
            let mut pieces: VecDeque<Option<Vec<u8>>> = pieces
                .iter()
                .map(|piece| piece.map(<[u8]>::to_vec))
                .collect();
            let send = |text: &str| {
                asked.push(text.to_owned());
                Ok(())
            };
            let receive = |wait: Duration| match pieces.pop_front() {
                Some(Some(piece)) => Ok(piece),
                Some(None) => {
                    thread::sleep(wait);
                    Ok(Vec::new())
                }
                None => Err(io::Error::from(ErrorKind::UnexpectedEof)),
            };
            let result = exchanged(send, receive);
            (asked, result)
        }

        #[test]
        fn replies_parsed() {
            let listed = read("DATA", ".", b"text/plain image/png\n");
            let mut buffer = format!("café\x1b[?5522;1$y\x1b[A{listed}\x1b]55").into_bytes();
            let replies = parsed(&mut buffer);
            let list = Reply::Read {
                status: "DATA".to_owned(),
                mime: ".".to_owned(),
                payload: STANDARD.encode("text/plain image/png\n"),
            };
            assert_eq!(replies, [Reply::Mode(true), list]);
            assert_eq!(buffer, b"\x1b]55");

            // A terminator cut between two reads waits for its second byte
            buffer.extend(b"22;type=read:status=OK\x1b");
            assert_eq!(parsed(&mut buffer), []);
            buffer.extend(b"\\\x1b]5522;type=read:status=DONE\x07\x1b[?5522;0$y");
            let status = |status: &str| Reply::Read {
                status: status.to_owned(),
                mime: String::new(),
                payload: String::new(),
            };
            let expected = [status("OK"), status("DONE"), Reply::Mode(false)];
            assert_eq!(parsed(&mut buffer), expected);
            assert!(buffer.is_empty());
        }

        #[test]
        fn image_exchanged() {
            let png = [b"\x89PNG\r\n\x1a\n".as_slice(), "naïve pixels".as_bytes()].concat();
            let (first, second) = png.split_at(5);
            let listed = [
                read("OK", "", b""),
                read("DATA", ".", b"text/plain ima"),
                read("DATA", ".", b"ge/jpeg image/png\n"),
                read("DONE", "", b""),
            ];
            let sent = [
                read("OK", "", b""),
                read("DATA", "image/png", first),
                read("DATA", "image/png", second),
                read("DONE", "", b""),
            ];
            let listed = listed.concat();
            let sent = format!("café{}", sent.concat());
            let (head, tail) = sent.split_at(17);

            let pieces = [
                Some(b"\x1b[?5522;1$y".as_slice()),
                Some(listed.as_bytes()),
                None,
                Some(head.as_bytes()),
                Some(tail.as_bytes()),
            ];
            let (asked, image) = exchange(&pieces);
            assert_eq!(asked[..2], [QUERY, LIST]);
            let request = "\x1b]5522;type=read:name=c21vbA==:pw=";
            assert!(asked[2].starts_with(request), "{asked:?}");
            assert!(asked[2].ends_with(";aW1hZ2UvcG5n\x1b\\"), "{asked:?}");
            assert_eq!(image, Ok(Some(png)));
        }

        #[test]
        fn exchange_refused() {
            let (asked, silent) = exchange(&[None]);
            assert_eq!((asked, silent), (vec![QUERY.to_owned()], Ok(None)));
            let (asked, unknown) = exchange(&[Some(b"\x1b[?5522;0$y")]);
            assert_eq!((asked.len(), unknown), (1, Ok(None)));

            let known = Some(b"\x1b[?5522;2$y".as_slice());
            let listed = [
                read("OK", "", b""),
                read("DATA", ".", b"text/plain\n"),
                read("DONE", "", b""),
            ];
            let listed = listed.concat();
            let (_, textual) = exchange(&[known, Some(listed.as_bytes())]);
            assert_eq!(textual, Err(NO_IMAGE.to_owned()));

            let listed = [
                read("OK", "", b""),
                read("DATA", ".", b"image/gif\n"),
                read("DONE", "", b""),
            ];
            let listed = listed.concat();
            let denied = read("EPERM", "", b"");
            let pieces = [known, Some(listed.as_bytes()), Some(denied.as_bytes())];
            let (_, denied) = exchange(&pieces);
            assert_eq!(denied, Err("clipboard: reading it was refused".to_owned()));

            let (_, gone) = exchange(&[known]);
            assert!(gone.is_err_and(|reason| reason.starts_with("clipboard: ")));
        }
    }
}
