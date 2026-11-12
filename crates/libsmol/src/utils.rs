use crate::prompt;
use reaper::cause;
use reqwest::{Client, ClientBuilder};
use serde_json::Value;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
#[cfg(test)]
use std::future;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};
#[cfg(test)]
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{oneshot, watch};

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

pub fn block_on<F: Future>(future: F) -> F::Output {
    #[cfg(test)]
    LazyLock::force(&DRIVER);
    RUNTIME.block_on(future)
}

pub type Reply = Result<Value, String>;

#[derive(Default)]
pub struct Waiting {
    next: AtomicU64,
    waiters: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
}

impl Waiting {
    pub fn ask(&self) -> (u64, oneshot::Receiver<Reply>) {
        let (sender, receiver) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut waiters = self.waiters.lock().expect("waiting lock");
        waiters.insert(id, sender);
        (id, receiver)
    }

    pub fn take(&self, id: u64) -> Option<oneshot::Sender<Reply>> {
        let mut waiters = self.waiters.lock().expect("waiting lock");
        waiters.remove(&id)
    }

    pub fn answer(&self, id: u64, reply: Reply) {
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
    middle_cut(text, max, "\n")
}

/// Like `truncated`, for text that must stay on one line
pub(crate) fn one_lined(text: &str, max: usize) -> String {
    middle_cut(text, max, " ")
}

/// Keeps `max` characters of `text` with the marker of the cut set off by `apart`
fn middle_cut(text: &str, max: usize, apart: &str) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }

    let kept = max / 2;
    let head: String = text.chars().take(max - kept).collect();
    let tail: String = text.chars().skip(count - kept).collect();
    let marker = prompt::cut(count - max);

    format!("{head}{apart}{marker}{apart}{tail}")
}

/// `head` and `tail` with `cut` characters already dropped between them, kept to `max` characters
/// and marked once where text is missing
pub(crate) fn seamed(head: &str, tail: &str, cut: usize, max: usize) -> String {
    if cut == 0 {
        let text = format!("{head}{tail}");
        return truncated(&text, max);
    }

    // Each side keeps up to half, and a side shorter than that leaves the rest to the other
    let heads = head.chars().count();
    let tails = tail.chars().count();
    let max = max.min(heads + tails);
    let half = max / 2;
    let tail_kept = match heads < max - half {
        true => tails.min(max - heads),
        false => tails.min(half),
    };
    let head_kept = heads.min(max - tail_kept);

    let head: String = head.chars().take(head_kept).collect();
    let tail: String = tail.chars().skip(tails - tail_kept).collect();
    let dropped = heads - head_kept + cut + tails - tail_kept;
    let marker = prompt::cut(dropped);

    format!("{head}\n{marker}\n{tail}")
}

pub(crate) fn new_id() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH);
    let stamp = now.map_or(0, |elapsed| elapsed.as_millis());
    // Each `RandomState` has its own seed, so an empty hash is a random number
    let random = RandomState::new().build_hasher().finish() as u32;

    format!("{stamp:x}-{random:08x}")
}

/// One spelling of a directory, without `.` parts or a trailing separator,
/// and lowercase on Windows since a typed one may differ in case from the saved one
pub(crate) fn folded(path: &Path) -> OsString {
    let path: PathBuf = path.components().collect();
    match cfg!(windows) {
        true => {
            let text = path.to_string_lossy();
            OsString::from(text.to_lowercase())
        }
        false => path.into_os_string(),
    }
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

/// Names `wait` only for reqwest's own read timeout, in clients without an overall timeout,
/// and not for one the system reports, such as a keepalive's
pub(crate) fn read_cause(error: &reqwest::Error, wait: Duration) -> String {
    let mut reported = false;
    let mut source = error.source();
    while let Some(inner) = source {
        let io = inner.downcast_ref::<io::Error>();
        reported |= io.is_some_and(|io| io.kind() == io::ErrorKind::TimedOut);
        source = inner.source();
    }
    if !error.is_timeout() || error.is_connect() || reported {
        return http_cause(error);
    }

    let seconds = wait.as_secs();
    prompt::timed_out(seconds)
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
    use crate::utils::{block_on, excerpt, one_lined, read_cause};
    use futures_util::stream;
    use http::Response;
    use reqwest::Body;
    use std::io;
    use std::time::Duration;

    #[test]
    fn middle_one_lined() {
        let command = format!("python3 -c {} --café", "x = 1; ".repeat(10));
        let cut = one_lined(&command, 20);
        assert_eq!(cut, "python3 -c [68 characters cut] 1;  --café");
        assert_eq!(one_lined("brew 🐍", 20), "brew 🐍");
    }

    #[test]
    fn excerpt_cut() {
        assert_eq!(excerpt(" upstream\n  overloaded "), "upstream overloaded");

        let whole = "café".repeat(75);
        assert_eq!(excerpt(&whole), whole);
        let long = format!("{whole}s");
        assert_eq!(excerpt(&long), format!("{whole}…"));
    }

    #[test]
    fn system_timeout_cause() {
        let failed = io::Error::from(io::ErrorKind::TimedOut);
        let chunks = stream::iter([Err::<Vec<u8>, io::Error>(failed)]);
        let body = Body::wrap_stream(chunks);
        let mut response: reqwest::Response = Response::new(body).into();
        let reading = response.chunk();
        let error = block_on(reading).expect_err("timed out");

        assert!(error.is_timeout());
        let wait = Duration::from_secs(5);
        assert_eq!(read_cause(&error, wait), "timed out");
    }
}
