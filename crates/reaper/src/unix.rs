use std::env;
#[cfg(target_os = "linux")]
use std::fs;
use std::fs::File;
use std::io::{self, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{self, Command};
#[cfg(target_os = "linux")]
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

/// Processes that started a new session left the group and are found by this inherited variable
const MARK_ENV: &str = "SMOL_REAPER";
/// A process killed a moment ago may already have forked children
#[cfg(target_os = "linux")]
const MAX_SWEEPS: usize = 3;
#[cfg(target_os = "linux")]
const SWEEP_EVERY: Duration = Duration::from_millis(50);

static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static NEXT_MARK: AtomicU64 = AtomicU64::new(1);
static INSTANCE: OnceLock<String> = OnceLock::new();
/// Pipe to the watchdog, closed however this process dies
static GUARD: OnceLock<PipeWriter> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct Mark(u64);

/// Must be called before any thread starts, because the forked watchdog keeps running Rust
pub fn guard() {
    let Ok((mut reader, writer)) = io::pipe() else {
        return;
    };

    // The watchdog sweeps by this prefix, so it has to exist before the fork
    instance();

    // SAFETY: No other thread exists yet
    match unsafe { libc::fork() } {
        0 => {}
        // Without a watchdog only PDEATHSIG is left
        -1 => return,
        _ => {
            GUARD.set(writer).ok();
            return;
        }
    }
    drop(writer);

    // It has to survive a kill of the parent's group or a terminal hangup to clean up
    // SAFETY: Only changes this process's own group, name, signal dispositions and descriptors
    unsafe {
        libc::setpgid(0, 0);
        // With the parent's name it would also die when the parent is killed by name
        #[cfg(target_os = "linux")]
        libc::prctl(libc::PR_SET_NAME, c"smol-reaper".as_ptr());
        for signal in [libc::SIGINT, libc::SIGHUP, libc::SIGTERM, libc::SIGQUIT] {
            libc::signal(signal, libc::SIG_IGN);
        }
        // Holding the parent's pipes would delay EOF for whoever reads them
        if let Ok(null) = File::options().read(true).write(true).open("/dev/null") {
            let null = null.as_raw_fd();
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
        }
    }

    let mut groups = Vec::new();
    let mut record = [0; 4];
    while reader.read_exact(&mut record).is_ok() {
        match i32::from_ne_bytes(record) {
            group if group > 0 => groups.push(group),
            group => groups.retain(|known| *known != -group),
        }
    }

    for group in groups {
        kill(group);
    }
    let prefix = instance();
    kill_marked(prefix);
    // SAFETY: Skips the exit handlers inherited from the parent
    unsafe { libc::_exit(0) }
}

/// PDEATHSIG tracks the thread that forked the child, so that thread must outlive the child
pub fn prepare(command: &mut Command) -> Mark {
    let mark = NEXT_MARK.fetch_add(1, Ordering::Relaxed);

    // Outer marks are kept so an enclosing agent still recognizes nested programs
    let own = mark_text(mark);
    let value = match env::var(MARK_ENV) {
        Ok(outer) => format!("{outer} {own}"),
        Err(_) => own,
    };
    command.env(MARK_ENV, value);

    // The group id is the child's pid
    // SAFETY: Only the async-signal-safe setsid runs after fork
    unsafe {
        command.pre_exec(|| {
            // With the controlling terminal kept, sudo in a background group
            // keeps writing password prompts to it
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    #[cfg(target_os = "linux")]
    {
        let parent = process::id() as libc::pid_t;
        // SAFETY: Only the async-signal-safe prctl, getppid and _exit run after fork
        unsafe {
            command.pre_exec(move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                // The parent exited before prctl took effect
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }

    Mark(mark)
}

/// Dropping it kills the whole group, including processes that started a new session
pub struct Group {
    id: i32,
    mark: Mark,
    released: AtomicBool,
}

impl Group {
    /// The caller must keep the child unreaped, as a reaped pid can come back as another group's id
    pub fn new(pid: u32, mark: Mark) -> io::Result<Group> {
        // killpg(0) would kill our own group
        if pid == 0 {
            return Err(io::Error::other("no process id"));
        }

        let id = pid as i32;
        GROUPS.lock().expect("groups lock").push(id);
        tell(id);

        Ok(Group {
            id,
            mark,
            released: AtomicBool::new(false),
        })
    }

    pub fn alive(&self) -> bool {
        // SAFETY: Signal 0 only checks whether the group still has a process
        unsafe { libc::killpg(self.id, 0) == 0 }
    }

    /// After `release` the group id may already be reused, so it is never killed
    pub fn kill(&self) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        kill(self.id);
        sweep(self.mark);
    }

    /// Forgets the group without killing it, since its id gets reused once the group is empty
    pub fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        forget(self.id);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        forget(self.id);
        self.kill();
    }
}

pub fn kill_all() {
    for group in GROUPS.lock().expect("groups lock").drain(..) {
        kill(group);
    }

    // Nothing to sweep in /proc if no program was ever started
    if NEXT_MARK.load(Ordering::Relaxed) > 1 {
        let prefix = instance();
        kill_marked(prefix);
    }
}

fn forget(group: i32) {
    let mut groups = GROUPS.lock().expect("groups lock");
    groups.retain(|known| *known != group);
    tell(-group);
}

fn sweep(mark: Mark) {
    // Reading /proc can hang, so it must not hold up the caller's thread
    let text = mark_text(mark.0);
    let spawned = thread::Builder::new().spawn(move || kill_marked(&text));
    // At the process limit no thread can start, and a panic in Drop would abort
    if spawned.is_err() {
        let text = mark_text(mark.0);
        kill_marked(&text);
    }
}

fn kill(group: i32) {
    // SAFETY: Only sends a signal and touches no memory
    unsafe { libc::killpg(group, libc::SIGKILL) };
}

/// A positive id registers a group and a negative one drops it, in four-byte records
/// below PIPE_BUF that concurrent writes never interleave
fn tell(group: i32) {
    if let Some(mut guard) = GUARD.get() {
        let record = group.to_ne_bytes();
        guard.write_all(&record).ok();
    }
}

/// Pids get reused, so the start time goes in too
fn instance() -> &'static str {
    INSTANCE.get_or_init(|| {
        let started = SystemTime::now().duration_since(UNIX_EPOCH);
        let nanos = started.unwrap_or_default().as_nanos();
        let pid = process::id();
        format!("{pid}.{nanos}")
    })
}

fn mark_text(mark: u64) -> String {
    let instance = instance();
    format!("{instance}.{mark}")
}

#[cfg(not(target_os = "linux"))]
fn kill_marked(_text: &str) {}

/// `text` is a single mark or this process's prefix
#[cfg(target_os = "linux")]
fn kill_marked(text: &str) {
    let own = process::id() as libc::pid_t;
    // SAFETY: Only reads this process's uid
    let uid = unsafe { libc::geteuid() };

    for pass in 0..MAX_SWEEPS {
        // A process in the middle of exec has no readable environment,
        // so sweep at least once more after a pause
        if pass > 0 {
            thread::sleep(SWEEP_EVERY);
        }
        let Ok(entries) = fs::read_dir("/proc") else {
            return;
        };

        let mut found = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let pid: Option<libc::pid_t> = name.to_str().and_then(|name| name.parse().ok());
            let Some(pid) = pid.filter(|pid| *pid != own) else {
                continue;
            };
            // Other users' processes can't be killed, and reading one can block on its memory lock
            let Ok(owner) = entry.metadata() else {
                continue;
            };
            if owner.uid() != uid {
                continue;
            }

            // Taking a pidfd first guarantees the process read is the process killed
            // SAFETY: Only opens a new descriptor
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            let fd = i32::try_from(fd).ok().filter(|fd| *fd >= 0);
            // SAFETY: The descriptor was just opened and is owned here
            let handle = fd.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            let path = entry.path().join("environ");
            let Ok(environ) = fs::read(path) else {
                continue;
            };
            if !marked(&environ, text) {
                continue;
            }

            found = true;
            // SAFETY: Only sends a signal and touches no memory
            unsafe {
                match &handle {
                    Some(handle) => {
                        let fd = handle.as_raw_fd();
                        let info = ptr::null::<libc::siginfo_t>();
                        libc::syscall(libc::SYS_pidfd_send_signal, fd, libc::SIGKILL, info, 0);
                    }
                    None => {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }

        if !found && pass > 0 {
            break;
        }
    }
}

/// `environ` is \0-separated `NAME=value` pairs with marks listed outermost first,
/// and `text` only matches up to a `.` boundary
#[cfg(target_os = "linux")]
fn marked(environ: &[u8], text: &str) -> bool {
    let prefix = format!("{MARK_ENV}=");
    for var in environ.split(|byte| *byte == 0) {
        let Some(value) = var.strip_prefix(prefix.as_bytes()) else {
            continue;
        };
        let value = String::from_utf8_lossy(value);
        let whole = |mark: &str| match mark.strip_prefix(text) {
            Some(rest) => rest.is_empty() || rest.starts_with('.'),
            None => false,
        };
        return value.split(' ').any(whole);
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::unix::prepare;
    #[cfg(target_os = "linux")]
    use crate::unix::{marked, sweep};
    #[cfg(target_os = "linux")]
    use std::fs;
    use std::process::Command;
    #[cfg(target_os = "linux")]
    use std::process::Stdio;
    #[cfg(target_os = "linux")]
    use std::thread;
    #[cfg(target_os = "linux")]
    use std::time::{Duration, Instant};

    #[test]
    fn own_session() {
        let mut command = Command::new("sleep");
        command.arg("30");
        prepare(&mut command);
        let mut child = command.spawn().expect("spawn");
        let pid = child.id() as libc::pid_t;

        // SAFETY: Only reads this process's session and group ids
        let ids = unsafe { (libc::getsid(pid), libc::getpgid(pid)) };
        child.kill().ok();
        child.wait().ok();
        assert_eq!(ids, (pid, pid));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn marks_matched() {
        let environ = "PATH=/bin\0SMOL_REAPER=9.9.1 1.2.3\0CAFÉ=crème\0";
        assert!(marked(environ.as_bytes(), "1.2.3"));
        // A process prefix matches every program that process started
        assert!(marked(environ.as_bytes(), "1.2"));

        let other = "SMOL_REAPER=1.2.30 11.2.3\0CAFÉ=1.2.3\0";
        assert!(!marked(other.as_bytes(), "1.2.3"));
        assert!(!marked(b"", "1.2.3"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn new_session_swept() {
        let mut command = Command::new("sh");
        let script = "setsid sleep 30 >/dev/null 2>&1 & echo $!";
        command.args(["-c", script]).stdout(Stdio::piped());
        let mark = prepare(&mut command);
        let output = command.output().expect("run");
        let pid = String::from_utf8_lossy(&output.stdout);
        let pid: libc::pid_t = pid.trim().parse().expect("pid");
        // Where PID 1 doesn't reap orphans the killed process stays a zombie
        let alive = || {
            let path = format!("/proc/{pid}/stat");
            let Ok(stat) = fs::read_to_string(path) else {
                return false;
            };
            // The state follows the name in parentheses, which may hold parentheses itself
            let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
            !state.is_some_and(|state| state.starts_with('Z'))
        };
        assert!(alive());

        sweep(mark);
        let started = Instant::now();
        while alive() {
            assert!(started.elapsed() < Duration::from_secs(5), "{pid}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
