use crate::config::{Access, Skill};
use crate::utils::new_id;
#[cfg(target_os = "linux")]
use landlock::{
    Access as _, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};
use reaper::{cause, tilde};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;

const TEMP_PREFIX: &str = "smol-";
/// The sandbox holds a lock on this file in its temporary folder while it lives
const LOCK: &str = "lock";
#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
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

/// The Landlock ruleset the worker restricts itself with
#[cfg(target_os = "linux")]
type Isolation = OwnedFd;
/// The Seatbelt profile sandbox-exec applies
#[cfg(target_os = "macos")]
type Isolation = String;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
type Isolation = Infallible;

/// What a session's worker and the programs it starts can see, kept across restarts
pub(crate) struct Sandbox {
    cwd: PathBuf,
    pub(crate) copies: PathBuf,
    temp: PathBuf,
    lock: Option<File>,
    env: Vec<(OsString, OsString)>,
    isolation: Option<Isolation>,
}

impl Sandbox {
    /// `config` is hidden like the credential locations, but `copies` and skill folders in it are not
    pub(crate) fn new(
        cwd: &Path,
        copies: &Path,
        config: &Path,
        skills: &[Skill],
        access: &Access,
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
            temp: temp.clone(),
            lock: None,
            env: Vec::new(),
            isolation: None,
        };
        let path = temp.join(LOCK);
        let lock = File::create(&path).map_err(|e| failed(&path, e))?;
        lock.lock().map_err(|e| failed(&path, e))?;
        sandbox.lock = Some(lock);
        // A Landlock rule holds on to the folder that exists when the rule is made
        fs::create_dir_all(copies).map_err(|e| failed(copies, e))?;

        let passed = PASSED_ENV.iter().copied();
        let named = access.env.iter().map(String::as_str);
        for name in passed.chain(named) {
            if let Some(value) = env::var_os(name) {
                sandbox.env.push((OsString::from(name), value));
            }
        }
        for name in TEMP_ENV {
            let value = scratch.clone().into_os_string();
            sandbox.env.push((OsString::from(name), value));
        }

        let mut granted = vec![(PathBuf::from("/"), false)];
        for skill in skills {
            if let Some(folder) = skill.path.parent() {
                granted.push((folder.to_owned(), false));
            }
        }
        for path in [cwd, &scratch, copies, Path::new("/dev")] {
            granted.push((path.to_owned(), true));
        }
        for path in &access.write {
            granted.push((path.clone(), true));
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

    /// The worker's command before its arguments, already confined
    pub(crate) fn command(&self, program: &Path) -> Command {
        #[cfg(target_os = "macos")]
        let mut command = match &self.isolation {
            Some(profile) => {
                let mut command = Command::new(SANDBOX_EXEC);
                command.arg("-p").arg(profile).arg(program);
                command
            }
            None => Command::new(program),
        };
        #[cfg(not(target_os = "macos"))]
        let mut command = Command::new(program);

        command.current_dir(&self.cwd).env_clear();
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

    // V9 also covers connecting to Unix sockets, which stay open like the network
    let abi = ABI::V8;
    let all = AccessFs::from_all(abi);
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
        .and_then(Ruleset::create)
        .and_then(|ruleset| ruleset.add_rules(rules));
    let ruleset: Option<OwnedFd> = match created {
        Ok(ruleset) => ruleset.into(),
        Err(e) => {
            warn!("sandbox: not enforced, {e}");
            return None;
        }
    };
    match ruleset {
        Some(_) => info!("sandbox: enforced (landlock)"),
        None => info!("sandbox: not enforced, landlock unavailable"),
    }
    ruleset
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
    if !Path::new(SANDBOX_EXEC).exists() {
        info!("sandbox: not enforced, {SANDBOX_EXEC} not found");
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
/// and a hidden path comes after a granted one of the same depth
#[cfg(any(target_os = "macos", test))]
fn profile(granted: &[(PathBuf, bool)], hidden: &[PathBuf]) -> String {
    let mut rules = Vec::new();
    for (path, write) in granted {
        let rule = match write {
            true => "allow file-read* file-write*",
            false => "allow file-read*",
        };
        rules.push((path, rule));
    }
    for path in hidden {
        rules.push((path, "deny file-read* file-write*"));
    }
    rules.sort_by_key(|(path, _)| path.components().count());

    let mut profile = "(version 1)\n(allow default)\n(deny file-write*)\n".to_owned();
    for (path, rule) in rules {
        let path = path.to_string_lossy();
        let path = path.replace('\\', "\\\\").replace('"', "\\\"");
        let line = format!("({rule} (subpath \"{path}\"))\n");
        profile.push_str(&line);
    }
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
    use crate::code::Worker;
    use crate::config::Access;
    #[cfg(target_os = "linux")]
    use crate::sandbox::spread;
    use crate::sandbox::{profile, sweep, Sandbox, LOCK, PASSED_ENV, TEMP_PREFIX};
    use crate::tests::{temp_dir, write_file};
    use crate::utils::{block_on, new_id, Interrupt};
    use std::env;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    /// The config folder is the parent of `copies`
    pub(crate) fn opened(cwd: &Path, copies: &Path) -> Sandbox {
        let config = copies.parent().expect("parent");
        let access = Access::default();
        Sandbox::new(cwd, copies, config, &[], &access).expect("sandbox")
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
            (allow file-read* (subpath \"/\"))\n\
            (allow file-read* file-write* (subpath \"/Users/zoë/café\"))\n\
            (deny file-read* file-write* (subpath \"/Users/zoë/.ssh\"))\n\
            (deny file-read* file-write* (subpath \"/Users/zoë/.smol\"))\n\
            (allow file-read* (subpath \"/Users/zoë/.smol/skills/pdf\"))\n\
            (deny file-read* file-write* (subpath \"/Users/zoë/a/b/c/we\\\"ird\\\\name\"))\n";
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
        let starting = Worker::new(&interrupt, sandbox, false);
        let mut worker = block_on(starting).expect("worker");
        let mut ignore = |_: &str| {};
        let evaluating = worker.evaluate(code, &[], &mut ignore);
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
        let access = Access {
            hidden: vec![dir.join("secret")],
            ..Access::default()
        };
        let config = dir.join("config");
        let copies = config.join("copies");
        let sandbox = Sandbox::new(&cwd, &copies, &config, &[], &access).expect("sandbox");
        if !sandbox.enforced() {
            fs::remove_dir_all(&dir).ok();
            return;
        }

        let code = "import subprocess, tempfile\n\
            def tried(act):\n    try:\n        act()\n        return 'ok'\n    except PermissionError:\n        return 'denied'\n\
            key = '../secret/id_rsa'\n\
            [tried(lambda: open('../notes.md').read()), tried(lambda: open(key).read()),\n\
             tried(lambda: open('résumé.txt', 'w').write('x')), tried(lambda: tempfile.TemporaryFile().write(b'x')),\n\
             tried(lambda: open('../naïve.txt', 'w').write('x')), tried(lambda: open('../config/x', 'w').write('x')),\n\
             subprocess.run(['cat', key], capture_output=True).returncode]";
        let logs = evaluated(sandbox, code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(
            logs,
            "['ok', 'denied', 'ok', 'ok', 'denied', 'denied', 1]\n"
        );
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
            if !passed && !name.starts_with("SMOL_") {
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
        let sandbox = Sandbox::new(&cwd, &copies, &dir, &[], &access).expect("sandbox");
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
