use crate::steps::{Exit, ShellStep, MAX_OBSERVATION_CHARS};
use crate::utils::{seamed, Interrupt};
#[cfg(windows)]
use encoding_rs::Encoding;
use reaper::{cause, terminal_view, Group, Mark, Member};
use std::collections::VecDeque;
use std::env;
use std::io::{self, Read};
use std::mem;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
#[cfg(windows)]
use std::str;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::select;
use tokio::sync::{watch, Notify};
use tokio::task;
use tokio::time::{self, Instant};
#[cfg(windows)]
use windows_sys::Win32::Globalization::GetACP;

/// Must hold `MAX_OBSERVATION_CHARS` characters at up to 4 bytes each
const KEPT_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
/// Gives up when a program left in the background keeps the pipe open
pub(crate) const DRAIN_WAIT: Duration = Duration::from_secs(2);
pub(crate) const SHOWN_EVERY: Duration = Duration::from_millis(100);

/// Dropping it kills the whole group
pub(crate) struct Process {
    group: Arc<Group>,
    /// The outer `Option` says whether the child was reaped
    /// and the inner one is `None` when reaping failed
    ended: watch::Receiver<Option<Option<Exit>>>,
}

/// What a program wrote, read from its pipe as it comes so the program never waits on a full pipe,
/// split at each end marker and clipped
pub(crate) struct Output {
    shared: Arc<Shared>,
}

struct Shared {
    parts: Mutex<Parts>,
    /// Wakes one waiter, so only one task may wait on the output at a time
    changed: Notify,
}

struct Parts {
    /// What arrived since the last end marker
    open: Clip,
    /// What came before the last end marker, until it is taken
    closed: Option<Clip>,
    /// The pipe closed and nothing more comes
    finished: bool,
}

impl Process {
    /// `child` must be freshly spawned and never waited on
    pub(crate) fn new(mut child: Child, mark: Mark) -> io::Result<Self> {
        let pid = child.id().expect("pid");
        let group = match Group::new(pid, mark) {
            Ok(group) => Arc::new(group),
            Err(e) => {
                // On Windows it is still suspended
                child.start_kill().ok();
                return Err(e);
            }
        };

        let (reaped, ended) = watch::channel(None);
        // A dropped `Process` couldn't kill the group if the reaping task held a strong reference
        let held = Arc::downgrade(&group);
        let waiting = async move {
            let status = child.wait().await;
            // Once reaped, an empty group's id may be reused, so it must not be killed by id
            if let Some(group) = held.upgrade() {
                if !group.alive() {
                    group.release();
                }
            }
            let end = status.ok().map(end_of);
            reaped.send(Some(end)).ok();
        };
        task::spawn(waiting);

        Ok(Self { group, ended })
    }

    /// Replaces the command's stdout and stderr with one pipe, whose output ends a part at each `end`
    pub(crate) fn start(mut command: Command, end: Option<Vec<u8>>) -> io::Result<(Self, Output)> {
        // Reading stdout and stderr separately would scramble the order of interleaved writes
        let (mut reader, writer) = io::pipe()?;
        let copied = writer.try_clone()?;
        command.stdout(copied).stderr(writer);
        let std = command.as_std_mut();
        let mark = reaper::prepare(std);
        let child = command.spawn()?;
        // Holding the write end here would keep the pipe open after the program exits
        drop(command);
        let process = Self::new(child, mark)?;

        let parts = Parts {
            open: Clip::new(KEPT_BYTES),
            closed: None,
            finished: false,
        };
        let shared = Arc::new(Shared {
            parts: Mutex::new(parts),
            changed: Notify::new(),
        });
        let output = Output {
            shared: Arc::clone(&shared),
        };

        // Reads to the end even once the output is dropped, as a full pipe blocks the program
        thread::spawn(move || {
            let mut buffer = vec![0; CHUNK_BYTES];
            let mut held = Vec::new();
            while let Ok(count @ 1..) = reader.read(&mut buffer) {
                let read = &buffer[..count];
                let mut parts = shared.lock();
                let Some(end) = &end else {
                    parts.open.push(read);
                    continue;
                };
                held.extend_from_slice(read);

                // Only code that writes the marker itself replaces a part not yet taken
                while let Some(at) = held.windows(end.len()).position(|window| window == end) {
                    parts.open.push(&held[..at]);
                    held.drain(..at + end.len());
                    let before = mem::replace(&mut parts.open, Clip::new(KEPT_BYTES));
                    parts.closed = Some(before);
                    shared.changed.notify_one();
                }

                // The marker may span two reads, so a tail that starts it waits for the next one
                let partial = (1..end.len())
                    .rev()
                    .find(|size| held.ends_with(&end[..*size]));
                let at = held.len() - partial.unwrap_or(0);
                parts.open.push(&held[..at]);
                held.drain(..at);
            }

            let mut parts = shared.lock();
            parts.open.push(&held);
            parts.finished = true;
            shared.changed.notify_one();
        });

        Ok((process, output))
    }

    pub(crate) async fn wait(&mut self) -> Option<Exit> {
        let ended = self.ended.wait_for(Option::is_some).await;
        ended.ok().and_then(|end| end.clone()).flatten()
    }

    pub(crate) fn reaped(&self) -> bool {
        self.ended.borrow().is_some()
    }

    pub(crate) fn kill(&self) {
        self.group.kill();
    }

    /// Includes programs it left in the background
    pub(crate) fn alive(&self) -> bool {
        self.group.alive()
    }

    /// The programs in its group besides itself, listed off the async runtime, or none when that fails
    pub(crate) async fn members(&self) -> Option<Vec<Member>> {
        // A dropped `Process` couldn't kill the group while the listing held a strong reference
        let held = Arc::downgrade(&self.group);
        let listing = move || held.upgrade().map(|group| group.members());
        let listed = task::spawn_blocking(listing).await;
        listed.ok().flatten()?.ok()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Parts> {
        self.parts.lock().expect("output lock")
    }
}

impl Output {
    /// What came before the next end marker, or None once the pipe closed.
    /// Dropping the future loses nothing
    pub(crate) async fn part(&self) -> Option<Clip> {
        loop {
            let (closed, finished) = {
                let mut parts = self.shared.lock();
                (parts.closed.take(), parts.finished)
            };
            if closed.is_some() || finished {
                return closed;
            }
            self.shared.changed.notified().await;
        }
    }

    /// The clipped text since the last end marker, only when more arrived since the last call
    pub(crate) fn fresh(&self, max: usize) -> Option<String> {
        let mut parts = self.shared.lock();
        parts.open.fresh(max)
    }

    /// Everything since the last end marker
    pub(crate) fn take(&self) -> Clip {
        let mut parts = self.shared.lock();
        mem::replace(&mut parts.open, Clip::new(KEPT_BYTES))
    }

    /// A part not yet taken followed by everything since the last end marker,
    /// once the pipe closes or background programs holding it open have had `DRAIN_WAIT`
    pub(crate) async fn rest(&self) -> Clip {
        let finishing = async {
            loop {
                let finished = self.shared.lock().finished;
                if finished {
                    return;
                }
                self.shared.changed.notified().await;
            }
        };
        time::timeout(DRAIN_WAIT, finishing).await.ok();

        let (closed, open) = {
            let mut parts = self.shared.lock();
            let open = mem::replace(&mut parts.open, Clip::new(KEPT_BYTES));
            (parts.closed.take(), open)
        };
        match closed {
            Some(closed) => closed.joined(&open),
            None => open,
        }
    }
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
    fn new(half: usize) -> Self {
        Self {
            half,
            head: Vec::new(),
            tail: VecDeque::new(),
            dropped: 0,
            cut: 0,
            reported: 0,
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
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

    /// Both kept texts in order, with what either dropped counted at the one seam
    fn joined(&self, next: &Clip) -> Clip {
        let mut joined = Clip::new(self.half);
        for clip in [self, next] {
            joined.push(&clip.head);
            let tail: Vec<u8> = clip.tail.iter().copied().collect();
            joined.push(&tail);
            joined.cut += clip.cut;
            joined.dropped += clip.dropped;
        }
        joined
    }

    /// The kept text as the program wrote it, and the number of characters dropped from the middle
    fn text(&self) -> (String, usize) {
        let mut bytes = self.head.clone();
        bytes.extend(&self.tail);
        let text = decoded(&bytes);
        (text, self.cut)
    }

    /// The kept text without terminal sequences and redrawn frames, and where its tail starts,
    /// with head and tail cleaned apart once the middle is dropped as a sequence may span the seam
    pub(crate) fn cleaned(&self) -> (String, usize) {
        if self.cut == 0 {
            let (text, _) = self.text();
            let text = terminal_view(&text);
            let seam = text.len();
            return (text, seam);
        }

        let head = decoded(&self.head);
        let head = terminal_view(&head);
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = decoded(&tail);
        let tail = terminal_view(&tail);
        let seam = head.len();
        (head + &tail, seam)
    }

    pub(crate) fn cut(&self) -> usize {
        self.cut
    }

    pub(crate) fn shown(&self, max: usize) -> String {
        let (text, seam) = self.cleaned();
        let (head, tail) = text.split_at(seam);
        seamed(head, tail, self.cut, max)
    }

    /// Returns text only when more output arrived since the last call
    fn fresh(&mut self, max: usize) -> Option<String> {
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

/// Runs a command the user typed after !, giving `shown` clipped output now and then
/// and handing back any programs left in the background
pub(crate) async fn run_shell(
    line: &str,
    cwd: &Path,
    interrupt: &Interrupt,
    shown: &mut (dyn FnMut(&str) + Send),
) -> (ShellStep, Option<Process>) {
    #[cfg(unix)]
    let program = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    #[cfg(windows)]
    let program = env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let name = Path::new(&program).file_stem().unwrap_or_default();
    let mut step = ShellStep {
        command: line.to_owned(),
        shell: name.to_string_lossy().into_owned(),
        output: String::new(),
        end: Exit::Interrupted,
    };

    let mut command = Command::new(&program);
    #[cfg(unix)]
    command.arg("-c").arg(line);
    // The whole line goes to cmd for quoting, and the UTF-8 code page applies only in a nested cmd
    #[cfg(windows)]
    let arg = format!("/d /s /c \"chcp 65001>nul & cmd /d /s /c \"{line}\"\"");
    #[cfg(windows)]
    command.as_std_mut().raw_arg(arg);
    // Stdin is empty so a program waiting for input reads EOF and moves on
    command.current_dir(cwd).stdin(Stdio::null());
    // Otherwise a spawned CPython uses GBK on the pipes and fails on characters outside it
    #[cfg(windows)]
    command.env("PYTHONUTF8", "1");
    let (mut process, output) = match Process::start(command, None) {
        Ok(started) => started,
        Err(e) => {
            step.end = Exit::Unstarted(cause(&e));
            return (step, None);
        }
    };

    let mut show_at = Instant::now() + SHOWN_EVERY;
    let end = loop {
        select! {
            biased;
            () = interrupt.wait() => {
                process.kill();
                break Exit::Interrupted;
            }
            // There is no exit code when reaping fails
            end = process.wait() => break end.unwrap_or(Exit::Exited(-1)),
            () = time::sleep_until(show_at) => {
                show_at = Instant::now() + SHOWN_EVERY;
                if let Some(text) = output.fresh(MAX_OBSERVATION_CHARS) {
                    shown(&text);
                }
            }
        }
    };

    let said = output.rest().await;
    step.output = said.shown(MAX_OBSERVATION_CHARS);
    step.end = end;

    // An interrupt has already killed the whole group
    let left = step.end != Exit::Interrupted && process.alive();
    (step, left.then_some(process))
}

/// Crash exit codes on Windows are negative as i32, so a negative code can't stand for a signal
fn end_of(status: ExitStatus) -> Exit {
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return Exit::Signaled(signal);
    }
    let code = status.code().unwrap_or(-1);
    Exit::Exited(code)
}

#[cfg(test)]
mod tests {
    use crate::spawn::{run_shell, Clip, Process};
    use crate::steps::{Exit, ShellStep, MAX_OBSERVATION_CHARS};
    use crate::tests::{interpreter, temp_dir};
    use crate::utils::{block_on, truncated, Interrupt};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tokio::process::Command;

    #[test]
    fn clip_matches_truncated() {
        let sizes = [
            0,
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
            let clipped = clip.shown(MAX_OBSERVATION_CHARS);
            let expected = truncated(&text, MAX_OBSERVATION_CHARS);
            assert_eq!(clipped, expected, "{size}");
        }
    }

    /// Progress redrawn with \r shrinks when cleaned, and the marker stays where the middle was dropped
    #[test]
    fn redraws_seam_marked() {
        let mut written = "head line one, two!\n".to_owned();
        for frame in 1..=50 {
            written.push_str(&format!("\rframe {frame:02}"));
        }
        written.push_str("\rlast\n");
        let mut clip = Clip::new(20);
        clip.push(written.as_bytes());

        let expected = "head line one, two!\n\n[436 characters cut]\nlast\n";
        assert_eq!(clip.shown(1000), expected);
        assert_eq!(clip.shown(10), "head \n[451 characters cut]\nlast\n");
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

    /// A part not yet taken comes with what followed it, such as a crash after the startup marker
    #[test]
    fn rest_joined() {
        let python = interpreter();
        let mut command = Command::new(python);
        let code =
            "import sys\nsys.stdout.buffer.write('started café\\n<end>crashed naïve\\n'.encode())";
        command.arg("-c").arg(code);
        let ending = b"<end>".to_vec();
        let reading = async {
            let (_process, output) = Process::start(command, Some(ending)).expect("start");
            output.rest().await
        };
        let said = block_on(reading);
        assert_eq!(said.shown(1000), "started café\ncrashed naïve\n");

        let mut first = Clip::new(4);
        first.push("café crème".as_bytes());
        let mut second = Clip::new(4);
        second.push("naïve".as_bytes());
        let joined = first.joined(&second);
        assert_eq!(joined.shown(100), "café\n[8 characters cut]\nïve");
    }

    /// The second item says whether programs were left in the background
    fn shelled(dir: &Path, line: &str, interrupt: &Interrupt) -> (ShellStep, bool) {
        let mut ignored = |_: &str| {};
        let running = run_shell(line, dir, interrupt, &mut ignored);
        let (ran, left) = block_on(running);
        (ran, left.is_some())
    }

    #[cfg(unix)]
    #[test]
    fn shell_typed() {
        let dir = temp_dir("shell-café");
        fs::create_dir_all(&dir).expect("dir");
        let interrupt = Interrupt::default();
        let line = "echo café; echo naïve >&2; echo résumé; pwd; exit 3";
        let (ran, _) = shelled(&dir, line, &interrupt);

        assert_eq!(ran.end, Exit::Exited(3));
        // `pwd` prints the physical path, and the macOS temp directory sits behind a symlink
        let real = fs::canonicalize(&dir).expect("canonical");
        let expected = format!("café\nnaïve\nrésumé\n{}\n", real.display());
        assert_eq!(ran.output, expected);
        assert!(!ran.shell.is_empty());

        let (read, _) = shelled(&dir, "cat", &interrupt);
        assert_eq!((read.end, read.output), (Exit::Exited(0), String::new()));
        let (left, kept) = shelled(&dir, "sleep 30 &", &interrupt);
        assert_eq!(left.end, Exit::Exited(0));
        assert!(kept);
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn shell_typed() {
        let dir = temp_dir("shell-café");
        fs::create_dir_all(&dir).expect("dir");
        let interrupt = Interrupt::default();
        let line = "echo café& (echo naïve)1>&2& echo résumé& cd& exit /b 3";
        let (ran, _) = shelled(&dir, line, &interrupt);

        assert_eq!(ran.end, Exit::Exited(3));
        let expected = format!("café\nnaïve\nrésumé\n{}\n", dir.display());
        assert_eq!(ran.output, expected);
        assert_eq!(ran.shell.to_lowercase(), "cmd");
        fs::remove_dir_all(&dir).ok();
    }

    /// Output written before the interrupt is kept too
    #[test]
    fn shell_interrupted() {
        let interrupt = Interrupt::default();
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let line = if cfg!(windows) {
            "echo first& echo last& ping -n 30 127.0.0.1 >nul"
        } else {
            "echo first; sleep 0.2; echo last; sleep 30"
        };
        // A timer would race the shell on a busy machine
        let mut shown = |output: &str| {
            if output.contains("last") {
                interrupt.cancel(0);
            }
        };
        let started = Instant::now();
        let running = run_shell(line, &cwd, &interrupt, &mut shown);
        let (ran, left) = block_on(running);

        assert_eq!(ran.end, Exit::Interrupted);
        assert_eq!(ran.output.trim(), "first\nlast");
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(left.is_none());
    }
}
