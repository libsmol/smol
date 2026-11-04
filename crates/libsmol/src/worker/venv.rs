use crate::utils::folded;
use reaper::{cause, tilde};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::env;
use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::task;
use tracing::{info, Span};

/// Holds the venvs and their locks in the config directory
const VENVS: &str = "venvs";
/// Without long paths Windows refuses paths over 260 characters, and packages nest deep in a venv
const MAX_NAME_CHARS: usize = 16;

/// How long the venv of a directory that is gone stays unused before it is removed
const MIN_IDLE: Duration = Duration::from_hours(7 * 24);

/// The venv kept for a session's working directory, which the worker makes and completes as it
/// enters it. The agent never reads or follows anything in it, since the code can write it
pub(crate) struct Venv {
    /// Where the code installs packages
    pub(crate) path: PathBuf,
    /// Shared, and naming the working directory so the venv goes once that is gone
    _held: File,
}

impl Venv {
    /// Also removes in the background the venvs of working directories that are gone,
    /// once no session used them for `MIN_IDLE`
    pub(crate) async fn open(config: &Path, cwd: &Path) -> Result<Self, String> {
        let venvs = venvs_of(config);
        let venv = venv_for(&venvs, cwd);
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(&venvs);
            format!("{shown}: {reason}")
        };
        fs::create_dir_all(&venvs).map_err(failed)?;

        // A working directory that isn't UTF-8 can't be named, and its venv stays
        let record = cwd.to_str().unwrap_or_default();
        let hold = held(&venv, record).await?;

        let own = lock_of(&venv);
        let swept = venvs.clone();
        let span = Span::current();
        // Tokio runs every session on one thread, which removing a large venv would hold for seconds
        let sweeping = move || {
            let _entered = span.enter();
            sweep_venvs(&swept, &own);
        };
        task::spawn_blocking(sweeping);

        Ok(Self {
            path: venv,
            _held: hold,
        })
    }
}

#[cfg(not(test))]
fn venvs_of(config: &Path) -> PathBuf {
    config.join(VENVS)
}

/// Tests share the venvs in the profile folder
#[cfg(test)]
fn venvs_of(_: &Path) -> PathBuf {
    let exe = env::current_exe().expect("test binary");
    let deps = exe.parent().expect("deps");
    let built = deps.parent().expect("profile");
    built.join(VENVS)
}

/// Named after the working directory
#[cfg(not(test))]
fn venv_for(venvs: &Path, cwd: &Path) -> PathBuf {
    let name = venv_name(cwd);
    venvs.join(name)
}

/// Tests share one venv
#[cfg(test)]
fn venv_for(venvs: &Path, _: &Path) -> PathBuf {
    venvs.join("venv")
}

/// The folder's name and a hash of the whole path, both as `folded` spells them
fn venv_name(cwd: &Path) -> String {
    let cwd = PathBuf::from(folded(cwd));
    let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
    let hash = hex(&digest);
    let named = cwd.file_name().unwrap_or_default().to_string_lossy();
    let named: String = named.chars().take(MAX_NAME_CHARS).collect();
    format!("{named}-{}", &hash[..16])
}

/// Removes the venvs of working directories that are gone, once no session used them for `MIN_IDLE`
fn sweep_venvs(venvs: &Path, own: &Path) {
    let Ok(entries) = fs::read_dir(venvs) else {
        return;
    };
    for entry in entries.flatten() {
        let lock = entry.path();
        let locking = lock.extension().is_some_and(|ext| ext == "lock");
        if lock == own || !locking {
            continue;
        }

        let recorded = fs::read_to_string(&lock).unwrap_or_default();
        // One that can't be looked up now is not gone
        let looked = Path::new(&recorded).try_exists();
        if recorded.is_empty() || !matches!(looked, Ok(false)) {
            continue;
        }

        let Ok(file) = File::open(&lock) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        // A drive or share that is away reads as gone too
        if !unused(file.metadata()) {
            continue;
        }

        // The lock goes only with the venv, or a venv left behind would have none
        let folder = lock.with_extension("");
        let removed = match fs::remove_dir_all(&folder) {
            Ok(()) => true,
            Err(e) => e.kind() == io::ErrorKind::NotFound,
        };
        if !removed {
            continue;
        }
        // Windows removes no file that is still open
        drop(file);
        fs::remove_file(&lock).ok();
        let shown = tilde(&folder);
        info!("python: venv removed [path={shown}]");
    }
}

/// Last used, by the time a session last took its lock, more than `MIN_IDLE` ago
fn unused(found: io::Result<Metadata>) -> bool {
    let modified = found.and_then(|found| found.modified());
    let idle = modified.ok().and_then(|time| time.elapsed().ok());
    idle.is_some_and(|idle| idle > MIN_IDLE)
}

fn hex(bytes: &[u8]) -> String {
    let digits: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    digits.concat()
}

/// Takes the venv's lock shared and dates it, which says when a session last used the venv
async fn held(venv: &Path, record: &str) -> Result<File, String> {
    let path = lock_of(venv);
    let shown = tilde(&path);
    let record = record.to_owned();
    let opening = move || {
        // Never truncated, since Windows refuses to write a file that another session holds
        let mut file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        // Written into a lock that is still empty, before any lock forbids it on Windows.
        // A record that can't be written only keeps the venv from being cleaned up
        if !record.is_empty() && file.metadata()?.len() == 0 {
            file.write_all(record.as_bytes()).ok();
        }
        // A file system without locks only loses the protection, as the history does
        file.lock_shared().ok();
        io::Result::Ok(file)
    };
    // Off the runtime's thread, since it waits while another session removes the venv
    let opened = task::spawn_blocking(opening).await.expect("lock");
    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("{shown}: {reason}")
    };
    let file = opened.map_err(failed)?;

    // Windows allows changing the time of a file that other sessions hold
    file.set_modified(SystemTime::now()).ok();
    Ok(file)
}

/// Next to the venv, hidden from the code with the config directory, of which only the venv is granted
fn lock_of(venv: &Path) -> PathBuf {
    let mut path = venv.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::tests::temp_dir;
    use crate::utils::{block_on, new_id};
    use crate::worker::venv::{held, lock_of, sweep_venvs, venv_name, Venv, MIN_IDLE};
    use std::env;
    use std::fs::{self, File};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// The venv tests share
    pub(crate) fn shared_venv(config: &Path, cwd: &Path) -> Venv {
        let opening = Venv::open(config, cwd);
        block_on(opening).expect("venv")
    }

    /// A file holding the text, last changed at the time
    fn dated(path: &Path, text: &str, time: SystemTime) {
        fs::write(path, text).expect("write");
        let file = File::options().write(true).open(path).expect("open");
        file.set_modified(time).expect("date");
    }

    #[test]
    fn venvs_swept() {
        let config = temp_dir("venvs");
        let venvs = config.join("venvs");
        let here = config.join("naïve");
        let gone = config.join("café").display().to_string();
        let now = SystemTime::now();
        let aged = now - MIN_IDLE - Duration::from_secs(60);
        fs::create_dir_all(&here).expect("here");
        let here = here.display().to_string();

        let venvs_made = [
            ("unused", &gone, aged),
            ("recent", &gone, now),
            ("present", &here, aged),
            ("held", &gone, aged),
            ("own", &gone, aged),
        ];
        for (name, record, time) in venvs_made {
            let venv = venvs.join(name);
            fs::create_dir_all(&venv).expect("venv");
            let lock = lock_of(&venv);
            dated(&lock, record, time);
        }
        let holding = File::open(venvs.join("held.lock")).expect("open");
        holding.lock_shared().expect("hold");

        let own = venvs.join("own.lock");
        sweep_venvs(&venvs, &own);
        let mut left: Vec<String> = Vec::new();
        for entry in fs::read_dir(&venvs).expect("venvs").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            left.push(name);
        }
        left.sort();
        drop(holding);
        fs::remove_dir_all(&config).ok();

        let expected = [
            "held",
            "held.lock",
            "own",
            "own.lock",
            "present",
            "present.lock",
            "recent",
            "recent.lock",
        ];
        assert_eq!(left, expected);
    }

    /// The test binary as python, for programs tests start outside the sandbox.
    /// Tests run in parallel, so a stale link is replaced by a rename
    #[cfg(unix)]
    pub(crate) fn interpreter() -> PathBuf {
        let exe = env::current_exe().expect("test binary");
        let deps = exe.parent().expect("deps");
        let linked = deps.join("python3");
        if fs::read_link(&linked).ok().as_ref() != Some(&exe) {
            let id = new_id();
            let aside = deps.join(format!(".python3-{id}"));
            symlink(&exe, &aside).expect("link");
            fs::rename(&aside, &linked).expect("link");
        }
        linked
    }

    /// Windows makes links only with a privilege, so the test binary is linked hard or copied
    #[cfg(windows)]
    pub(crate) fn interpreter() -> PathBuf {
        let exe = env::current_exe().expect("test binary");
        let deps = exe.parent().expect("deps");
        let named = deps.join("python3.exe");
        let id = new_id();
        let aside = deps.join(format!(".python3-{id}.exe"));
        if fs::hard_link(&exe, &aside).is_err() {
            fs::copy(&exe, &aside).expect("copy");
        }
        // Another test may be running the one already there
        fs::rename(&aside, &named).ok();
        named
    }

    #[test]
    fn marks_use() {
        let dir = temp_dir("held");
        fs::create_dir_all(&dir).expect("dir");
        let venv = dir.join("venv");
        let holding = held(&venv, "");
        let first = block_on(holding).expect("hold");
        first.set_modified(SystemTime::UNIX_EPOCH).expect("age");

        // Another session takes it while the first still holds it
        let holding = held(&venv, "");
        let second = block_on(holding).expect("hold");
        let lock = lock_of(&venv);
        let used = fs::metadata(lock).and_then(|found| found.modified());
        let used = used.expect("time");
        assert!(used > SystemTime::UNIX_EPOCH, "{used:?}");

        drop((first, second));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn venvs_named() {
        let name = venv_name(Path::new("/home/zoë/naïve café"));
        assert!(name.starts_with("naïve café-"), "{name}");
        assert_eq!(name.chars().count(), "naïve café-".chars().count() + 16);

        let long = "🐍".repeat(100);
        let path = Path::new("/home").join(&long);
        let name = venv_name(&path);
        assert_eq!(name.chars().count(), 16 + 1 + 16, "{name}");
        assert_ne!(
            venv_name(Path::new("/a/work")),
            venv_name(Path::new("/b/work"))
        );
        assert_eq!(
            venv_name(Path::new("/a/./work/")),
            venv_name(Path::new("/a/work"))
        );
        #[cfg(windows)]
        assert_eq!(
            venv_name(Path::new(r"C:\Users\Zoë\Café")),
            venv_name(Path::new(r"c:\users\zoë\CAFÉ"))
        );
    }
}
