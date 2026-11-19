use crate::{worded, Member};
#[cfg(target_os = "linux")]
use std::env;
#[cfg(target_os = "linux")]
use std::fs;
use std::fs::File;
use std::io::{self, PipeWriter, Read, Write};
use std::mem;
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process;
use std::process::Command;
use std::ptr;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(target_os = "linux")]
use std::time::{SystemTime, UNIX_EPOCH};

const KILL_WAIT: Duration = Duration::from_secs(1);
const CHECK_EVERY: Duration = Duration::from_millis(20);

const LIVE: u8 = 0;
const RELEASED: u8 = 1;
const ENDED: u8 = 2;

/// Processes that started a new session left the group and are found by this inherited variable
#[cfg(target_os = "linux")]
const MARK_ENV: &str = "SMOL_REAPER";
/// A process killed a moment ago may already have forked children
#[cfg(target_os = "linux")]
const MAX_SWEEPS: usize = 3;
#[cfg(target_os = "linux")]
const SWEEP_EVERY: Duration = Duration::from_millis(50);

static GROUPS: Mutex<Vec<Known>> = Mutex::new(Vec::new());
#[cfg(target_os = "linux")]
static NEXT_MARK: AtomicU64 = AtomicU64::new(1);
#[cfg(target_os = "linux")]
static INSTANCE: OnceLock<String> = OnceLock::new();
/// Pipe to the watchdog, closed however this process dies
static GUARD: OnceLock<PipeWriter> = OnceLock::new();

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub struct Mark(u64);
/// Only Linux finds the processes that left the group, in /proc
#[cfg(not(target_os = "linux"))]
#[derive(Clone, Copy)]
pub struct Mark(());

struct Known {
    id: i32,
    /// A second SIGTERM makes some programs skip their cleanup
    termed: bool,
}

/// What /proc tells of a process
#[cfg(target_os = "linux")]
struct Listed {
    pid: u32,
    parent: u32,
    group: i32,
    started: u64,
    zombie: bool,
    /// Its executable's name, for one that shows no arguments
    name: String,
    path: PathBuf,
}

/// Must be called before any thread starts, because the forked watchdog keeps running Rust
pub fn guard() {
    let Ok((mut reader, writer)) = io::pipe() else {
        return;
    };

    // The watchdog sweeps by this prefix, so it has to exist before the fork
    #[cfg(target_os = "linux")]
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
    // SAFETY: Only changes this process's own group, signal dispositions and descriptors
    unsafe {
        libc::setpgid(0, 0);
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

    for &group in &groups {
        kill(group, libc::SIGTERM);
    }
    wait_out(groups, false);

    #[cfg(target_os = "linux")]
    {
        let prefix = instance();
        kill_marked(prefix);
    }

    // SAFETY: Skips the exit handlers inherited from the parent
    unsafe { libc::_exit(0) }
}

/// PDEATHSIG tracks the thread that forked the child, so that thread must outlive the child
pub fn prepare(command: &mut Command) -> Mark {
    #[cfg(target_os = "linux")]
    let mark = {
        let number = NEXT_MARK.fetch_add(1, Ordering::Relaxed);
        // Outer marks are kept so an enclosing agent still recognizes nested programs
        let own = mark_text(number);
        let value = match env::var(MARK_ENV) {
            Ok(outer) => format!("{outer} {own}"),
            Err(_) => own,
        };
        command.env(MARK_ENV, value);
        Mark(number)
    };
    #[cfg(not(target_os = "linux"))]
    let mark = Mark(());

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

    mark
}

/// Dropping it kills the whole group, including processes that started a new session
pub struct Group {
    id: i32,
    mark: Mark,
    /// `LIVE`, `RELEASED` or `ENDED`, and only a live group is signaled by id, as a released one's
    /// id may belong to another group
    state: AtomicU8,
}

impl Group {
    /// The caller must keep the child unreaped, as a reaped pid can come back as another group's id
    pub fn new(pid: u32, mark: Mark) -> io::Result<Group> {
        // killpg(0) would kill our own group
        if pid == 0 {
            return Err(io::Error::other("no process id"));
        }

        let id = pid as i32;
        let known = Known { id, termed: false };
        GROUPS.lock().expect("groups lock").push(known);
        tell(id);

        Ok(Group {
            id,
            mark,
            state: AtomicU8::new(LIVE),
        })
    }

    /// False once released or killed, as the id may belong to another group by then
    pub fn alive(&self) -> bool {
        self.state.load(Ordering::SeqCst) == LIVE && alive(self.id)
    }

    /// Includes descendants in a session of their own, which leaves the group but not the tree,
    /// leaves out zombies, is none once released or killed and reads /proc, which can block an async
    /// thread
    #[cfg(target_os = "linux")]
    pub fn members(&self) -> io::Result<Vec<Member>> {
        if self.state.load(Ordering::SeqCst) != LIVE {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir("/proc")?;

        // All are read before following parents, which /proc may list after their children
        let mut listed = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let pid: Option<u32> = name.to_str().and_then(|name| name.parse().ok());
            let Some(pid) = pid else {
                continue;
            };
            let path = entry.path();
            let Ok(stat) = fs::read_to_string(path.join("stat")) else {
                continue;
            };

            // The fields follow the name in parentheses, which may hold parentheses itself,
            // and proc(5) numbers the state 3, so the parent, group and start time are 1, 2 and 19
            let Some((head, tail)) = stat.rsplit_once(')') else {
                continue;
            };
            let fields: Vec<&str> = tail.split_whitespace().collect();
            let state = fields.first().copied().unwrap_or_default();
            let parent = fields.get(1).and_then(|field| field.parse().ok());
            let group = fields.get(2).and_then(|field| field.parse().ok());
            let started = fields.get(19).and_then(|field| field.parse().ok());
            let (Some(parent), Some(group), Some(started)) = (parent, group, started) else {
                continue;
            };
            let name = head.split_once('(').map_or("", |(_, name)| name);
            listed.push(Listed {
                pid,
                parent,
                group,
                started,
                zombie: matches!(state, "Z" | "X"),
                name: name.to_owned(),
                path,
            });
        }

        let mut inside: Vec<u32> = Vec::new();
        for process in listed.iter().filter(|process| process.group == self.id) {
            inside.push(process.pid);
        }
        loop {
            let before = inside.len();
            for process in &listed {
                if inside.contains(&process.parent) && !inside.contains(&process.pid) {
                    inside.push(process.pid);
                }
            }
            if inside.len() == before {
                break;
            }
        }

        let mut members = Vec::new();
        for process in listed {
            let first = process.pid == self.id as u32;
            if first || process.zombie || !inside.contains(&process.pid) {
                continue;
            }

            // A process in the middle of exec shows no arguments
            let arguments = fs::read(process.path.join("cmdline")).unwrap_or_default();
            let arguments = String::from_utf8_lossy(&arguments);
            let command = match arguments.trim_matches('\0').is_empty() {
                true => worded(&process.name),
                false => worded(&arguments),
            };
            members.push(Member {
                pid: process.pid,
                parent: process.parent,
                started: process.started,
                command,
            });
        }
        Ok(members)
    }

    /// Includes descendants in a session of their own, which leaves the group but not the tree,
    /// leaves out zombies and is none once released or killed
    #[cfg(target_os = "macos")]
    pub fn members(&self) -> io::Result<Vec<Member>> {
        if self.state.load(Ordering::SeqCst) != LIVE {
            return Ok(Vec::new());
        }

        // SAFETY: The buffer holds `size` bytes
        let grouped = |buffer, size| unsafe { libc::proc_listpgrppids(self.id, buffer, size) };
        let mut inside = pids(grouped)?;
        let mut next = 0;
        while let Some(&parent) = inside.get(next) {
            next += 1;
            // SAFETY: The buffer holds `size` bytes
            let born = |buffer, size| unsafe { libc::proc_listchildpids(parent, buffer, size) };
            // One that exited meanwhile has no children to list
            let children = pids(born).unwrap_or_default();
            for child in children {
                if !inside.contains(&child) {
                    inside.push(child);
                }
            }
        }

        let mut members = Vec::new();
        for &pid in inside.iter().filter(|pid| **pid != self.id) {
            // SAFETY: proc_bsdinfo holds only integers, for which zero is valid
            let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
            let size = mem::size_of_val(&info) as libc::c_int;
            let buffer = (&raw mut info).cast();
            // SAFETY: The buffer holds `size` bytes
            let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, buffer, size) };
            // A process that already exited has nothing to read
            if read != size || info.pbi_status == libc::SZOMB {
                continue;
            }

            let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
            let mut length = 0;
            let unused = ptr::null_mut();
            // SAFETY: A null buffer only asks for the length
            let sized =
                unsafe { libc::sysctl(name.as_mut_ptr(), 3, unused, &mut length, unused, 0) };
            let mut procargs = vec![0u8; length];
            let buffer = procargs.as_mut_ptr().cast();
            // SAFETY: The buffer holds `length` bytes
            let read =
                unsafe { libc::sysctl(name.as_mut_ptr(), 3, buffer, &mut length, unused, 0) };
            procargs.truncate(length);

            let command = match sized == 0 && read == 0 {
                true => arguments(&procargs),
                false => None,
            };
            let comm = info.pbi_comm.iter().take_while(|c| **c != 0);
            let comm: Vec<u8> = comm.map(|c| *c as u8).collect();
            let command = command.unwrap_or_else(|| String::from_utf8_lossy(&comm).into_owned());
            let started = info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec;
            members.push(Member {
                pid: pid as u32,
                parent: info.pbi_ppid,
                started,
                command: worded(&command),
            });
        }
        Ok(members)
    }

    /// Sends SIGTERM, then SIGKILL to what is left after `KILL_WAIT`, and of a released group only
    /// ends the processes that left it
    pub fn kill(&self) {
        let released = match self.state.swap(ENDED, Ordering::SeqCst) {
            ENDED => return,
            state => state == RELEASED,
        };
        // Only Linux finds the processes that left a group
        if released && cfg!(not(target_os = "linux")) {
            return;
        }

        if !released {
            // Held while signaling, as `kill_all` must not send a second SIGTERM
            let mut groups = GROUPS.lock().expect("groups lock");
            let found = groups.iter_mut().find(|known| known.id == self.id);
            if let Some(known) = found.filter(|known| !known.termed) {
                known.termed = true;
                kill(self.id, libc::SIGTERM);
            }
        }

        // Waiting and reading /proc must not hold up the caller's thread
        let (id, mark) = (self.id, self.mark);
        let finishing = move || match released {
            true => sweep(mark),
            false => finish(id, mark),
        };
        let spawned = thread::Builder::new().spawn(finishing);
        // At the process limit no thread can start, and a panic in Drop would abort
        if spawned.is_err() {
            finishing();
        }
    }

    /// Forgets the group without killing it, since its id gets reused once the group is empty,
    /// while dropping it still ends the processes that left it
    pub fn release(&self) {
        let state = &self.state;
        let released = state.compare_exchange(LIVE, RELEASED, Ordering::SeqCst, Ordering::SeqCst);
        if released.is_ok() {
            forget(self.id);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Ends every registered group as `Group::kill` does but in place, reaping their first processes,
/// which nothing else may still wait for
pub fn kill_all() {
    let mut registered = GROUPS.lock().expect("groups lock");
    let known = mem::take(&mut *registered);
    drop(registered);

    let mut groups = Vec::new();
    for known in known {
        if !known.termed {
            kill(known.id, libc::SIGTERM);
        }
        groups.push(known.id);
    }
    wait_out(groups.clone(), true);

    // Their ids may be reused once this process exits and the watchdog runs
    for group in groups {
        tell(-group);
    }

    // Nothing to sweep in /proc if no program was ever started
    #[cfg(target_os = "linux")]
    if NEXT_MARK.load(Ordering::Relaxed) > 1 {
        let prefix = instance();
        kill_marked(prefix);
    }
}

fn finish(group: i32, mark: Mark) {
    wait_out(vec![group], false);

    // Before the sweep, which may hang reading /proc, as an empty group's id can be reused
    forget(group);
    sweep(mark);
}

/// Waits up to `KILL_WAIT` for the groups to end, then sends SIGKILL to what is left of them
fn wait_out(mut groups: Vec<i32>, reaping: bool) {
    let deadline = Instant::now() + KILL_WAIT;
    loop {
        // An unreaped zombie keeps its group alive
        if reaping {
            for &group in &groups {
                let status = ptr::null_mut();
                // SAFETY: Only collects the status of children in the group that already ended
                while unsafe { libc::waitpid(-group, status, libc::WNOHANG) } > 0 {}
            }
        }
        groups.retain(|group| alive(*group));
        if groups.is_empty() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(CHECK_EVERY);
    }

    for group in groups {
        kill(group, libc::SIGKILL);
    }
}

fn forget(group: i32) {
    let mut groups = GROUPS.lock().expect("groups lock");
    groups.retain(|known| known.id != group);
    tell(-group);
}

#[cfg(target_os = "linux")]
fn sweep(mark: Mark) {
    let text = mark_text(mark.0);
    kill_marked(&text);
}

#[cfg(not(target_os = "linux"))]
fn sweep(_: Mark) {}

fn alive(group: i32) -> bool {
    // SAFETY: Signal 0 only checks whether the group still has a process
    unsafe { libc::killpg(group, 0) == 0 }
}

fn kill(group: i32, signal: libc::c_int) {
    // SAFETY: Only sends a signal and touches no memory
    unsafe { libc::killpg(group, signal) };
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
#[cfg(target_os = "linux")]
fn instance() -> &'static str {
    INSTANCE.get_or_init(|| {
        let started = SystemTime::now().duration_since(UNIX_EPOCH);
        let nanos = started.unwrap_or_default().as_nanos();
        let pid = process::id();
        format!("{pid}.{nanos}")
    })
}

#[cfg(target_os = "linux")]
fn mark_text(mark: u64) -> String {
    let instance = instance();
    format!("{instance}.{mark}")
}

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

/// The pids `list` writes into a buffer of the size it is given, a larger one until they fit
#[cfg(target_os = "macos")]
fn pids(
    list: impl Fn(*mut libc::c_void, libc::c_int) -> libc::c_int,
) -> io::Result<Vec<libc::pid_t>> {
    let mut pids: Vec<libc::pid_t> = vec![0; 64];
    loop {
        let size = mem::size_of_val(pids.as_slice()) as libc::c_int;
        let count = list(pids.as_mut_ptr().cast(), size);
        let Ok(count) = usize::try_from(count) else {
            return Err(io::Error::last_os_error());
        };
        // A full buffer may have left some out
        if count < pids.len() {
            pids.truncate(count);
            return Ok(pids);
        }
        pids.resize(pids.len() * 2, 0);
    }
}

/// What KERN_PROCARGS2 gives is the count of arguments, the executable's path,
/// NULs up to the first argument, then each argument and the environment ending in NUL
#[cfg(target_os = "macos")]
fn arguments(procargs: &[u8]) -> Option<String> {
    let (count, rest) = procargs.split_first_chunk::<4>()?;
    let count = usize::try_from(i32::from_ne_bytes(*count)).ok()?;
    let path = rest.iter().position(|byte| *byte == 0)?;
    let rest = &rest[path..];
    let first = rest.iter().position(|byte| *byte != 0)?;

    let mut arguments = Vec::new();
    for argument in rest[first..].split(|byte| *byte == 0).take(count) {
        let argument = String::from_utf8_lossy(argument);
        arguments.push(argument.into_owned());
    }
    Some(arguments.join(" "))
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
    use crate::unix::{prepare, Group, KILL_WAIT};
    #[cfg(target_os = "linux")]
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::io::Read;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    #[cfg(target_os = "linux")]
    use std::thread;
    #[cfg(target_os = "linux")]
    use std::time::Duration;
    use std::time::Instant;

    /// The group gets SIGTERM first, and a program that handles it ends cleanly
    #[test]
    fn term_first() {
        let script = "trap 'echo cleaned; exit 0' TERM; echo ready; sleep 30 & wait";
        let mut command = Command::new("sh");
        command.args(["-c", script]).stdout(Stdio::piped());
        let mark = prepare(&mut command);
        let mut child = command.spawn().expect("spawn");
        let group = Group::new(child.id(), mark).expect("group");
        let mut said = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        said.read_line(&mut line).expect("ready");

        drop(group);
        let status = child.wait().expect("wait");
        said.read_line(&mut line).expect("cleaned");
        assert_eq!(line, "ready\ncleaned\n");
        assert_eq!(status.code(), Some(0));
    }

    /// A program that ignores SIGTERM gets SIGKILL once `KILL_WAIT` passed
    #[test]
    fn killed_after_wait() {
        let script = "trap '' TERM; echo ready; exec sleep 30";
        let mut command = Command::new("sh");
        command.args(["-c", script]).stdout(Stdio::piped());
        let mark = prepare(&mut command);
        let mut child = command.spawn().expect("spawn");
        let group = Group::new(child.id(), mark).expect("group");
        let mut said = BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        said.read_line(&mut line).expect("ready");

        let started = Instant::now();
        group.kill();
        let status = child.wait().expect("wait");
        let waited = started.elapsed();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(waited >= KILL_WAIT, "{waited:?}");
        assert!(waited < KILL_WAIT * 5, "{waited:?}");
    }

    /// A group released once its first process ended, as when a command is done, still ends
    /// what left it when dropped
    #[test]
    #[cfg(target_os = "linux")]
    fn new_session_swept() {
        let mut command = Command::new("sh");
        let script = "setsid sleep 30 >/dev/null 2>&1 & echo $!";
        command.args(["-c", script]).stdout(Stdio::piped());
        let mark = prepare(&mut command);
        let mut child = command.spawn().expect("spawn");
        let group = Group::new(child.id(), mark).expect("group");
        let mut said = String::new();
        let mut stdout = child.stdout.take().expect("stdout");
        stdout.read_to_string(&mut said).expect("pid");
        child.wait().expect("wait");
        let pid: libc::pid_t = said.trim().parse().expect("pid");

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

        group.release();
        drop(group);
        let started = Instant::now();
        while alive() {
            assert!(started.elapsed() < Duration::from_secs(5), "{pid}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
