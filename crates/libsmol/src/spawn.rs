use crate::memory::{Exit, ShellStep, MAX_OBSERVATION_CHARS};
use crate::utils::{Clip, Interrupt, KEPT_BYTES, SHOWN_EVERY};
use reaper::{cause, Mark};
use std::env;
use std::io::{self, PipeReader, Read};
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

const CHUNK_BYTES: usize = 64 * 1024;
/// Unread pieces past this stop the relay, so a fast program waits on the full pipe
/// instead of piling output up in memory
const MAX_QUEUED_PIECES: usize = 4;
/// Gives up when a program left in the background keeps the pipe open
const DRAIN_WAIT: Duration = Duration::from_secs(2);

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

    pub(crate) async fn exited(&mut self) -> Option<Exit> {
        let ended = self.ended.wait_for(Option::is_some).await;
        ended.ok().and_then(|end| end.clone()).flatten()
    }

    pub(crate) fn kill(&self) {
        self.group.kill();
    }

    /// Includes programs it left in the background
    pub(crate) fn alive(&self) -> bool {
        self.group.alive()
    }
}

/// Waits while the receiver falls behind, but once the receiver is gone it keeps reading
/// until the pipe closes, because a full pipe blocks the program
fn relay(mut output: PipeReader) -> Receiver<Vec<u8>> {
    let (pieces, received) = mpsc::channel(MAX_QUEUED_PIECES);
    thread::spawn(move || {
        let mut buffer = vec![0; CHUNK_BYTES];
        while let Ok(count @ 1..) = output.read(&mut buffer) {
            let piece = buffer[..count].to_vec();
            pieces.blocking_send(piece).ok();
        }
    });
    received
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
    // Reading stdout and stderr separately would scramble the order of interleaved writes
    let pipe = io::pipe().and_then(|(output, writer)| Ok((output, writer.try_clone()?, writer)));
    let (output, copied, writer) = match pipe {
        Ok(pipe) => pipe,
        Err(e) => {
            step.end = Exit::Unstarted(cause(&e));
            return (step, None);
        }
    };
    // Stdin is empty so a program waiting for input reads EOF and moves on
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(copied)
        .stderr(writer);
    // Otherwise a spawned CPython uses GBK on the pipes and fails on characters outside it
    #[cfg(windows)]
    process.env("PYTHONUTF8", "1");
    let std = process.as_std_mut();
    let mark = reaper::prepare(std);
    let spawned = process.spawn();
    // Holding the write end here would keep the pipe open after the program exits
    drop(process);
    let started = spawned.and_then(|child| Group::new(child, mark));
    let mut group = match started {
        Ok(group) => group,
        Err(e) => {
            step.end = Exit::Unstarted(cause(&e));
            return (step, None);
        }
    };
    let mut pieces = relay(output);

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
            Some(piece) = pieces.recv() => kept.push(&piece),
        }
    };

    let draining = async {
        while let Some(piece) = pieces.recv().await {
            kept.push(&piece);
        }
    };
    time::timeout(DRAIN_WAIT, draining).await.ok();
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
