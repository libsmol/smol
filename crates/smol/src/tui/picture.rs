use crate::chat::resolved;
use crate::tui::{desktop, screen, theme};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::imageops::{self, FilterType};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgba, RgbaImage};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::hash::Hash;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::str;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use ureq::tls::{RootCerts, TlsConfig};
use ureq::Agent;
use url::Url;

/// Kitty's Unicode placeholder, which the diacritics after it number by row and column
const PLACEHOLDER: char = '\u{10EEEE}';
/// The first entries of Kitty's rowcolumn-diacritics.txt, one per row or column number
const DIACRITICS: [char; 100] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
    '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
    '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}', '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073A}', '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074A}',
];
/// A placeholder needs a diacritic for its column
const MAX_COLUMNS: u64 = DIACRITICS.len() as u64;
const MAX_ROWS: u64 = 20;
/// About how wide a cell is in pixels, so a small picture is not blown up
const CELL_PIXELS: u64 = 8;
/// The longer side of what is sent to a terminal that takes Kitty graphics
const MAX_SIDE_PIXELS: u32 = 1280;
/// Kitty and Ghostty drop the oldest images past 320 MB without a word,
/// which this many of the largest stay well under
const MAX_HELD_IMAGES: usize = 24;
/// The longer side kept for half blocks, a few times what the largest picture shows
const MAX_HALVES_PIXELS: u32 = 400;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// The longer side decoding allows, since a small file can declare a huge image that decoding
/// would allocate whole
const MAX_DECODED_SIDE: u32 = 16_384;
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
/// Each decode can take MAX_DECODED_BYTES
const MAX_LOADERS: usize = 2;
const FETCH_WAIT: Duration = Duration::from_secs(10);
/// Kitty takes a payload in base64 chunks of at most this many bytes
const MAX_CHUNK_BYTES: usize = 4096;
/// Colors carry the ids, and a larger image id would need a third diacritic
const MAX_ID: u32 = (1 << 24) - 1;

/// A file's modification time and size, which change when it is written again
type Stamp = (SystemTime, u64);
type Lines = Vec<Vec<Span<'static>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Source {
    File(PathBuf),
    Web(String),
}

impl Source {
    /// Where a Markdown image points, with paths relative to `cwd`, or None for other schemes
    pub(crate) fn of(address: &str, cwd: &Path) -> Option<Source> {
        let Ok(url) = Url::parse(address) else {
            let path = resolved(address, cwd);
            return Some(Source::File(path));
        };
        match url.scheme() {
            "http" | "https" => Some(Source::Web(address.to_owned())),
            "file" => url.to_file_path().ok().map(Source::File),
            // A Windows path parses with its drive letter as the scheme
            scheme if scheme.len() == 1 => Some(Source::File(PathBuf::from(address))),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Load {
    Loading(Instant),
    Ready(Arc<Picture>),
    /// Shown as its alt text, so why it failed goes unsaid
    Failed,
}

#[derive(Debug)]
pub(crate) struct Picture {
    natural: (u32, u32),
    look: Look,
    /// A web picture as downloaded, for opening it whole, while a file is read again then
    bytes: Option<Vec<u8>>,
    format: ImageFormat,
}

#[derive(Debug)]
enum Look {
    /// Scaled down to what half blocks can show, with the rows last drawn and their size,
    /// since every relayout draws all of them
    Pixels {
        pixels: RgbaImage,
        cached: Mutex<Option<((u32, u32), Lines)>>,
    },
    /// For terminals that take Kitty graphics, which scale it themselves
    Png(Vec<u8>),
}

#[derive(Default)]
struct Store {
    /// With the stamp a file had when it was read
    states: HashMap<Source, (Load, Option<Stamp>)>,
    /// Kitty image ids by picture, and while the terminal has the image, the last send it showed in
    images: HashMap<Source, (u32, Option<u64>)>,
    /// Kitty placement ids by picture and size in cells, and whether the terminal has the placement
    placed: HashMap<(Source, u32, u32), (u32, bool)>,
    /// Ids the terminal still holds for pictures loaded again or of an earlier conversation
    retired: Vec<u32>,
    last_id: u32,
    sends: u64,
    /// Pictures waiting for a loader, which takes the latest first, as the bottom of a conversation
    /// is laid out last
    queue: Vec<(Source, Option<Stamp>)>,
    loaders: usize,
}

thread_local! {
    /// Only the UI thread draws, and tests each get their own
    static STORE: Arc<Mutex<Store>> = Arc::default();
}
/// Counts loads that ended, so the transcript lays its rows out again
static LOADED: AtomicU64 = AtomicU64::new(0);
/// Kitty images are drawn only outside a multiplexer,
/// which keeps graphics to itself unless set up to pass them through
pub(crate) static KITTY: LazyLock<bool> = LazyLock::new(|| {
    // Tests run in whatever terminal started them
    if cfg!(test) {
        return false;
    }
    let term = env::var("TERM").unwrap_or_default();
    let program = env::var("TERM_PROGRAM").unwrap_or_default();
    let remote = ["SSH_TTY", "SSH_CONNECTION"].map(env::var_os);
    let ssh = remote.iter().any(Option::is_some);
    let multiplexed = ["TMUX", "STY", "ZELLIJ"].map(env::var_os);
    let nested = multiplexed.iter().any(Option::is_some);
    !nested && kitty_terminal(&term, &program, ssh)
});

/// Terminals known to draw Kitty's Unicode placeholders, by the TERM they set,
/// since other variables pass on to terminals started from their shells
fn kitty_terminal(term: &str, program: &str, ssh: bool) -> bool {
    let own = matches!(term, "xterm-kitty" | "xterm-ghostty");
    // Ghostty's ssh-env sends TERM_PROGRAM with a plain xterm TERM, which a terminal started
    // from Ghostty's shell may also have
    let forwarded = ssh && program == "ghostty" && term == "xterm-256color";
    own || forwarded
}

pub(crate) fn loaded() -> u64 {
    LOADED.load(Ordering::Relaxed)
}

pub(crate) fn loading() -> bool {
    let shared = STORE.with(Arc::clone);
    let store = shared.lock().expect("pictures");
    let waiting = |(state, _): &(Load, Option<Stamp>)| matches!(state, Load::Loading(_));
    store.states.values().any(waiting)
}

/// Starts loading a picture in the background the first time it is asked for,
/// and again once its file was written again or appeared
pub(crate) fn state(source: &Source) -> Load {
    let stamp = match source {
        Source::File(path) => match fs::metadata(path) {
            Ok(meta) => meta.modified().ok().map(|at| (at, meta.len())),
            Err(_) => None,
        },
        Source::Web(_) => None,
    };
    let shared = STORE.with(Arc::clone);
    let mut store = shared.lock().expect("pictures");
    if let Some((state, read)) = store.states.get(source) {
        if matches!(state, Load::Loading(_)) || *read == stamp {
            return state.clone();
        }
    }
    let started = Load::Loading(Instant::now());
    store
        .states
        .insert(source.clone(), (started.clone(), stamp));
    store.queue.push((source.clone(), stamp));
    if store.loaders == MAX_LOADERS {
        return started;
    }
    store.loaders += 1;
    drop(store);

    let kitty = *KITTY;
    thread::spawn(move || loop {
        let mut store = shared.lock().expect("pictures");
        let Some((source, stamp)) = store.queue.pop() else {
            store.loaders -= 1;
            return;
        };
        drop(store);

        // A panic must neither end the loader uncounted nor restore the terminal
        let loaded = screen::caught(|| load(&source, kitty));
        let loaded = match loaded.flatten() {
            Some(picture) => Load::Ready(Arc::new(picture)),
            None => Load::Failed,
        };
        let mut store = shared.lock().expect("pictures");
        store.states.insert(source.clone(), (loaded, stamp));

        // The terminal's copy of an earlier version goes at the next send, with its placements
        if let Some((id, Some(_))) = store.images.remove(&source) {
            store.retired.push(id);
        }
        store.placed.retain(|key, _| key.0 != source);
        LOADED.fetch_add(1, Ordering::Relaxed);
    });
    started
}

/// Frees the pictures of a conversation that was replaced
pub(crate) fn clear() {
    let shared = STORE.with(Arc::clone);
    let mut store = shared.lock().expect("pictures");
    let Store {
        states,
        images,
        placed,
        retired,
        queue,
        ..
    } = &mut *store;

    // No loader takes the queued ones now, while those at work end on their own
    for (source, _) in queue.drain(..) {
        states.remove(&source);
    }
    states.retain(|_, (state, _)| matches!(state, Load::Loading(_)));

    // The terminal holds these until the next send, so ids keep counting on rather than start over
    let held = images.drain().filter(|(_, (_, shown))| shown.is_some());
    let ids = held.map(|(_, (id, _))| id);
    retired.extend(ids);
    placed.clear();
}

fn load(source: &Source, kitty: bool) -> Option<Picture> {
    let bytes = match source {
        Source::File(path) => read(path)?,
        Source::Web(url) => {
            let roots = TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build();
            let config = Agent::config_builder()
                .timeout_global(Some(FETCH_WAIT))
                .tls_config(roots)
                .build();
            let agent = Agent::new_with_config(config);

            let response = agent.get(url).call().ok()?;
            let kind = response.headers().get("content-type");
            let kind = kind.and_then(|kind| kind.to_str().ok()).unwrap_or_default();
            if !kind.starts_with("image/") {
                return None;
            }
            let mut body = response.into_body();
            let limited = body.with_config().limit(MAX_IMAGE_BYTES);
            limited.read_to_vec().ok()?
        }
    };

    let format = image::guess_format(&bytes).ok()?;
    let known = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::WebP,
    ];
    if !known.contains(&format) {
        return None;
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODED_SIDE);
    limits.max_image_height = Some(MAX_DECODED_SIDE);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().ok()?;
    // The decoded image counts against the limit before the decoder's own buffers, as in decode()
    limits.reserve(decoder.total_bytes()).ok()?;
    decoder.set_limits(limits).ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let decoded = DynamicImage::from_decoder(decoder).ok()?;

    let turned = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    let natural = match turned {
        true => (decoded.height(), decoded.width()),
        false => (decoded.width(), decoded.height()),
    };
    let side = match kitty {
        true => MAX_SIDE_PIXELS,
        false => MAX_HALVES_PIXELS,
    };
    let mut shrunk = match natural.0.max(natural.1) > side {
        true => decoded.thumbnail(side, side),
        false => decoded,
    };
    // Turning the shrunk copy spares copying the whole decoded image
    shrunk.apply_orientation(orientation);
    let pixels = shrunk.to_rgba8();
    let look = match kitty {
        true => {
            let mut png = Vec::new();
            let mut cursor = Cursor::new(&mut png);
            pixels.write_to(&mut cursor, ImageFormat::Png).ok()?;
            Look::Png(png)
        }
        false => Look::Pixels {
            pixels,
            cached: Mutex::default(),
        },
    };

    let bytes = match source {
        Source::File(_) => None,
        Source::Web(_) => Some(bytes),
    };
    Some(Picture {
        natural,
        look,
        bytes,
        format,
    })
}

/// Devices and pipes report no size and can be read forever, so only regular files are read
fn read(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }

    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Columns and rows a picture `width` by `height` pixels takes in at most `room` columns,
/// with a cell about twice as tall as it is wide
pub(crate) fn cells(width: u32, height: u32, room: usize) -> (u32, u32) {
    let (width, height) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let room = u64::try_from(room).unwrap_or(u64::MAX);
    let natural = width.div_ceil(CELL_PIXELS);
    let mut columns = natural.min(room).clamp(1, MAX_COLUMNS);
    let mut rows = (columns * height).div_ceil(width * 2).max(1);
    if rows > MAX_ROWS {
        rows = MAX_ROWS;
        columns = (rows * 2 * width).div_ceil(height).clamp(1, columns);
    }
    (columns as u32, rows as u32)
}

/// Rows of cells drawing `picture` in at most `room` columns
pub(crate) fn drawn(source: &Source, picture: &Picture, room: usize) -> Lines {
    let (width, height) = picture.natural;
    let (columns, rows) = cells(width, height, room);
    match &picture.look {
        Look::Png(_) => {
            let shared = STORE.with(Arc::clone);
            let mut store = shared.lock().expect("pictures");
            let Store {
                images,
                placed,
                last_id,
                ..
            } = &mut *store;
            let image = numbered(images, source.clone(), last_id);
            let key = (source.clone(), columns, rows);
            let placement = numbered(placed, key, last_id);
            placeholders(image, placement, columns, rows)
        }
        Look::Pixels { pixels, cached } => {
            let mut cached = cached.lock().expect("halves");
            if let Some((size, lines)) = &*cached {
                if *size == (columns, rows) {
                    return lines.clone();
                }
            }
            let lines = halves(pixels, columns, rows);
            *cached = Some(((columns, rows), lines.clone()));
            lines
        }
    }
}

/// The id of `key`, numbered next when it has none yet
fn numbered<K: Eq + Hash, V: Default>(
    ids: &mut HashMap<K, (u32, V)>,
    key: K,
    last: &mut u32,
) -> u32 {
    let next = || {
        *last = *last % MAX_ID + 1;
        (*last, V::default())
    };
    ids.entry(key).or_insert_with(next).0
}

/// The image id is the placeholders' foreground color
fn placeholders(image: u32, placement: u32, columns: u32, rows: u32) -> Lines {
    let [_, red, green, blue] = image.to_be_bytes();
    let [_, under_red, under_green, under_blue] = placement.to_be_bytes();
    // Kitty reads the placement, and so the size it draws, from the underline color
    let style = Style::new()
        .fg(Color::Rgb(red, green, blue))
        .underline_color(Color::Rgb(under_red, under_green, under_blue));
    let mut lines = Vec::new();
    for row in DIACRITICS.iter().take(rows as usize) {
        let mut cells = String::new();
        for column in DIACRITICS.iter().take(columns as usize) {
            cells.push(PLACEHOLDER);
            cells.push(*row);
            cells.push(*column);
        }
        lines.push(vec![Span::styled(cells, style)]);
    }
    lines
}

/// Each cell shows two pixels, the upper half block in the color above and its background below
fn halves(pixels: &RgbaImage, columns: u32, rows: u32) -> Lines {
    let small = imageops::resize(pixels, columns, rows * 2, FilterType::Triangle);
    let mut lines = Vec::new();
    for row in 0..rows {
        let mut spans = Vec::new();
        for column in 0..columns {
            let top = blended(small.get_pixel(column, row * 2));
            let bottom = blended(small.get_pixel(column, row * 2 + 1));
            let style = Style::new().fg(top).bg(bottom);
            spans.push(Span::styled("▀", style));
        }
        lines.push(spans);
    }
    lines
}

/// Transparent parts show the conversation's background
fn blended(pixel: &Rgba<u8>) -> Color {
    let [red, green, blue, alpha] = pixel.0.map(u16::from);
    let (under_red, under_green, under_blue) = match theme::BG {
        Color::Rgb(red, green, blue) => (red, green, blue),
        _ => (0, 0, 0),
    };
    let mix =
        |over: u16, under: u8| ((over * alpha + u16::from(under) * (255 - alpha)) / 255) as u8;
    Color::Rgb(
        mix(red, under_red),
        mix(green, under_green),
        mix(blue, under_blue),
    )
}

/// Deletes the Kitty images the terminal no longer needs, sends it the first one on screen
/// it lacks, and a placement for every size the ones it holds were drawn at, which their
/// placeholders then show
pub(crate) fn send(out: &mut impl Write, visible: &[Source]) -> io::Result<()> {
    let shared = STORE.with(Arc::clone);
    let mut store = shared.lock().expect("pictures");
    store.sends += 1;
    let Store {
        states,
        images,
        placed,
        retired,
        sends,
        ..
    } = &mut *store;
    let mut wrote = false;

    // Deleting an image deletes its placements too
    for id in retired.drain(..) {
        write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
        wrote = true;
    }

    for source in visible {
        if let Some((_, Some(shown))) = images.get_mut(source) {
            *shown = *sends;
        }
    }

    // One image a pass lets the keys typed meanwhile in
    let mut next = None;
    for source in visible {
        let Some(&(id, None)) = images.get(source) else {
            continue;
        };
        let Some((Load::Ready(picture), _)) = states.get(source) else {
            continue;
        };
        if let Look::Png(png) = &picture.look {
            next = Some((source, id, png));
            break;
        }
    }

    // The held image that showed longest ago makes room, and goes again once it shows
    let held = images.values().filter(|(_, shown)| shown.is_some()).count();
    let hidden = images
        .iter()
        .filter(|(_, (_, shown))| shown.is_some_and(|at| at < *sends));
    let stalest = hidden.min_by_key(|(_, (_, shown))| *shown);
    let stalest = stalest.map(|(key, (id, _))| (key.clone(), *id));
    let evicted = stalest.filter(|_| next.is_some() && held >= MAX_HELD_IMAGES);
    if let Some((gone, id)) = evicted {
        write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
        images.insert(gone.clone(), (id, None));
        for ((key, _, _), (_, sent)) in placed.iter_mut() {
            if *key == gone {
                *sent = false;
            }
        }
    }

    if let Some((source, id, png)) = next {
        let data = STANDARD.encode(png);
        let chunks: Vec<&[u8]> = data.as_bytes().chunks(MAX_CHUNK_BYTES).collect();
        for (index, chunk) in chunks.iter().enumerate() {
            let more = u8::from(index + 1 < chunks.len());
            let chunk = str::from_utf8(chunk).unwrap_or_default();
            // q=2 keeps the terminal from answering, which would arrive as typed keys
            match index {
                0 => write!(out, "\x1b_Ga=t,i={id},f=100,q=2,m={more};{chunk}\x1b\\")?,
                _ => write!(out, "\x1b_Gm={more};{chunk}\x1b\\")?,
            }
        }
        images.insert(source.clone(), (id, Some(*sends)));
        wrote = true;
    }

    // A placement needs the image the terminal already has
    for ((source, columns, rows), (id, sent)) in placed.iter_mut() {
        let Some(&(image, Some(_))) = images.get(source) else {
            continue;
        };
        if *sent {
            continue;
        }

        write!(
            out,
            "\x1b_Ga=p,U=1,i={image},p={id},c={columns},r={rows},q=2\x1b\\"
        )?;
        *sent = true;
        wrote = true;
    }

    match wrote {
        true => out.flush(),
        false => Ok(()),
    }
}

/// Writes the picture's file, named after its real format, where only the user can read it,
/// because the system opens a file by its name and the model chose this one
pub(crate) fn open(source: &Source) -> Result<(), String> {
    let Load::Ready(picture) = state(source) else {
        return Err("image: not loaded yet".to_owned());
    };
    let read_again = match source {
        Source::File(path) => read(path),
        Source::Web(_) => None,
    };
    let Some(bytes) = picture.bytes.as_ref().or(read_again.as_ref()) else {
        return Err("image: can't be read again".to_owned());
    };
    // The file may have changed since it was drawn
    let format = image::guess_format(bytes).ok();
    if format != Some(picture.format) {
        return Err("image: no longer the image shown".to_owned());
    }

    let extension = picture.format.extensions_str().first().unwrap_or(&"png");
    let path = desktop::saved("image", source, extension, bytes)?;
    desktop::open(&path)
}

#[cfg(test)]
mod tests {
    use crate::tui::picture::*;
    use crate::tui::tests::{settled, written};
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageBuffer, ImageEncoder, Rgb, RgbImage};
    use std::{process, slice};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn sources_parsed() {
        let cwd = Path::new("/home/zoë/naïve");
        let local = Source::of("charts/café.png", cwd);
        assert_eq!(local, Some(Source::File(cwd.join("charts/café.png"))));
        let absolute = Source::of("/tmp/résumé.png", cwd);
        assert_eq!(
            absolute,
            Some(Source::File(PathBuf::from("/tmp/résumé.png")))
        );
        // A file URL has a drive letter on Windows
        let path = env::temp_dir().join("résumé.png");
        let file = Url::from_file_path(&path).expect("url");
        assert_eq!(Source::of(file.as_str(), cwd), Some(Source::File(path)));
        let drive = r"C:\Users\zoë\café.png";
        let windows = Source::of(drive, cwd);
        assert_eq!(windows, Some(Source::File(PathBuf::from(drive))));
        let web = "https://example.com/a/café.png?size=2";
        assert_eq!(Source::of(web, cwd), Some(Source::Web(web.to_owned())));
        assert_eq!(Source::of("data:image/png;base64,AAAA", cwd), None);
    }

    #[test]
    fn kitty_detected() {
        let cases = [
            ("xterm-kitty", "", false, true),
            ("xterm-ghostty", "ghostty", false, true),
            ("xterm-256color", "ghostty", true, true),
            ("xterm-256color", "ghostty", false, false),
            ("xterm-256color", "vscode", true, false),
            ("alacritty", "ghostty", true, false),
        ];
        for (term, program, ssh, kitty) in cases {
            let found = kitty_terminal(term, program, ssh);
            assert_eq!(found, kitty, "{term} {program} {ssh}");
        }
    }

    #[test]
    fn cells_fitted() {
        // A cell is twice as tall as wide, so a square takes twice the columns of its rows
        assert_eq!(cells(80, 80, 100), (10, 5));
        assert_eq!(cells(1600, 400, 60), (60, 8));
        assert_eq!(cells(400, 1600, 100), (10, 20));
        assert_eq!(cells(3000, 100, 500), (100, 2));
        assert_eq!(cells(1, 1, 0), (1, 1));
    }

    #[test]
    fn halves_colored() {
        let painted = |_: u32, y: u32| match y < 2 {
            true => Rgba([255, 0, 0, 255]),
            false => Rgba([0, 0, 255, 255]),
        };
        let pixels: RgbaImage = ImageBuffer::from_fn(4, 4, painted);
        let lines = halves(&pixels, 1, 1);
        assert_eq!(lines.len(), 1);
        let cell = &lines[0][0];
        assert_eq!(cell.content, "▀");
        // Scaling down blends a little across the edge between the halves
        let (Some(Color::Rgb(red, _, blue)), Some(Color::Rgb(under_red, _, under_blue))) =
            (cell.style.fg, cell.style.bg)
        else {
            panic!("{cell:?}");
        };
        assert!(red > 200 && blue < 60, "{cell:?}");
        assert!(under_blue > 200 && under_red < 60, "{cell:?}");
    }

    #[test]
    fn transparent_blended() {
        let clear = blended(&Rgba([255, 255, 255, 0]));
        assert_eq!(clear, theme::BG);
        assert_eq!(blended(&Rgba([1, 2, 3, 255])), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn placeholders_numbered() {
        let lines = placeholders(0x01_02_03, 0x04_05_06, 3, 2);
        assert_eq!(lines.len(), 2);
        let span = &lines[1][0];
        assert_eq!(span.style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(span.style.underline_color, Some(Color::Rgb(4, 5, 6)));
        assert_eq!(span.content.width(), 3);
        let second: Vec<char> = span.content.chars().skip(3).take(3).collect();
        assert_eq!(second, [PLACEHOLDER, DIACRITICS[1], DIACRITICS[1]]);
    }

    /// Only pictures on screen go to the terminal, one a call, and the one shown longest ago
    /// makes room past the limit and goes again once it shows
    #[test]
    fn kitty_sent_visible() {
        let path = written("kitty", 80, 80);
        let picture = Arc::new(load(&Source::File(path.clone()), true).expect("load"));
        fs::remove_dir_all(path.parent().expect("dir")).ok();
        let web = |name: &str| Source::Web(format!("https://a.io/{name}.png"));
        let [a, b, c] = ["café", "naïve", "résumé"].map(web);
        let shared = STORE.with(Arc::clone);
        for source in [&a, &b, &c] {
            let ready = Load::Ready(picture.clone());
            let mut store = shared.lock().expect("pictures");
            store.states.insert(source.clone(), (ready, None));
        }

        let id = |source: &Source| {
            let lines = drawn(source, &picture, 100);
            let Some(Color::Rgb(red, green, blue)) = lines[0][0].style.fg else {
                panic!("{:?}", lines[0][0]);
            };
            u32::from_be_bytes([0, red, green, blue])
        };
        let sent = |visible: &[Source]| {
            let mut out = Vec::new();
            send(&mut out, visible).expect("send");
            String::from_utf8(out).expect("utf8")
        };
        let image = |id: u32| format!("\x1b_Ga=t,i={id},f=100,q=2,m=0;");
        let deleted = |id: u32| format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        let (first, second) = (id(&a), id(&b));

        assert_eq!(sent(&[]), "");
        let shown = sent(slice::from_ref(&a));
        assert!(shown.starts_with(&image(first)), "{shown}");
        let placed = format!("\x1b_Ga=p,U=1,i={first},p=");
        assert!(shown.contains(&placed), "{shown}");
        assert!(shown.ends_with(",c=10,r=5,q=2\x1b\\"), "{shown}");
        assert_eq!(shown.matches("\x1b_Ga=").count(), 2, "{shown}");

        // Drawn at another width, it gets a placement of its own
        drawn(&a, &picture, 6);
        let narrower = sent(slice::from_ref(&a));
        assert!(narrower.starts_with(&placed), "{narrower}");
        assert!(narrower.ends_with(",c=6,r=3,q=2\x1b\\"), "{narrower}");
        let both = sent(&[a.clone(), b.clone()]);
        assert!(both.starts_with(&image(second)), "{both}");
        assert!(!both.contains(&image(first)), "{both}");
        assert_eq!(sent(slice::from_ref(&b)), "");

        shared.lock().expect("pictures").retired.push(7);
        assert_eq!(sent(&[]), deleted(7));
        assert_eq!(sent(&[]), "");

        let third = id(&c);
        let mut store = shared.lock().expect("pictures");
        let latest = Some(store.sends);
        for n in 2..MAX_HELD_IMAGES {
            let filler = web(&n.to_string());
            store.images.insert(filler, (1000 + n as u32, latest));
        }
        drop(store);
        let full = sent(slice::from_ref(&c));
        assert!(full.starts_with(&deleted(first)), "{full}");
        assert!(full.contains(&image(third)), "{full}");
        let again = sent(slice::from_ref(&a));
        assert!(again.starts_with(&deleted(second)), "{again}");
        assert!(again.contains(&image(first)), "{again}");
        assert!(again.contains(&placed), "{again}");
    }

    /// A conversation's pictures go with it, and the terminal's ids are deleted at the next send
    #[test]
    fn pictures_cleared() {
        let picture = Picture {
            natural: (1, 1),
            look: Look::Png(Vec::new()),
            bytes: None,
            format: ImageFormat::Png,
        };
        let ready = Source::Web("https://a.io/café.png".to_owned());
        let failed = Source::Web("https://a.io/naïve.png".to_owned());
        let busy = Source::Web("https://a.io/résumé.png".to_owned());
        let shared = STORE.with(Arc::clone);
        let mut store = shared.lock().expect("pictures");
        let loaded = Load::Ready(Arc::new(picture));
        store.states.insert(ready.clone(), (loaded, None));
        store.states.insert(failed.clone(), (Load::Failed, None));
        // A loader is at work on one, so the next ones wait for it
        let started = Load::Loading(Instant::now());
        store.states.insert(busy.clone(), (started, None));
        store.loaders = MAX_LOADERS;
        store.images.insert(ready.clone(), (7, Some(1)));
        store.images.insert(failed, (8, None));
        store.placed.insert((ready, 2, 1), (9, true));
        store.last_id = 9;
        drop(store);
        for name in ["a", "b", "c"] {
            let missing = Source::File(PathBuf::from(format!("/nowhere/{name}.png")));
            assert!(matches!(state(&missing), Load::Loading(_)));
        }

        clear();
        let store = shared.lock().expect("pictures");
        let left: Vec<&Source> = store.states.keys().collect();
        assert_eq!(left, [&busy]);
        assert!(store.queue.is_empty() && store.images.is_empty() && store.placed.is_empty());
        assert_eq!(store.retired, [7]);
        assert_eq!(store.last_id, 9);
    }

    #[test]
    fn failures_settled() {
        let dir = env::temp_dir().join(format!("smol-picture-text-{}", process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let text = dir.join("notes.png");
        fs::write(&text, "café notes").expect("write");
        let state = settled(&Source::File(text));
        fs::remove_dir_all(&dir).ok();
        assert!(matches!(state, Load::Failed), "{state:?}");

        // Wider than decoding allows, though the file is small
        let wide = written("wide", MAX_DECODED_SIDE + 1, 2);
        let state = settled(&Source::File(wide.clone()));
        fs::remove_dir_all(wide.parent().expect("dir")).ok();
        assert!(matches!(state, Load::Failed), "{state:?}");
    }

    /// A photo tagged as turned shows upright, with its sides swapped
    #[test]
    fn exif_turned() {
        let painted = |_: u32, y: u32| match y < 4 {
            true => Rgb([255, 0, 0]),
            false => Rgb([0, 0, 255]),
        };
        let pixels: RgbImage = ImageBuffer::from_fn(16, 8, painted);
        // A little-endian TIFF whose one entry is orientation 6, a quarter turn clockwise
        let exif = [
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let mut jpeg = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 95);
        encoder.set_exif_metadata(exif.to_vec()).expect("exif");
        encoder.encode_image(&pixels).expect("encode");
        let dir = env::temp_dir().join(format!("smol-picture-exif-{}", process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("café.jpg");
        fs::write(&path, &jpeg).expect("write");

        let loaded = load(&Source::File(path), false);
        fs::remove_dir_all(&dir).ok();
        let picture = loaded.expect("load");
        assert_eq!(picture.natural, (8, 16));
        let Look::Pixels { pixels, .. } = &picture.look else {
            panic!("{picture:?}");
        };
        assert_eq!(pixels.dimensions(), (8, 16));
        // The red top is now on the right, and JPEG blurs the colors a little
        let [red, _, blue, _] = pixels.get_pixel(0, 8).0;
        assert!(blue > 200 && red < 60, "{red} {blue}");
        let [red, _, blue, _] = pixels.get_pixel(7, 8).0;
        assert!(red > 200 && blue < 60, "{red} {blue}");
    }

    /// A file written again, or one that appears after it was asked for, loads anew
    #[test]
    fn rewritten_reloaded() {
        let path = written("rewritten", 16, 8);
        let source = Source::File(path.clone());
        let Load::Ready(first) = settled(&source) else {
            panic!("{:?}", settled(&source));
        };
        assert_eq!(first.natural, (16, 8));

        written("rewritten", 24, 8);
        let Load::Ready(second) = settled(&source) else {
            panic!("{:?}", settled(&source));
        };
        assert_eq!(second.natural, (24, 8));

        let later = Source::File(path.with_file_name("later.png"));
        assert!(matches!(settled(&later), Load::Failed));
        fs::copy(&path, path.with_file_name("later.png")).expect("copy");
        let state = settled(&later);
        fs::remove_dir_all(path.parent().expect("dir")).ok();
        assert!(matches!(state, Load::Ready(_)), "{state:?}");
    }

    /// A device reports no size and would be read forever
    #[test]
    #[cfg(unix)]
    fn devices_refused() {
        assert_eq!(read(Path::new("/dev/zero")), None);
        assert_eq!(read(Path::new("/dev")), None);
    }
}
