use crate::utils::new_id;
#[cfg(target_os = "linux")]
use landlock::{
    Access as _, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, Scope,
    ABI,
};
use reaper::{cause, tilde};
#[cfg(any(target_os = "macos", test))]
use std::collections::BTreeSet;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use std::convert::Infallible;
use std::env;
#[cfg(target_os = "linux")]
use std::ffi::c_void;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::ptr;
use tokio::process::Command;
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;

const TEMP_PREFIX: &str = "smol-";
/// The sandbox holds a lock on this file in its temporary folder while it lives
const LOCK: &str = "lock";

/// `LANDLOCK_CREATE_RULESET_VERSION`, which libc doesn't define
#[cfg(target_os = "linux")]
const LANDLOCK_VERSION: libc::c_uint = 1;
/// Applies a profile and starts a program under it, by full path rather than looked up on `PATH`
#[cfg(target_os = "macos")]
const SEATBELT: &str = "/usr/bin/sandbox-exec";

#[cfg(unix)]
const TEMP_ENV: &[&str] = &["TMPDIR"];
#[cfg(windows)]
const TEMP_ENV: &[&str] = &["TEMP", "TMP"];
const PIP_CACHE_ENV: &str = "PIP_CACHE_DIR";

/// The Landlock ruleset the worker restricts itself with
#[cfg(target_os = "linux")]
type Isolation = OwnedFd;
/// The Seatbelt profile the worker starts under
#[cfg(target_os = "macos")]
type Isolation = String;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
type Isolation = Infallible;

/// What a Landlock rule lets the code do with a path and all below it
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Grant {
    /// For a folder that holds a hidden path, which lets the code list the hidden folders too
    List,
    Read,
    Write,
}

/// What the code and the programs it starts may reach. Everything else is readable,
/// so `read` matters only below a hidden place
#[derive(Default)]
pub(crate) struct Rules {
    /// Names of the environment variables passed on
    pub(crate) env: Vec<String>,
    pub(crate) write: Vec<PathBuf>,
    pub(crate) read: Vec<PathBuf>,
    /// Places the code can neither read nor write
    pub(crate) hidden: Vec<PathBuf>,
}

/// Starts programs confined by `Rules`, with a temporary folder of their own, kept across restarts
pub(crate) struct Sandbox {
    temp: PathBuf,
    lock: Option<File>,
    env: Vec<(OsString, OsString)>,
    isolation: Option<Isolation>,
}

impl Sandbox {
    pub(crate) fn new(rules: &Rules) -> Result<Self, String> {
        let id = new_id();
        let name = format!("{TEMP_PREFIX}{id}");
        let temp = env::temp_dir().join(name);
        let scratch = temp.join("tmp");
        let failed = |path: &Path, e: io::Error| {
            let shown = tilde(path);
            let reason = cause(&e);
            format!("{shown}: {reason}")
        };
        fs::create_dir_all(&scratch).map_err(|e| failed(&scratch, e))?;

        // Dropping it when a step below fails removes the temporary folder
        let mut sandbox = Self {
            temp: temp.clone(),
            lock: None,
            env: Vec::new(),
            isolation: None,
        };
        let path = temp.join(LOCK);
        let lock = File::create(&path).map_err(|e| failed(&path, e))?;
        lock.lock().map_err(|e| failed(&path, e))?;
        sandbox.lock = Some(lock);

        for name in &rules.env {
            let Some(value) = env::var_os(name) else {
                continue;
            };
            sandbox.env.push((OsString::from(name), value));
        }
        for name in TEMP_ENV {
            let value = scratch.clone().into_os_string();
            sandbox.env.push((OsString::from(name), value));
        }

        let mut granted = vec![(PathBuf::from("/"), false)];
        for path in &rules.read {
            granted.push((path.clone(), false));
        }
        for path in [&scratch, Path::new("/dev")] {
            granted.push((path.to_owned(), true));
        }
        for path in &rules.write {
            granted.push((path.clone(), true));
        }

        // Links resolve before the kernel checks a path, so rules compare real paths
        let mut real = Vec::new();
        for (path, write) in granted {
            if let Ok(path) = fs::canonicalize(&path) {
                real.push((path, write));
            }
        }

        let mut hidden = rules.hidden.clone();
        for path in &mut hidden {
            if let Ok(resolved) = fs::canonicalize(&path) {
                *path = resolved;
            }
        }

        sandbox.isolation = isolated(&real, &hidden);
        // pip's own cache can't be written, which building a package from source needs.
        // First, so a cache that `[allow] env` passes wins
        if sandbox.isolation.is_some() {
            let cache = scratch.join("pip").into_os_string();
            sandbox
                .env
                .insert(0, (OsString::from(PIP_CACHE_ENV), cache));
        }

        Ok(sandbox)
    }

    pub(crate) fn enforced(&self) -> bool {
        self.isolation.is_some()
    }

    /// A command before its arguments, starting in `cwd` and confined before it runs any code
    pub(crate) fn command(&self, program: &Path, cwd: &Path) -> Command {
        #[cfg(target_os = "macos")]
        let mut command = match &self.isolation {
            Some(profile) => {
                let mut command = Command::new(SEATBELT);
                command.arg("-p").arg(profile).arg(program);
                command
            }
            None => Command::new(program),
        };
        #[cfg(not(target_os = "macos"))]
        let mut command = Command::new(program);
        command.current_dir(cwd).env_clear();
        for (name, value) in &self.env {
            command.env(name, value);
        }

        #[cfg(target_os = "linux")]
        if let Some(ruleset) = &self.isolation {
            let ruleset = ruleset.as_raw_fd();
            // SAFETY: Only the async-signal-safe prctl and syscall run after fork
            unsafe {
                command.pre_exec(move || {
                    // A process without privileges may restrict itself only with this set
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        command
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Windows can't remove the lock file while it is open
        self.lock.take();
        fs::remove_dir_all(&self.temp).ok();
    }
}

#[cfg(target_os = "linux")]
fn isolated(granted: &[(PathBuf, bool)], hidden: &[PathBuf]) -> Option<Isolation> {
    // A missing path would only get more folders granted entry by entry
    let hidden: Vec<PathBuf> = hidden
        .iter()
        .filter(|path| path.exists())
        .cloned()
        .collect();
    let mut allowed = Vec::new();
    for (path, write) in granted {
        let grant = match write {
            true => Grant::Write,
            false => Grant::Read,
        };
        spread(path, grant, &hidden, &mut allowed);
    }

    // A kernel drops the rights and scopes it doesn't know, and the rest still holds
    let abi = ABI::V9;
    let all = AccessFs::from_all(abi);
    // Connecting to a Unix socket is a write right, so only writable folders allow it
    let read = AccessFs::from_read(abi);
    let mut rules = Vec::new();
    for (path, grant) in allowed {
        let Ok(fd) = PathFd::new(&path) else {
            continue;
        };
        let mut access = match grant {
            Grant::Write => all,
            Grant::Read => read,
            Grant::List => AccessFs::ReadDir.into(),
        };
        // The kernel refuses a rule on a file that names rights only folders have
        if !path.is_dir() {
            access &= AccessFs::from_file(abi);
        }
        rules.push(PathBeneath::new(fd, access));
    }

    let rules = rules.into_iter().map(Ok);
    let created = Ruleset::default()
        .handle_access(all)
        .and_then(|ruleset| ruleset.scope(Scope::Signal | Scope::AbstractUnixSocket))
        .and_then(Ruleset::create)
        .and_then(|ruleset| ruleset.add_rules(rules));
    let ruleset: Option<OwnedFd> = match created {
        Ok(ruleset) => ruleset.into(),
        Err(e) => {
            warn!("sandbox: not enforced, {e}");
            return None;
        }
    };

    let version = landlock_abi();
    let open = match version {
        ..6 => Some("signals and unix sockets"),
        6..9 => Some("unix sockets"),
        _ => None,
    };
    match (&ruleset, open) {
        (Some(_), None) => info!("sandbox: enforced (landlock)"),
        (Some(_), Some(open)) => info!("sandbox: enforced (landlock), {open} open [abi={version}]"),
        (None, _) => info!("sandbox: not enforced, landlock unavailable"),
    }

    ruleset
}

/// Negative without Landlock
#[cfg(target_os = "linux")]
fn landlock_abi() -> libc::c_long {
    // SAFETY: A null attribute with the version flag only asks for the kernel's ABI
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            ptr::null::<c_void>(),
            0_usize,
            LANDLOCK_VERSION,
        )
    }
}

/// Landlock can't take back access below a granted folder,
/// so a folder holding a hidden path is granted entry by entry and may only be listed
#[cfg(target_os = "linux")]
fn spread(path: &Path, grant: Grant, hidden: &[PathBuf], allowed: &mut Vec<(PathBuf, Grant)>) {
    if hidden.iter().any(|spot| spot == path) {
        return;
    }
    let holds = hidden.iter().any(|spot| spot.starts_with(path));
    if !holds {
        allowed.push((path.to_owned(), grant));
        return;
    }

    allowed.push((path.to_owned(), Grant::List));
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        // A rule on a link would grant where it leads, which `/` already lets the code read
        let linked = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        if !linked {
            let inner = entry.path();
            spread(&inner, grant, hidden, allowed);
        }
    }
}

#[cfg(target_os = "macos")]
fn isolated(granted: &[(PathBuf, bool)], hidden: &[PathBuf]) -> Option<Isolation> {
    // Apple deprecated it, and the worker runs unconfined once it is gone
    if !Path::new(SEATBELT).exists() {
        info!("sandbox: not enforced, sandbox-exec not found");
        return None;
    }

    info!("sandbox: enforced (seatbelt)");
    let profile = profile(granted, hidden);
    Some(profile)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn isolated(_: &[(PathBuf, bool)], _: &[PathBuf]) -> Option<Isolation> {
    None
}

/// Seatbelt takes the last rule that matches, so deeper paths come later
/// and a hidden path or a folder above one comes after a granted one of the same depth
#[cfg(any(target_os = "macos", test))]
fn profile(granted: &[(PathBuf, bool)], hidden: &[PathBuf]) -> String {
    let mut rules: Vec<(&Path, &str, &str)> = Vec::new();
    for (path, write) in granted {
        let rule = match write {
            true => "allow file-read* file-write* network-outbound",
            false => "allow file-read*",
        };
        rules.push((path, rule, "subpath"));
        // Unlike Landlock, a subpath rule lets the code swap the folder itself for a link,
        // which a later session would grant
        if *write {
            rules.push((path, "deny file-write-unlink file-write-create", "literal"));
        }
    }
    for path in hidden {
        rules.push((
            path,
            "deny file-read* file-write* network-outbound",
            "subpath",
        ));
    }

    // A rule follows the path, so a folder above a hidden one must stay where it is
    let mut above = BTreeSet::new();
    for path in hidden {
        above.extend(path.ancestors().skip(1));
    }
    for path in above {
        rules.push((path, "deny file-write-unlink file-write-create", "literal"));
    }
    rules.sort_by_key(|(path, _, _)| path.components().count());

    // Only connecting to a Unix socket has a path, so TCP and UDP stay open
    let mut profile = "(version 1)\n(allow default)\n(deny file-write*)\n\
        (deny network-outbound (subpath \"/\"))\n"
        .to_owned();
    for (path, rule, matched) in rules {
        let path = path.to_string_lossy();
        let path = path.replace('\\', "\\\\").replace('"', "\\\"");
        let line = format!("({rule} ({matched} \"{path}\"))\n");
        profile.push_str(&line);
    }
    // Names resolve through this socket, and signals reach only the worker's own process group
    profile.push_str(
        "(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n\
        (deny signal (target others))\n",
    );
    profile
}

/// Removes the temporary folders of sandboxes whose libsmol died without dropping them
pub(crate) fn sweep() {
    let temp = env::temp_dir();
    let Ok(entries) = fs::read_dir(&temp) else {
        return;
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(TEMP_PREFIX) {
            continue;
        }
        let folder = entry.path();
        let Ok(lock) = File::open(folder.join(LOCK)) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        drop(lock);
        if fs::remove_dir_all(&folder).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        info!("sandbox: stale folders removed [count={removed}]");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::config::Access;
    use crate::tests::{temp_dir, write_file};
    use crate::utils::{block_on, new_id, Interrupt};
    use crate::worker::host::tests::unserved;
    use crate::worker::host::Worker;
    #[cfg(target_os = "linux")]
    use crate::worker::sandbox::{landlock_abi, spread, Grant};
    use crate::worker::sandbox::{profile, sweep, LOCK, PIP_CACHE_ENV, TEMP_PREFIX};
    use crate::worker::workplace::tests::opened;
    use crate::worker::workplace::{Workplace, PASSED_ENV};
    use crate::worker::Delegation;
    use std::env;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(target_os = "linux")]
    use std::os::unix::net::SocketAddr;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn profile_built() {
        let granted = [
            (PathBuf::from("/Users/zoë/.smol/skills/p\"d\\f"), false),
            (PathBuf::from("/"), false),
            (PathBuf::from("/Users/zoë/café"), true),
        ];
        let hidden = [
            PathBuf::from("/Users/zoë/a/b"),
            PathBuf::from("/Users/zoë/.ssh"),
            PathBuf::from("/Users/zoë/.smol"),
        ];
        let expected = "(version 1)\n(allow default)\n(deny file-write*)\n\
            (deny network-outbound (subpath \"/\"))\n\
            (allow file-read* (subpath \"/\"))\n\
            (deny file-write-unlink file-write-create (literal \"/\"))\n\
            (deny file-write-unlink file-write-create (literal \"/Users\"))\n\
            (deny file-write-unlink file-write-create (literal \"/Users/zoë\"))\n\
            (allow file-read* file-write* network-outbound (subpath \"/Users/zoë/café\"))\n\
            (deny file-write-unlink file-write-create (literal \"/Users/zoë/café\"))\n\
            (deny file-read* file-write* network-outbound (subpath \"/Users/zoë/.ssh\"))\n\
            (deny file-read* file-write* network-outbound (subpath \"/Users/zoë/.smol\"))\n\
            (deny file-write-unlink file-write-create (literal \"/Users/zoë/a\"))\n\
            (deny file-read* file-write* network-outbound (subpath \"/Users/zoë/a/b\"))\n\
            (allow file-read* (subpath \"/Users/zoë/.smol/skills/p\\\"d\\\\f\"))\n\
            (allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n\
            (deny signal (target others))\n";
        assert_eq!(profile(&granted, &hidden), expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn folders_spread() {
        let dir = temp_dir("spread");
        let outside = dir.join("outside");
        let home = dir.join("home");
        write_file(&home.join(".ssh/id_rsa"), b"secret");
        write_file(&home.join(".config/gh/hosts.yml"), b"token");
        write_file(&home.join(".config/git/config"), b"[user]");
        write_file(&home.join(".gitconfig"), b"[user]");
        write_file(&home.join("café/main.rs"), b"fn main() {}");
        write_file(&outside.join("notes.md"), b"notes");
        symlink(home.join(".ssh"), home.join("keys")).expect("link");
        symlink(home.join(".config"), home.join("settings")).expect("link");
        symlink(&outside, home.join("data")).expect("link");
        let home = fs::canonicalize(&home).expect("real");
        let hidden = [home.join(".ssh"), home.join(".config/gh")];

        let mut allowed = Vec::new();
        spread(&home, Grant::Read, &hidden, &mut allowed);
        spread(&home.join("café"), Grant::Write, &hidden, &mut allowed);
        spread(&home.join(".ssh"), Grant::Write, &hidden, &mut allowed);
        allowed.sort();
        fs::remove_dir_all(&dir).ok();

        let expected = [
            (home.clone(), Grant::List),
            (home.join(".config"), Grant::List),
            (home.join(".config/git"), Grant::Read),
            (home.join(".gitconfig"), Grant::Read),
            (home.join("café"), Grant::Read),
            (home.join("café"), Grant::Write),
        ];
        assert_eq!(allowed, expected);
    }

    fn evaluated(workplace: Workplace, code: &str) -> String {
        let interrupt = Interrupt::default();
        let restarted = false;
        let starting = Worker::new(
            &interrupt,
            Arc::new(workplace),
            &[],
            Delegation::Off,
            restarted,
        );
        let mut worker = block_on(starting).expect("worker");

        let mut ignore = |_: &str| {};
        let mut none = unserved;
        let evaluating = worker.evaluate(code, &[], &mut ignore, &mut none);
        let output = block_on(evaluating).expect("evaluate");
        assert_eq!(output.error, None, "{}", output.logs);
        output.logs
    }

    /// Programs the code starts are confined too, and the folders the code writes stay in place
    #[test]
    fn files_confined() {
        let dir = temp_dir("confined");
        let cwd = dir.join("café");
        fs::create_dir_all(&cwd).expect("dir");
        write_file(&dir.join("secret/id_rsa"), b"key");
        write_file(&dir.join("notes.md"), b"notes");
        let shared = dir.join("shared");
        fs::create_dir_all(&shared).expect("dir");
        let access = Access {
            write: vec![shared.clone(), dir.join("missing")],
            hidden: vec![dir.join("secret")],
            ..Access::default()
        };
        let workplace = opened(&cwd, &dir.join("config"), &access);
        #[cfg(target_os = "macos")]
        assert!(workplace.sandbox.enforced());
        if !workplace.sandbox.enforced() {
            fs::remove_dir_all(&dir).ok();
            return;
        }
        assert_eq!(workplace.writable(), Some(&[shared][..]));

        let code = "import os, subprocess, sys, tempfile\n\
            def tried(act):\n    try:\n        act()\n        return 'ok'\n    except PermissionError:\n        return 'denied'\n\
            key = '../secret/id_rsa'\n\
            def linked():\n    try:\n        os.link(key, 'k')\n    except OSError:\n        return 'denied'\n    return tried(lambda: open('k').read())\n\
            [tried(lambda: open('../notes.md').read()), tried(lambda: open(key).read()),\n\
             tried(lambda: open('résumé.txt', 'w').write('x')), tried(lambda: tempfile.TemporaryFile().write(b'x')),\n\
             tried(lambda: open('../naïve.txt', 'w').write('x')), tried(lambda: open('../config/x', 'w').write('x')),\n\
             tried(lambda: open('../shared/x', 'w').write('x')),\n\
             tried(lambda: os.rename(os.getcwd(), '../moved')), tried(lambda: os.rename(sys.prefix, sys.prefix + '-moved')),\n\
             tried(lambda: os.listdir('..')), tried(lambda: os.listdir('/')), linked(),\n\
             subprocess.run(['cat', key], capture_output=True).returncode]";
        let logs = evaluated(workplace, code);
        fs::remove_dir_all(&dir).ok();

        let expected =
            "['ok', 'denied', 'ok', 'ok', 'denied', 'denied', 'ok', 'denied', 'denied', \
                        'ok', 'ok', 'denied', 1]\n";
        assert_eq!(logs, expected);
    }

    /// Renaming a folder above a hidden one doesn't uncover it
    #[test]
    fn hidden_moved() {
        let dir = temp_dir("moved");
        let cwd = dir.join("café");
        write_file(&cwd.join("a/b/secret.txt"), b"key");
        let access = Access {
            hidden: vec![cwd.join("a/b")],
            ..Access::default()
        };
        let workplace = opened(&cwd, &dir.join("config"), &access);
        #[cfg(target_os = "macos")]
        assert!(workplace.sandbox.enforced());
        if !workplace.sandbox.enforced() {
            fs::remove_dir_all(&dir).ok();
            return;
        }

        let code = "import os\n\
            def tried(act):\n    try:\n        return act()\n    except PermissionError:\n        return 'denied'\n\
            def moved():\n    os.rename('a', 'moved')\n    return open('moved/b/secret.txt').read()\n\
            [tried(lambda: open('a/b/secret.txt').read()), tried(moved)]";
        let logs = evaluated(workplace, code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(logs, "['denied', 'denied']\n");
    }

    /// A link in a writable folder that holds a hidden path doesn't make where it leads writable
    #[cfg(unix)]
    #[test]
    fn links_not_granted() {
        let dir = temp_dir("linked");
        let cwd = dir.join("café");
        write_file(&cwd.join(".env"), b"key");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).expect("dir");
        symlink(&outside, cwd.join("data")).expect("link");
        let access = Access {
            hidden: vec![cwd.join(".env")],
            ..Access::default()
        };
        let workplace = opened(&cwd, &dir.join("config"), &access);
        #[cfg(target_os = "macos")]
        assert!(workplace.sandbox.enforced());
        if !workplace.sandbox.enforced() {
            fs::remove_dir_all(&dir).ok();
            return;
        }

        let code = "import os\n\
            def tried(act):\n    try:\n        return act()\n    except PermissionError:\n        return 'denied'\n\
            [tried(lambda: open('data/naïve.txt', 'w').write('x')), tried(lambda: open('.env').read()),\n \
             tried(lambda: sorted(os.listdir('.')))]";
        let logs = evaluated(workplace, code);
        let written = outside.join("naïve.txt").exists();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(logs, "['denied', 'denied', ['.env', 'data']]\n");
        assert!(!written);
    }

    /// Sockets and signals reach outside the sandbox only on kernels that can't stop them,
    /// while names still resolve
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn ipc_confined() {
        let dir = temp_dir("ipc");
        let cwd = dir.join("café");
        fs::create_dir_all(&cwd).expect("dir");
        fs::create_dir_all(dir.join("outside")).expect("dir");
        let path = dir.join("outside/s.sock");
        let _outside = UnixListener::bind(&path).expect("bind");
        #[cfg(target_os = "linux")]
        let name = format!("smol-ipc-{}", new_id());
        #[cfg(target_os = "linux")]
        let address = SocketAddr::from_abstract_name(&name).expect("name");
        #[cfg(target_os = "linux")]
        let _scoped = UnixListener::bind_addr(&address).expect("bind");
        let workplace = opened(&cwd, &dir.join("config"), &Access::default());
        #[cfg(target_os = "macos")]
        assert!(workplace.sandbox.enforced());
        if !workplace.sandbox.enforced() {
            fs::remove_dir_all(&dir).ok();
            return;
        }

        let shown = path.display();
        let probes = format!(
            "tried(lambda: reach('own.sock')), tried(lambda: reach({shown:?})),\n \
             tried(lambda: os.kill(os.getppid(), 0)), len(socket.getaddrinfo('localhost', 80)) > 0"
        );
        #[cfg(target_os = "linux")]
        let probes = format!("{probes}, tried(lambda: reach('\\0{name}'))");
        let code = format!(
            "import os, socket\n\
            def tried(act):\n    try:\n        act()\n        return 'ok'\n    except PermissionError:\n        return 'denied'\n\
            def reach(address):\n    socket.socket(socket.AF_UNIX).connect(address)\n\
            own = socket.socket(socket.AF_UNIX)\nown.bind('own.sock')\nown.listen()\n\
            [{probes}]"
        );
        let logs = evaluated(workplace, &code);
        fs::remove_dir_all(&dir).ok();

        #[cfg(target_os = "linux")]
        let expected = {
            let abi = landlock_abi();
            let sockets = match abi >= 9 {
                true => "denied",
                false => "ok",
            };
            let scoped = match abi >= 6 {
                true => "denied",
                false => "ok",
            };
            format!("['ok', '{sockets}', '{scoped}', True, '{scoped}']\n")
        };
        #[cfg(target_os = "macos")]
        let expected = "['ok', 'denied', 'denied', True]\n".to_owned();
        assert_eq!(logs, expected);
    }

    #[test]
    fn env_filtered() {
        let mut unpassed = Vec::new();
        for (name, _) in env::vars_os() {
            let Ok(name) = name.into_string() else {
                continue;
            };
            let passed = PASSED_ENV
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&name));
            // The worker sets VIRTUAL_ENV as it enters the venv
            let set = name.starts_with("SMOL_") || name == "VIRTUAL_ENV" || name == PIP_CACHE_ENV;
            if !passed && !set {
                unpassed.push(name);
            }
        }
        let [allowed, dropped, ..] = &unpassed[..] else {
            return;
        };

        let dir = temp_dir("env");
        let cwd = dir.join("café");
        fs::create_dir_all(&cwd).expect("dir");
        let access = Access {
            env: vec![allowed.clone()],
            ..Access::default()
        };
        let workplace = opened(&cwd, &dir, &access);
        let scratch = workplace.sandbox.temp.join("tmp").display().to_string();
        let cached = match workplace.sandbox.enforced() {
            true => "True",
            false => "False",
        };
        let code = format!(
            "import os, tempfile\n\
             cache = os.environ.get('PIP_CACHE_DIR', '').startswith({scratch:?})\n\
             [name in os.environ for name in ({allowed:?}, {dropped:?})] + [tempfile.gettempdir() == {scratch:?}, cache]"
        );
        let logs = evaluated(workplace, &code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(logs, format!("[True, False, True, {cached}]\n"));
    }

    /// Only folders whose lock nobody holds are removed
    #[test]
    fn stale_swept() {
        let dir = temp_dir("swept");
        let cwd = dir.join("café");
        fs::create_dir_all(&cwd).expect("dir");
        let live = opened(&cwd, &dir, &Access::default());
        let id = new_id();
        let name = format!("{TEMP_PREFIX}{id}");
        let stale = env::temp_dir().join(name);
        fs::create_dir_all(stale.join("tmp")).expect("dir");
        fs::write(stale.join(LOCK), "").expect("lock");

        sweep();
        let kept = [cwd.exists(), live.sandbox.temp.exists(), stale.exists()];
        let temp = live.sandbox.temp.clone();
        drop(live);
        let left = [temp.exists(), dir.join("copies").exists()];
        fs::remove_dir_all(&dir).ok();

        assert_eq!(kept, [true, true, false]);
        assert_eq!(left, [false, false]);
    }
}
