use crate::config::{Access, Skill};
use crate::utils::new_id;
use crate::worker::python::Python;
#[cfg(target_os = "macos")]
use crate::worker::SANDBOX_ENV;
#[cfg(target_os = "linux")]
use landlock::{
    Access as _, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, Scope,
    ABI,
};
use reaper::{cause, tilde};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use std::convert::Infallible;
use std::env;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::c_void;
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::ffi::{c_char, c_int, CStr, CString};
use std::fs::{self, File};
use std::io;
use std::iter;
#[cfg(target_os = "macos")]
use std::mem;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
/// Relative to the home folder
const CREDENTIALS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".config/gh",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".password-store",
    ".local/share/keyrings",
    "Library/Keychains",
    ".smol",
    ".claude",
    ".claude.json",
    ".codex",
];
#[cfg(unix)]
const PASSED_ENV: &[&str] = &[
    "HOME", "USER", "LOGNAME", "PATH", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ",
];
#[cfg(windows)]
const PASSED_ENV: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "PATH",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERNAME",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "CommonProgramFiles",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];
#[cfg(unix)]
const TEMP_ENV: &[&str] = &["TMPDIR"];
#[cfg(windows)]
const TEMP_ENV: &[&str] = &["TEMP", "TMP"];
const VENV_ENV: &str = "VIRTUAL_ENV";

/// The Landlock ruleset the worker restricts itself with
#[cfg(target_os = "linux")]
type Isolation = OwnedFd;
/// The Seatbelt profile the worker confines itself with
#[cfg(target_os = "macos")]
type Isolation = String;
/// `sandbox_init` and `sandbox_free_error` as sandbox.h declares them
#[cfg(target_os = "macos")]
type SandboxInit = unsafe extern "C" fn(*const c_char, u64, *mut *mut c_char) -> c_int;
#[cfg(target_os = "macos")]
type FreeError = unsafe extern "C" fn(*mut c_char);
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
type Isolation = Infallible;

/// What a session's worker and the programs it starts can see, kept across restarts
pub(crate) struct Sandbox {
    cwd: PathBuf,
    pub(crate) copies: PathBuf,
    pub(crate) python: Python,
    temp: PathBuf,
    /// The places in `[allow] write` that existed when the session opened
    writable: Vec<PathBuf>,
    lock: Option<File>,
    env: Vec<(OsString, OsString)>,
    isolation: Option<Isolation>,
}

impl Sandbox {
    /// `config` is hidden like the credential locations,
    /// but `copies`, skill folders and the parts of `python` in it are not
    pub(crate) fn new(
        cwd: &Path,
        copies: &Path,
        config: &Path,
        skills: &[Skill],
        access: &Access,
        python: Python,
    ) -> Result<Self, String> {
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
            cwd: cwd.to_owned(),
            copies: copies.to_owned(),
            python,
            temp: temp.clone(),
            writable: Vec::new(),
            lock: None,
            env: Vec::new(),
            isolation: None,
        };
        let path = temp.join(LOCK);
        let lock = File::create(&path).map_err(|e| failed(&path, e))?;
        lock.lock().map_err(|e| failed(&path, e))?;
        sandbox.lock = Some(lock);
        // A Landlock rule holds on to the folder that exists when the rule is made
        let python = &sandbox.python;
        for folder in [copies, &python.venv] {
            fs::create_dir_all(folder).map_err(|e| failed(folder, e))?;
        }

        let passed = PASSED_ENV.iter().copied();
        let named = access.env.iter().map(String::as_str);
        for name in passed.chain(named) {
            let Some(mut value) = env::var_os(name) else {
                continue;
            };
            // Programs the code starts find the venv's `python` and `pip` first
            if name == "PATH" {
                let paths = iter::once(python.bin()).chain(env::split_paths(&value));
                if let Ok(joined) = env::join_paths(paths) {
                    value = joined;
                }
            }
            sandbox.env.push((OsString::from(name), value));
        }
        for name in TEMP_ENV {
            let value = scratch.clone().into_os_string();
            sandbox.env.push((OsString::from(name), value));
        }
        let venv = python.venv.clone().into_os_string();
        sandbox.env.push((OsString::from(VENV_ENV), venv));

        let mut granted = vec![(PathBuf::from("/"), false)];
        for skill in skills {
            if let Some(folder) = skill.path.parent() {
                granted.push((folder.to_owned(), false));
            }
        }
        for path in [&python.runtime.home, &python.runtime.module] {
            granted.push((path.clone(), false));
        }
        for path in [cwd, &scratch, copies, &python.venv, Path::new("/dev")] {
            granted.push((path.to_owned(), true));
        }
        for path in &access.write {
            granted.push((path.clone(), true));
            if path.exists() {
                sandbox.writable.push(path.clone());
            }
        }
        // Links resolve before the kernel checks a path, so rules compare real paths
        let mut real = Vec::new();
        for (path, write) in granted {
            if let Ok(path) = fs::canonicalize(&path) {
                real.push((path, write));
            }
        }

        let mut hidden = vec![config.to_owned()];
        if let Some(home) = env::home_dir() {
            for name in CREDENTIALS {
                hidden.push(home.join(name));
            }
        }
        hidden.extend(access.hidden.iter().cloned());
        for path in &mut hidden {
            if let Ok(resolved) = fs::canonicalize(&path) {
                *path = resolved;
            }
        }

        sandbox.isolation = isolated(&real, &hidden);
        Ok(sandbox)
    }

    pub(crate) fn enforced(&self) -> bool {
        self.isolation.is_some()
    }

    /// Where code may write besides the working directory and its temporary folder,
    /// or `None` when files aren't isolated
    pub(crate) fn writable(&self) -> Option<&[PathBuf]> {
        self.enforced().then_some(self.writable.as_slice())
    }

    /// The worker's command before its arguments, confined before it runs any code
    pub(crate) fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command.current_dir(&self.cwd).env_clear();
        for (name, value) in &self.env {
            command.env(name, value);
        }

        #[cfg(target_os = "macos")]
        if let Some(profile) = &self.isolation {
            command.env(SANDBOX_ENV, profile);
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
        // Removed only here because the worker's rule is on the folder made at the start
        fs::remove_dir(&self.copies).ok();
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
        spread(path, *write, &hidden, &mut allowed);
    }

    // A kernel drops the rights and scopes it doesn't know, and the rest still holds
    let abi = ABI::V9;
    let all = AccessFs::from_all(abi);
    // Connecting to a Unix socket is a write right, so only writable folders allow it
    let read = AccessFs::from_read(abi);
    let mut rules = Vec::new();
    for (path, write) in allowed {
        let Ok(fd) = PathFd::new(&path) else {
            continue;
        };
        let mut access = match write {
            true => all,
            false => read,
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
/// so a folder holding a hidden path is granted entry by entry
#[cfg(target_os = "linux")]
fn spread(path: &Path, write: bool, hidden: &[PathBuf], allowed: &mut Vec<(PathBuf, bool)>) {
    if hidden.iter().any(|spot| spot == path) {
        return;
    }
    let holds = hidden.iter().any(|spot| spot.starts_with(path));
    if !holds {
        allowed.push((path.to_owned(), write));
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let inner = entry.path();
        let linked = entry.file_type().is_ok_and(|kind| kind.is_symlink());
        if !linked {
            spread(&inner, write, hidden, allowed);
            continue;
        }

        // A rule on a link grants where it leads
        let Ok(real) = fs::canonicalize(&inner) else {
            continue;
        };
        let near = hidden
            .iter()
            .any(|spot| real.starts_with(spot) || spot.starts_with(&real));
        if !near {
            allowed.push((real, write));
        }
    }
}

#[cfg(target_os = "macos")]
fn isolated(granted: &[(PathBuf, bool)], hidden: &[PathBuf]) -> Option<Isolation> {
    // Apple deprecated it, and the worker runs unconfined once it is gone
    if seatbelt().is_none() {
        info!("sandbox: not enforced, sandbox_init not found");
        return None;
    }

    info!("sandbox: enforced (seatbelt)");
    let profile = profile(granted, hidden);
    Some(profile)
}

/// Looked up at run time because linking to them would keep libsmol from loading without them
#[cfg(target_os = "macos")]
fn seatbelt() -> Option<(SandboxInit, FreeError)> {
    // SAFETY: Only looks the symbols up in the libraries already loaded
    let (init, free) = unsafe {
        let init = libc::dlsym(libc::RTLD_DEFAULT, c"sandbox_init".as_ptr());
        let free = libc::dlsym(libc::RTLD_DEFAULT, c"sandbox_free_error".as_ptr());
        (init, free)
    };
    if init.is_null() || free.is_null() {
        return None;
    }

    // SAFETY: Both symbols have the signatures sandbox.h declares
    let init = unsafe { mem::transmute::<*mut c_void, SandboxInit>(init) };
    let free = unsafe { mem::transmute::<*mut c_void, FreeError>(free) };
    Some((init, free))
}

/// Confines this process and every program it starts from now on, which can't be undone
#[cfg(target_os = "macos")]
pub fn confine(profile: &str) -> Result<(), String> {
    let Some((init, free)) = seatbelt() else {
        return Err("sandbox: sandbox_init not found".to_owned());
    };
    let profile = CString::new(profile).map_err(|e| format!("sandbox: {e}"))?;

    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: Flags 0 make the profile its text, and error is set only when it fails
    let status = unsafe { init(profile.as_ptr(), 0, &mut error) };
    if status == 0 {
        return Ok(());
    }

    let reason = match error.is_null() {
        true => format!("sandbox_init returned {status}"),
        false => {
            // SAFETY: The text is NUL-terminated and only sandbox_free_error may free it
            let text = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe { free(error) };
            text
        }
    };
    Err(format!("sandbox: {reason}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn isolated(_: &[(PathBuf, bool)], _: &[PathBuf]) -> Option<Isolation> {
    None
}

/// Seatbelt takes the last rule that matches, so deeper paths come later
/// and a hidden path comes after a granted one of the same depth
#[cfg(any(target_os = "macos", test))]
fn profile(granted: &[(PathBuf, bool)], hidden: &[PathBuf]) -> String {
    let mut rules = Vec::new();
    for (path, write) in granted {
        let rule = match write {
            true => "allow file-read* file-write* network-outbound",
            false => "allow file-read*",
        };
        rules.push((path, rule));
    }
    for path in hidden {
        rules.push((path, "deny file-read* file-write* network-outbound"));
    }
    rules.sort_by_key(|(path, _)| path.components().count());

    // Only connecting to a Unix socket has a path, so TCP and UDP stay open
    let mut profile = "(version 1)\n(allow default)\n(deny file-write*)\n\
        (deny network-outbound (subpath \"/\"))\n"
        .to_owned();
    for (path, rule) in rules {
        let path = path.to_string_lossy();
        let path = path.replace('\\', "\\\\").replace('"', "\\\"");
        let line = format!("({rule} (subpath \"{path}\"))\n");
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
    use crate::worker::host::tests::no_agents;
    use crate::worker::host::Worker;
    use crate::worker::python::tests::installed;
    #[cfg(target_os = "linux")]
    use crate::worker::sandbox::{landlock_abi, spread};
    use crate::worker::sandbox::{
        profile, sweep, Sandbox, LOCK, PASSED_ENV, TEMP_PREFIX, VENV_ENV,
    };
    use crate::worker::Delegation;
    use std::env;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::symlink;
    #[cfg(target_os = "linux")]
    use std::os::unix::net::SocketAddr;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// The config folder is the parent of `copies`
    pub(crate) fn opened(cwd: &Path, copies: &Path) -> Sandbox {
        let config = copies.parent().expect("parent");
        let access = Access::default();
        let python = installed(config, cwd);
        Sandbox::new(cwd, copies, config, &[], &access, python).expect("sandbox")
    }

    #[test]
    fn profile_built() {
        let granted = [
            (PathBuf::from("/Users/zoë/.smol/skills/pdf"), false),
            (PathBuf::from("/"), false),
            (PathBuf::from("/Users/zoë/café"), true),
        ];
        let hidden = [
            PathBuf::from("/Users/zoë/a/b/c/we\"ird\\name"),
            PathBuf::from("/Users/zoë/.ssh"),
            PathBuf::from("/Users/zoë/.smol"),
        ];
        let expected = "(version 1)\n(allow default)\n(deny file-write*)\n\
            (deny network-outbound (subpath \"/\"))\n\
            (allow file-read* (subpath \"/\"))\n\
            (allow file-read* file-write* network-outbound (subpath \"/Users/zoë/café\"))\n\
            (deny file-read* file-write* network-outbound (subpath \"/Users/zoë/.ssh\"))\n\
            (deny file-read* file-write* network-outbound (subpath \"/Users/zoë/.smol\"))\n\
            (allow file-read* (subpath \"/Users/zoë/.smol/skills/pdf\"))\n\
            (deny file-read* file-write* network-outbound (subpath \"/Users/zoë/a/b/c/we\\\"ird\\\\name\"))\n\
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
        let outside = fs::canonicalize(&outside).expect("real");
        let hidden = [home.join(".ssh"), home.join(".config/gh")];

        let mut allowed = Vec::new();
        spread(&home, false, &hidden, &mut allowed);
        spread(&home.join("café"), true, &hidden, &mut allowed);
        spread(&home.join(".ssh"), true, &hidden, &mut allowed);
        allowed.sort();
        fs::remove_dir_all(&dir).ok();

        let expected = [
            (home.join(".config/git"), false),
            (home.join(".gitconfig"), false),
            (home.join("café"), false),
            (home.join("café"), true),
            (outside, false),
        ];
        assert_eq!(allowed, expected);
    }

    fn evaluated(sandbox: Sandbox, code: &str) -> String {
        let interrupt = Interrupt::default();
        let restarted = false;
        let starting = Worker::new(
            &interrupt,
            Arc::new(sandbox),
            &[],
            Delegation::Off,
            restarted,
        );
        let mut worker = block_on(starting).expect("worker");
        let mut ignore = |_: &str| {};
        let mut none = no_agents;
        let evaluating = worker.evaluate(code, &[], &mut ignore, &mut none);
        let output = block_on(evaluating).expect("evaluate");
        assert_eq!(output.error, None, "{}", output.logs);
        output.logs
    }

    /// Programs the code starts are confined too
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
        let config = dir.join("config");
        let copies = config.join("copies");
        let python = installed(&config, &cwd);
        let sandbox = Sandbox::new(&cwd, &copies, &config, &[], &access, python);
        let sandbox = sandbox.expect("sandbox");
        #[cfg(target_os = "macos")]
        assert!(sandbox.enforced());
        if !sandbox.enforced() {
            fs::remove_dir_all(&dir).ok();
            return;
        }
        assert_eq!(sandbox.writable(), Some(&[shared][..]));

        let code = "import os, subprocess, tempfile\n\
            def tried(act):\n    try:\n        act()\n        return 'ok'\n    except PermissionError:\n        return 'denied'\n\
            key = '../secret/id_rsa'\n\
            [tried(lambda: open('../notes.md').read()), tried(lambda: open(key).read()),\n\
             tried(lambda: open('résumé.txt', 'w').write('x')), tried(lambda: tempfile.TemporaryFile().write(b'x')),\n\
             tried(lambda: open('../naïve.txt', 'w').write('x')), tried(lambda: open('../config/x', 'w').write('x')),\n\
             tried(lambda: open('../shared/x', 'w').write('x')),\n\
             subprocess.run(['cat', key], capture_output=True).returncode, 'SMOL_SANDBOX' in os.environ]";
        let logs = evaluated(sandbox, code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(
            logs,
            "['ok', 'denied', 'ok', 'ok', 'denied', 'denied', 'ok', 1, False]\n"
        );
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
        let config = dir.join("config");
        let copies = config.join("copies");
        let python = installed(&config, &cwd);
        let sandbox = Sandbox::new(&cwd, &copies, &config, &[], &Access::default(), python);
        let sandbox = sandbox.expect("sandbox");
        #[cfg(target_os = "macos")]
        assert!(sandbox.enforced());
        if !sandbox.enforced() {
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
        let logs = evaluated(sandbox, &code);
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
            if !passed && !name.starts_with("SMOL_") && name != VENV_ENV {
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
        let copies = dir.join("copies");
        let python = installed(&dir, &cwd);
        let sandbox = Sandbox::new(&cwd, &copies, &dir, &[], &access, python).expect("sandbox");
        let scratch = sandbox.temp.join("tmp").display().to_string();
        let code = format!(
            "import os, tempfile\n[name in os.environ for name in ({allowed:?}, {dropped:?})] + [tempfile.gettempdir() == {scratch:?}]"
        );
        let logs = evaluated(sandbox, &code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(logs, "[True, False, True]\n");
    }

    /// Only folders whose lock nobody holds are removed
    #[test]
    fn stale_swept() {
        let dir = temp_dir("swept");
        let cwd = dir.join("café");
        fs::create_dir_all(&cwd).expect("dir");
        let live = opened(&cwd, &dir.join("copies"));
        let id = new_id();
        let name = format!("{TEMP_PREFIX}{id}");
        let stale = env::temp_dir().join(name);
        fs::create_dir_all(stale.join("tmp")).expect("dir");
        fs::write(stale.join(LOCK), "").expect("lock");

        sweep();
        let kept = [cwd.exists(), live.temp.exists(), stale.exists()];
        let temp = live.temp.clone();
        drop(live);
        let left = [temp.exists(), dir.join("copies").exists()];
        fs::remove_dir_all(&dir).ok();

        assert_eq!(kept, [true, true, false]);
        assert_eq!(left, [false, false]);
    }
}
