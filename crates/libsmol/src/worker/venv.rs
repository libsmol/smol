use crate::history::VENV;
use reaper::{cause, tilde};
#[cfg(test)]
use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::task;
use tracing::{info, Span};

/// How long the venv of a directory that is gone stays unused before it is removed
const MIN_IDLE: Duration = Duration::from_hours(7 * 24);

/// The venv kept for a session's working directory, whose contents the worker makes as it enters it.
/// The agent never reads or follows anything in it, since the code can write it
pub(crate) struct Venv {
    /// Where the code installs packages
    pub(crate) path: PathBuf,
    /// Shared, and naming the working directory so the venv goes once that is gone
    _held: File,
}

impl Venv {
    /// Also removes in the background the venvs of the projects next to `project`
    /// whose working directories are gone, once no session used them for `MIN_IDLE`
    pub(crate) async fn open(project: &Path, cwd: &Path) -> Result<Self, String> {
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(project);
            format!("{shown}: {reason}")
        };
        fs::create_dir_all(project).map_err(failed)?;

        // A working directory that isn't UTF-8 can't be named, and its venv stays
        let record = cwd.to_str().unwrap_or_default();
        let venv = venv_in(project);
        let hold = held(&venv, record).await?;

        if let Some(projects) = project.parent() {
            let projects = projects.to_owned();
            let own = lock_of(&venv);
            let span = Span::current();
            // Tokio runs every session on one thread, which removing a large venv would hold for seconds
            let sweeping = move || {
                let _entered = span.enter();
                sweep_venvs(&projects, &own);
            };
            task::spawn_blocking(sweeping);
        }

        Ok(Self {
            path: venv,
            _held: hold,
        })
    }
}

#[cfg(not(test))]
fn venv_in(project: &Path) -> PathBuf {
    project.join(VENV)
}

/// Tests share one venv in the profile folder
#[cfg(test)]
fn venv_in(_: &Path) -> PathBuf {
    let exe = env::current_exe().expect("test binary");
    let deps = exe.parent().expect("deps");
    let built = deps.parent().expect("profile");
    built.join(VENV)
}

/// Removes the venvs of the projects whose working directories are gone,
/// once no session used them for `MIN_IDLE`, and then the projects left empty
fn sweep_venvs(projects: &Path, own: &Path) {
    let Ok(entries) = fs::read_dir(projects) else {
        return;
    };
    for entry in entries.flatten() {
        let project = entry.path();
        let venv = project.join(VENV);
        let lock = lock_of(&venv);
        if lock == own {
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
        // A drive or share that is away reads as gone too, so only a lock unused for `MIN_IDLE` counts
        let modified = file.metadata().and_then(|found| found.modified());
        let idle = modified.ok().and_then(|time| time.elapsed().ok());
        if !idle.is_some_and(|idle| idle > MIN_IDLE) {
            continue;
        }

        // The lock goes only with the venv, or a venv left behind would have none
        let removed = match fs::remove_dir_all(&venv) {
            Ok(()) => true,
            Err(e) => e.kind() == io::ErrorKind::NotFound,
        };
        if !removed {
            continue;
        }
        // Windows removes no file that is still open
        drop(file);
        fs::remove_file(&lock).ok();
        fs::remove_dir(&project).ok();
        let shown = tilde(&venv);
        info!("python: venv removed [path={shown}]");
    }
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
    use crate::history::VENV;
    use crate::tests::temp_dir;
    use crate::utils::block_on;
    use crate::worker::venv::{held, lock_of, sweep_venvs, Venv, MIN_IDLE};
    use std::fs::{self, File};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    /// The venv tests share, opened for a project in `config`
    pub(crate) fn shared_venv(config: &Path, cwd: &Path) -> Venv {
        let project = config.join("project");
        let opening = Venv::open(&project, cwd);
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
        let projects = config.join("projects");
        let here = config.join("naïve");
        let gone = config.join("café").display().to_string();
        let now = SystemTime::now();
        let aged = now - MIN_IDLE - Duration::from_secs(60);
        fs::create_dir_all(&here).expect("here");
        let here = here.display().to_string();

        let venvs_made = [
            ("unused", &gone, aged),
            ("saved", &gone, aged),
            ("recent", &gone, now),
            ("present", &here, aged),
            ("held", &gone, aged),
            ("own", &gone, aged),
        ];
        for (name, record, time) in venvs_made {
            let venv = projects.join(name).join(VENV);
            fs::create_dir_all(&venv).expect("venv");
            let lock = lock_of(&venv);
            dated(&lock, record, time);
        }
        let session = projects.join("saved").join("s1.json");
        fs::write(session, "{}").expect("session");
        let lock = lock_of(&projects.join("held").join(VENV));
        let holding = File::open(lock).expect("open");
        holding.lock_shared().expect("hold");

        let own = lock_of(&projects.join("own").join(VENV));
        sweep_venvs(&projects, &own);
        let mut left: Vec<String> = Vec::new();
        for project in fs::read_dir(&projects).expect("projects").flatten() {
            let named = project.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(project.path()).expect("project").flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                left.push(format!("{named}/{name}"));
            }
            left.push(named);
        }
        left.sort();
        drop(holding);
        fs::remove_dir_all(&config).ok();

        let expected = [
            "held",
            "held/.venv",
            "held/.venv.lock",
            "own",
            "own/.venv",
            "own/.venv.lock",
            "present",
            "present/.venv",
            "present/.venv.lock",
            "recent",
            "recent/.venv",
            "recent/.venv.lock",
            "saved",
            "saved/s1.json",
        ];
        assert_eq!(left, expected);
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
}
