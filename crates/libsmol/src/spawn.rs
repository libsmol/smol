use crate::prompt;
use crate::steps::{Exit, ShellStep, MAX_OBSERVATION_CHARS};
use crate::utils::{truncated, Interrupt};
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

/// Kept at each end of a clip, which must hold `MAX_OBSERVATION_CHARS` characters at up to 4 bytes
/// each
const MAX_HALF_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
/// Gives up when a program left in the background keeps the pipe open
const DRAIN_WAIT: Duration = Duration::from_secs(2);
/// A program that is going away gets this long to exit and say how
const EXIT_WAIT: Duration = Duration::from_secs(2);
pub(crate) const SHOWN_EVERY: Duration = Duration::from_millis(100);

/// Dropping it kills the whole group
pub(crate) struct Process {
    group: Arc<Group>,
    ended: watch::Receiver<Reaped>,
}

/// How the child ended once reaped, unknown when reaping failed
#[derive(Clone)]
enum Reaped {
    Running,
    Ended(Option<Exit>),
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

        let (reaped, ended) = watch::channel(Reaped::Running);
        // A dropped `Process` couldn't kill the group if the reaping task held a strong reference
        let held = Arc::downgrade(&group);
        let waiting = async move {
            let status = child.wait().await;
            if let Some(group) = held.upgrade() {
                release_empty(&group);
            }
            let end = status.ok().map(end_of);
            reaped.send(Reaped::Ended(end)).ok();
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
            open: Clip::new(MAX_HALF_BYTES),
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
                    let before = mem::replace(&mut parts.open, Clip::new(MAX_HALF_BYTES));
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
        let reaped = |reaped: &Reaped| matches!(reaped, Reaped::Ended(_));
        let ended = self.ended.wait_for(reaped).await;
        match ended.as_deref() {
            Ok(Reaped::Ended(end)) => end.clone(),
            _ => None,
        }
    }

    pub(crate) fn reaped(&self) -> bool {
        matches!(*self.ended.borrow(), Reaped::Ended(_))
    }

    /// How it ended, once it does or after `EXIT_WAIT`, and all it wrote that `output` still
    /// holds, read after the group is killed
    pub(crate) async fn remains(&mut self, output: &Output) -> (Option<Exit>, Clip) {
        let exiting = self.wait();
        let end = time::timeout(EXIT_WAIT, exiting).await.ok().flatten();
        // Programs it left, like the helpers multiprocessing starts, would keep the pipe open
        self.kill();
        let said = output.rest().await;
        (end, said)
    }

    pub(crate) fn kill(&self) {
        self.group.kill();
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
        mem::replace(&mut parts.open, Clip::new(MAX_HALF_BYTES))
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
            let open = mem::replace(&mut parts.open, Clip::new(MAX_HALF_BYTES));
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

    /// The kept text as the program wrote it
    fn text(&self) -> String {
        let mut bytes = self.head.clone();
        bytes.extend(&self.tail);
        decoded(&bytes)
    }

    /// The kept text without terminal sequences, redrawn frames and the line breaks it ends with,
    /// kept to `max` characters and marked once where text is missing
    pub(crate) fn shown(&self, max: usize) -> String {
        if self.cut == 0 {
            let text = self.text();
            let text = terminal_view(&text);
            let text = text.trim_end_matches('\n');
            return truncated(text, max);
        }

        // Head and tail are cleaned apart, as a sequence may span the seam
        let head = decoded(&self.head);
        let head = terminal_view(&head);
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = decoded(&tail);
        let tail = terminal_view(&tail);
        let seam = head.len();
        let text = head + &tail;
        let text = text.trim_end_matches('\n');

        // Each side keeps up to half, and a side shorter than that leaves the rest to the other
        let (head, tail) = text.split_at(seam.min(text.len()));
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
        let dropped = heads - head_kept + self.cut + tails - tail_kept;
        let marker = prompt::cut(dropped);

        format!("{head}\n{marker}\n{tail}")
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

/// Runs a command the user typed after !, giving `shown` clipped output now and then,
/// and hands back the command unless interrupted, as dropping it ends what it left running
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

    // What the command left, in its group or in a session of its own, ends with the session,
    // while an interrupt has already ended all of it
    release_empty(&process.group);
    let kept = step.end != Exit::Interrupted;
    (step, kept.then_some(process))
}

/// Once its first process is reaped, an empty group's id may be reused and must not be killed by
/// id, while dropping it still ends the processes that left it
fn release_empty(group: &Group) {
    if !group.alive() {
        group.release();
    }
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
    use crate::prompt;
    use crate::spawn::{run_shell, Clip};
    use crate::steps::{Exit, MAX_OBSERVATION_CHARS};
    use crate::utils::{block_on, truncated, Interrupt};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

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

        let marker = prompt::cut(436);
        let expected = format!("head line one, two!\n\n{marker}\nlast");
        assert_eq!(clip.shown(1000), expected);
        let marker = prompt::cut(450);
        assert_eq!(clip.shown(10), format!("head l\n{marker}\nlast"));
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
