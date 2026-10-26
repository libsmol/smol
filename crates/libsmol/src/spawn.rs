use crate::memory::{Exit, ShellStep, MAX_OBSERVATION_CHARS};
use crate::utils::{Clip, Interrupt};
use crate::worker::END;
use reaper::{cause, Mark};
use std::env;
use std::io::{self, Read};
use std::mem;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::select;
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::watch;
use tokio::task;
use tokio::time::{self, Instant};

/// Must hold `MAX_OBSERVATION_CHARS` characters at up to 4 bytes each
pub(crate) const KEPT_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
/// Unread pieces past this stop the relay, so a fast program waits on the full pipe
/// instead of piling output up in memory
const MAX_QUEUED_PIECES: usize = 4;
/// Gives up when a program left in the background keeps the pipe open
pub(crate) const DRAIN_WAIT: Duration = Duration::from_secs(2);
pub(crate) const SHOWN_EVERY: Duration = Duration::from_millis(100);

/// Dropping it kills the whole group
pub(crate) struct Group {
    group: Arc<reaper::Group>,
    /// The outer `Option` says whether the child was reaped
    /// and the inner one is `None` when reaping failed
    ended: watch::Receiver<Option<Option<Exit>>>,
}

impl Group {
    /// `child` must be freshly spawned and never waited on
    pub(crate) fn new(mut child: Child, mark: Mark) -> io::Result<Self> {
        let pid = child.id().expect("pid");
        let group = match reaper::Group::new(pid, mark) {
            Ok(group) => Arc::new(group),
            Err(e) => {
                // On Windows it is still suspended
                child.start_kill().ok();
                return Err(e);
            }
        };

        let (reaped, ended) = watch::channel(None);
        // A dropped `Group` couldn't kill the group if the reaping task held a strong reference
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

    /// Replaces the command's stdout and stderr with one pipe
    pub(crate) fn start(mut command: Command) -> io::Result<(Self, Receiver<Piece>)> {
        // Reading stdout and stderr separately would scramble the order of interleaved writes
        let (mut output, writer) = io::pipe()?;
        let copied = writer.try_clone()?;
        command.stdout(copied).stderr(writer);
        let std = command.as_std_mut();
        let mark = reaper::prepare(std);
        let child = command.spawn()?;
        // Holding the write end here would keep the pipe open after the program exits
        drop(command);
        let group = Self::new(child, mark)?;

        // Waits while the receiver falls behind, but once the receiver is gone it keeps reading
        // until the pipe closes, because a full pipe blocks the program
        let (pieces, received) = mpsc::channel(MAX_QUEUED_PIECES);
        thread::spawn(move || {
            let mut buffer = vec![0; CHUNK_BYTES];
            let mut held = Vec::new();
            while let Ok(count @ 1..) = output.read(&mut buffer) {
                held.extend_from_slice(&buffer[..count]);
                while let Some(at) = held.windows(END.len()).position(|window| window == END) {
                    let rest = held.split_off(at + END.len());
                    held.truncate(at);
                    let before = mem::replace(&mut held, rest);
                    pieces.blocking_send(Piece::Output(before)).ok();
                    pieces.blocking_send(Piece::End).ok();
                }

                // The marker may span two reads, so a tail that starts it waits for the next one
                let partial = (1..END.len())
                    .rev()
                    .find(|size| held.ends_with(&END[..*size]));
                let at = held.len() - partial.unwrap_or(0);
                let rest = held.split_off(at);
                let before = mem::replace(&mut held, rest);
                pieces.blocking_send(Piece::Output(before)).ok();
            }
            pieces.blocking_send(Piece::Output(held)).ok();
        });

        Ok((group, received))
    }

    pub(crate) async fn exited(&mut self) -> Option<Exit> {
        let ended = self.ended.wait_for(Option::is_some).await;
        ended.ok().and_then(|end| end.clone()).flatten()
    }

    /// The spawned program has been reaped
    pub(crate) fn ended(&self) -> bool {
        self.ended.borrow().is_some()
    }

    pub(crate) fn kill(&self) {
        self.group.kill();
    }

    /// Includes programs it left in the background
    pub(crate) fn alive(&self) -> bool {
        self.group.alive()
    }
}

pub(crate) enum Piece {
    Output(Vec<u8>),
    /// The END marker the worker writes after each snippet
    End,
}

/// Collects what is left in the pipe without waiting forever on background programs holding it open
pub(crate) async fn drain(pieces: &mut Receiver<Piece>, output: &mut Clip) {
    let draining = async {
        while let Some(piece) = pieces.recv().await {
            if let Piece::Output(bytes) = piece {
                output.push(&bytes);
            }
        }
    };
    time::timeout(DRAIN_WAIT, draining).await.ok();
}

/// Runs a command the user typed after !, giving `shown` clipped output now and then
/// and handing back any programs left in the background
pub(crate) async fn spawn_shell(
    line: &str,
    cwd: &Path,
    interrupt: &Interrupt,
    shown: &mut (dyn FnMut(&str) + Send),
) -> (ShellStep, Option<Group>) {
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

    let mut process = Command::new(&program);
    #[cfg(unix)]
    process.arg("-c").arg(line);
    // The whole line goes to cmd for quoting, and the UTF-8 code page applies only in a nested cmd
    #[cfg(windows)]
    let arg = format!("/d /s /c \"chcp 65001>nul & cmd /d /s /c \"{line}\"\"");
    #[cfg(windows)]
    process.as_std_mut().raw_arg(arg);
    // Stdin is empty so a program waiting for input reads EOF and moves on
    process.current_dir(cwd).stdin(Stdio::null());
    // Otherwise a spawned CPython uses GBK on the pipes and fails on characters outside it
    #[cfg(windows)]
    process.env("PYTHONUTF8", "1");
    let (mut group, mut pieces) = match Group::start(process) {
        Ok(started) => started,
        Err(e) => {
            step.end = Exit::Unstarted(cause(&e));
            return (step, None);
        }
    };

    let mut kept = Clip::new(KEPT_BYTES);
    let mut show_at = Instant::now() + SHOWN_EVERY;
    let end = loop {
        select! {
            biased;
            () = interrupt.wait() => {
                group.kill();
                break Exit::Interrupted;
            }
            // There is no exit code when reaping fails
            end = group.exited() => break end.unwrap_or(Exit::Exited(-1)),
            () = time::sleep_until(show_at) => {
                show_at = Instant::now() + SHOWN_EVERY;
                if let Some(text) = kept.fresh(MAX_OBSERVATION_CHARS) {
                    shown(&text);
                }
            }
            Some(piece) = pieces.recv() => {
                if let Piece::Output(bytes) = piece {
                    kept.push(&bytes);
                }
            }
        }
    };

    drain(&mut pieces, &mut kept).await;
    step.output = kept.shown(MAX_OBSERVATION_CHARS);
    step.end = end;

    // An interrupt has already killed the whole group
    let left = step.end != Exit::Interrupted && group.alive();
    (step, left.then_some(group))
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
    use crate::memory::{Exit, ShellStep};
    use crate::spawn::spawn_shell;
    use crate::tests::temp_dir;
    use crate::utils::{block_on, Interrupt};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The second item says whether programs were left in the background
    fn shelled(dir: &Path, line: &str, interrupt: &Interrupt) -> (ShellStep, bool) {
        let mut ignored = |_: &str| {};
        let running = spawn_shell(line, dir, interrupt, &mut ignored);
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
        let running = spawn_shell(line, &cwd, &interrupt, &mut shown);
        let (ran, left) = block_on(running);
        assert_eq!(ran.end, Exit::Interrupted);
        assert_eq!(ran.output.trim(), "first\nlast");
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(left.is_none());
    }
}
