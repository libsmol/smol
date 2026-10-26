use crate::prompt;
#[cfg(windows)]
use encoding_rs::Encoding;
use reaper::{cause, terminal_view};
use reqwest::{Client, ClientBuilder};
use serde_json::Value;
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
#[cfg(test)]
use std::future;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
#[cfg(windows)]
use std::str;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};
#[cfg(test)]
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{oneshot, watch};
#[cfg(windows)]
use windows_sys::Win32::Globalization::GetACP;

/// Must hold `MAX_OBSERVATION_CHARS` characters at up to 4 bytes each
pub(crate) const KEPT_BYTES: usize = 256 * 1024;
pub(crate) const SHOWN_EVERY: Duration = Duration::from_millis(100);
const MAX_EXCERPT_CHARS: usize = 300;
/// The read timeout starts when the request is sent,
/// so only a shorter connect wait reports an unreachable host as a connect failure
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// A single thread keeps session tasks from running
/// between the cancel and the sender drop when a session closes
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    let built = Builder::new_current_thread().enable_all().build();
    built.expect("runtime")
});

/// A long-lived thread drives the runtime
/// because PDEATHSIG kills programs spawned from a test thread that has exited
#[cfg(test)]
static DRIVER: LazyLock<()> = LazyLock::new(|| {
    let (ready, started) = mpsc::channel();
    let driving = move || {
        let parked = async move {
            ready.send(()).ok();
            future::pending::<()>().await
        };
        RUNTIME.block_on(parked)
    };
    thread::spawn(driving);
    started.recv().ok();
});

pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    #[cfg(test)]
    LazyLock::force(&DRIVER);
    RUNTIME.block_on(future)
}

pub(crate) type Reply = Result<Value, String>;

#[derive(Default)]
pub(crate) struct Waiting {
    next: AtomicU64,
    waiters: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
}

impl Waiting {
    pub(crate) fn ask(&self) -> (u64, oneshot::Receiver<Reply>) {
        let (sender, receiver) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut waiters = self.waiters.lock().expect("waiting lock");
        waiters.insert(id, sender);
        (id, receiver)
    }

    pub(crate) fn take(&self, id: u64) -> Option<oneshot::Sender<Reply>> {
        let mut waiters = self.waiters.lock().expect("waiting lock");
        waiters.remove(&id)
    }

    pub(crate) fn answer(&self, id: u64, reply: Reply) {
        if let Some(sender) = self.take(id) {
            sender.send(reply).ok();
        }
    }
}

/// Cancels by turn and never needs resetting
#[derive(Debug, Clone, Default)]
pub(crate) struct Interrupt(Arc<Turns>);

#[derive(Debug, Default)]
struct Turns {
    turn: AtomicU64,
    /// Every turn before this one is cancelled
    cancelled: watch::Sender<u64>,
}

impl Interrupt {
    pub(crate) fn cancel(&self, turn: u64) {
        let before = turn.saturating_add(1);
        self.0.cancelled.send_if_modified(|cancelled| {
            let later = before > *cancelled;
            *cancelled = (*cancelled).max(before);
            later
        });
    }

    pub(crate) fn begin(&self, turn: u64) {
        self.0.turn.store(turn, Ordering::SeqCst);
    }

    pub(crate) fn cancelled(&self, turn: u64) -> bool {
        turn < *self.0.cancelled.borrow()
    }

    pub(crate) fn triggered(&self) -> bool {
        let turn = self.0.turn.load(Ordering::SeqCst);
        self.cancelled(turn)
    }

    pub(crate) async fn wait(&self) {
        let mut cancelled = self.0.cancelled.subscribe();
        let current = |&before: &u64| self.0.turn.load(Ordering::SeqCst) < before;
        // The sender lives in the same struct and can't close first
        cancelled.wait_for(current).await.ok();
    }
}

pub(crate) fn truncated(text: &str, max: usize) -> String {
    truncated_with(text, max, 0)
}

/// `cut` counts characters already dropped from the middle
pub(crate) fn truncated_with(text: &str, max: usize, cut: usize) -> String {
    let count = text.chars().count();
    if count <= max && cut == 0 {
        return text.to_owned();
    }

    let max = max.min(count);
    let kept = max / 2;
    let head: String = text.chars().take(max - kept).collect();
    let tail: String = text.chars().skip(count - kept).collect();
    let cut = count - max + cut;
    let marker = prompt::cut(cut);

    format!("{head}\n{marker}\n{tail}")
}

/// Keeps about `half` bytes at each end without splitting a UTF-8 character
pub(crate) struct Clip {
    half: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    /// Bytes dropped from the middle
    dropped: usize,
    /// Characters dropped from the middle
    cut: usize,
    /// Bytes received as of the last `fresh`
    reported: usize,
}

impl Clip {
    pub(crate) fn new(half: usize) -> Self {
        Self {
            half,
            head: Vec::new(),
            tail: VecDeque::new(),
            dropped: 0,
            cut: 0,
            reported: 0,
        }
    }

    pub(crate) fn push(&mut self, mut bytes: &[u8]) {
        let continues = |byte: &u8| byte & 0xC0 == 0x80;
        // The head runs to the end of a character,
        // and the next chunk completes one split across chunks
        if self.tail.is_empty() {
            let room = self.half.saturating_sub(self.head.len());
            let mut room = room.min(bytes.len());
            while room < bytes.len() && continues(&bytes[room]) {
                room += 1;
            }
            let (now, later) = bytes.split_at(room);
            self.head.extend_from_slice(now);
            bytes = later;
        }
        self.tail.extend(bytes);

        // The tail starts at the beginning of a character
        let mut over = self.tail.len().saturating_sub(self.half);
        while over > 0 && over < self.tail.len() && continues(&self.tail[over]) {
            over += 1;
        }
        let dropped = self.tail.drain(..over);
        self.cut += dropped.filter(|byte| !continues(byte)).count();
        self.dropped += over;
    }

    /// The kept text and the number of characters dropped from the middle
    pub(crate) fn text(&self) -> (String, usize) {
        let mut bytes = self.head.clone();
        bytes.extend(&self.tail);
        let text = decoded(&bytes);
        (text, self.cut)
    }

    pub(crate) fn shown(&self, max: usize) -> String {
        let (text, cut) = self.text();
        let text = terminal_view(&text);
        truncated_with(&text, max, cut)
    }

    /// Returns text only when more output arrived since the last call
    pub(crate) fn fresh(&mut self, max: usize) -> Option<String> {
        let pushed = self.head.len() + self.tail.len() + self.dropped;
        if pushed == self.reported {
            return None;
        }
        self.reported = pushed;
        let text = self.shown(max);
        Some(text)
    }
}

#[cfg(not(windows))]
fn decoded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Some programs write in the system code page with \r\n line ends,
/// so lines that aren't UTF-8 are decoded with that code page
#[cfg(windows)]
fn decoded(bytes: &[u8]) -> String {
    // SAFETY: Only reads a system setting
    let page = unsafe { GetACP() };
    let label = match page {
        932 => "shift_jis".to_owned(),
        936 => "gbk".to_owned(),
        949 => "euc-kr".to_owned(),
        950 => "big5".to_owned(),
        866 => "ibm866".to_owned(),
        874 | 1250..=1258 => format!("windows-{page}"),
        _ => "utf-8".to_owned(),
    };
    let encoding = Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);

    let mut text = String::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        match str::from_utf8(line) {
            Ok(line) => text.push_str(line),
            // A character split at the end of a read is still UTF-8 waiting for its last bytes
            Err(error) if error.error_len().is_none() => {
                let line = String::from_utf8_lossy(line);
                text.push_str(&line);
            }
            Err(_) => {
                let (line, _) = encoding.decode_without_bom_handling(line);
                text.push_str(&line);
            }
        }
    }
    text.replace("\r\n", "\n")
}

pub(crate) fn new_id() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH);
    let stamp = now.map_or(0, |elapsed| elapsed.as_millis());
    // Each `RandomState` has its own seed, so an empty hash is a random number
    let random = RandomState::new().build_hasher().finish() as u32;

    format!("{stamp:x}-{random:08x}")
}

pub(crate) fn excerpt(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let joined = words.join(" ");
    if joined.chars().count() <= MAX_EXCERPT_CHARS {
        return joined;
    }

    let mut excerpt: String = joined.chars().take(MAX_EXCERPT_CHARS).collect();
    excerpt.push('…');
    excerpt
}

pub(crate) fn http_cause(error: &reqwest::Error) -> String {
    let mut root: &dyn Error = error;
    while let Some(source) = root.source() {
        root = source;
    }
    let reason = match error.is_timeout() {
        true => "timed out".to_owned(),
        false => cause(root),
    };

    let Some(url) = error.url().filter(|_| error.is_connect()) else {
        return reason;
    };
    match (url.host_str(), url.port_or_known_default()) {
        (Some(host), Some(port)) => {
            let address = format!("{host}:{port}");
            prompt::no_connection(&address, &reason)
        }
        _ => reason,
    }
}

pub(crate) fn http_client(builder: ClientBuilder) -> Result<Client, String> {
    // An idle connection may have been closed by the peer or a proxy,
    // and reusing it fails with connection closed
    let builder = builder
        .connect_timeout(CONNECT_WAIT)
        .pool_max_idle_per_host(0);
    match builder.build() {
        Ok(client) => Ok(client),
        Err(e) => {
            let reason = http_cause(&e);
            Err(format!("http client: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::MAX_OBSERVATION_CHARS;
    use crate::utils::{excerpt, truncated, truncated_with, Clip};

    #[test]
    fn clip_matches_truncated() {
        let sizes = [
            0,
            10,
            MAX_OBSERVATION_CHARS,
            MAX_OBSERVATION_CHARS + 1,
            MAX_OBSERVATION_CHARS * 3 + 7,
        ];
        for size in sizes {
            let text: String = "café 🐍\n".chars().cycle().take(size).collect();
            let mut clip = Clip::new(MAX_OBSERVATION_CHARS * 2);
            // Chunk boundaries fall inside characters
            for chunk in text.as_bytes().chunks(4097) {
                clip.push(chunk);
            }
            let (kept, cut) = clip.text();
            let clipped = truncated_with(&kept, MAX_OBSERVATION_CHARS, cut);
            let expected = truncated(&text, MAX_OBSERVATION_CHARS);
            assert_eq!(clipped, expected, "{size}");
        }
    }

    /// A character split across two chunks is completed by the next one
    #[test]
    fn clip_shown_fresh() {
        let bytes = "é🍕".as_bytes();
        let mut clip = Clip::new(4);
        clip.push(&bytes[..4]);
        assert_eq!(clip.fresh(100).as_deref(), Some("é\u{fffd}"));
        assert_eq!(clip.fresh(100), None);

        clip.push(&bytes[4..]);
        assert_eq!(clip.fresh(100).as_deref(), Some("é🍕"));
    }

    #[test]
    fn clip_head_whole() {
        // The head limit of 3 bytes falls inside ï
        let mut clip = Clip::new(3);
        clip.push("naïve café".as_bytes());
        assert_eq!(clip.text(), ("naïfé".to_owned(), 5));
    }

    #[test]
    fn excerpt_cut() {
        assert_eq!(excerpt(" upstream\n  overloaded "), "upstream overloaded");

        let whole = "café".repeat(75);
        assert_eq!(excerpt(&whole), whole);
        let long = format!("{whole}s");
        assert_eq!(excerpt(&long), format!("{whole}…"));
    }
}
