use crate::memory::Exit;
use crate::spawn::{drain, Clip, Group, Piece, KEPT_BYTES};
use crate::utils::{folded, http_cause, http_client, new_id};
use crate::worker::sandbox::Sandbox;
use flate2::read::GzDecoder;
use reaper::{cause, tilde};
use reqwest::Client;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::env;
#[cfg(test)]
use std::env::consts::DLL_PREFIX;
use std::env::consts::{ARCH, DLL_SUFFIX, OS};
use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tar::Archive;
use tokio::process::Command;
use tokio::select;
use tokio::task;
use tracing::{info, Span};

/// The python-build-standalone release the worker runs
const RELEASE: &str = "20261026";
pub(crate) const PYTHON: &str = "3.14.10";
/// Rust and python-build-standalone name targets alike,
/// and each has the sha256 of its `install_only_stripped` archive.
/// release.yml reads the targets from here, each a quoted triple on a line of its own
const BUILDS: [(&str, &str); 5] = [
    (
        "x86_64-unknown-linux-gnu",
        "4d3c16c59ed0802a3d1d34d8bce504b15ceeaff39d62dadecc6a9fa803412aed",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "90f48337bc3dd4ae6eda81e001e4c981236d403b6b5a07778bbdf94ee423dc00",
    ),
    (
        "x86_64-apple-darwin",
        "2fd8becd48cf0ff5f0a15df3c0db7cae7aa708dcaacf901bd30d6cdd6229370b",
    ),
    (
        "aarch64-apple-darwin",
        "95eb8c87d16af37c54e9765e31d8c56c0d8ffa9968ea0f704f73a9b83f84e9f9",
    ),
    (
        "x86_64-pc-windows-msvc",
        "c1876a9795d3a94ee49a1499a69cd91e26ca3e2829593f93fa6e4db90d05402b",
    ),
];
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Holds the runtimes, the worker modules, the venvs and their locks in the config directory
const STORE: &str = "runtime";
/// Holds a folder of venvs for each runtime in the store
const VENVS: &str = "venvs";
/// Held by an install or a repair in the store
const STORE_LOCK: &str = "lock";
/// Without long paths Windows refuses paths over 260 characters, and packages nest deep in a venv
const MAX_NAME_CHARS: usize = 16;

const DOWNLOAD_WAIT: Duration = Duration::from_secs(30);
/// How long a runtime, a module or the venv of a directory that is gone stays unused before it is removed
const MIN_IDLE: Duration = Duration::from_hours(7 * 24);

#[cfg(unix)]
const HOME_BIN: &str = "bin";
#[cfg(windows)]
const HOME_BIN: &str = "";
/// Where pip puts the scripts it installs, in a venv and in the runtime
#[cfg(unix)]
const SCRIPTS: &str = "bin";
#[cfg(windows)]
const SCRIPTS: &str = "Scripts";
#[cfg(unix)]
const EXE: &str = "python";
#[cfg(windows)]
const EXE: &str = "python.exe";
#[cfg(unix)]
const PIP: &str = "pip";
#[cfg(windows)]
const PIP: &str = "pip.exe";
/// The runtime's own site-packages, removed since site at start would put it ahead of the venv's
#[cfg(unix)]
const SITE: &str = "lib/python3.14/site-packages";
#[cfg(windows)]
const SITE: &str = "Lib/site-packages";

/// Loads the worker module from `sys.argv[1]` whatever its file name,
/// and calls the function named in `sys.argv[2]` with the venv's interpreter
const LOADER: &str = "import sys
from importlib.machinery import ExtensionFileLoader
from importlib.util import module_from_spec, spec_from_loader
loader = ExtensionFileLoader('_smol', sys.argv[1])
module = module_from_spec(spec_from_loader('_smol', loader))
loader.exec_module(module)
getattr(module, sys.argv[2])(sys.argv[3])";

/// The interpreter and the worker module of this libsmol version, kept in place while held
pub(crate) struct Runtime {
    store: PathBuf,
    /// The interpreter and its standard library, which the code only reads
    pub(crate) home: PathBuf,
    /// The worker module, which the code only reads
    pub(crate) module: PathBuf,
    /// Shared locks, which keep other libsmol versions from removing them
    _held: [File; 2],
}

impl Runtime {
    /// Downloads what is missing, and removes in the background what no session used for `MIN_IDLE`
    pub(crate) async fn install(config: &Path) -> Result<Self, String> {
        let system = match OS {
            "linux" if cfg!(target_env = "gnu") => "unknown-linux-gnu",
            "macos" => "apple-darwin",
            "windows" => "pc-windows-msvc",
            other => other,
        };
        let target = format!("{ARCH}-{system}");
        let built = BUILDS.into_iter().find(|(name, _)| *name == target);
        let Some((target, sha)) = built else {
            return Err(format!("{target}: no build"));
        };

        let (store, module) = stored(config);
        let named = format!("cpython-{PYTHON}+{RELEASE}");
        let home = store.join(named);
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(&store);
            format!("{shown}: {reason}")
        };
        fs::create_dir_all(&store).map_err(failed)?;

        // Taken before looking, which waits out a removal under way
        let home_held = held(&home, "").await?;
        let module_held = held(&module, "").await?;
        let holds = [home_held, module_held];

        let swept = store.clone();
        let current = [home.clone(), module.clone()];
        let venvs = store.join(VENVS);
        let span = Span::current();
        // Tokio runs every session on one thread, which removing a runtime and its venvs would hold
        let sweeping = move || {
            let _entered = span.enter();
            sweep_store(&swept, &current, &venvs);
        };
        task::spawn_blocking(sweeping);

        if home.exists() && module.exists() {
            return Ok(Self {
                store,
                home,
                module,
                _held: holds,
            });
        }

        let _lock = locked(&store).await?;
        let builder = Client::builder().read_timeout(DOWNLOAD_WAIT);
        let client = http_client(builder)?;

        if !home.exists() {
            info!("python: downloading runtime [version={PYTHON}+{RELEASE}]");
            let name = format!("cpython-{PYTHON}+{RELEASE}-{target}-install_only_stripped.tar.gz");
            let url = format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{RELEASE}/{name}"
            );
            let archive = fetched(&client, &url, &name, sha).await?;

            // Inside the store, because a rename moves a folder only within one file system
            let id = new_id();
            let folder = store.join(format!(".{id}"));
            let unpacked = folder.join("python");
            let into = folder.clone();
            let trimmed = unpacked.clone();
            let unpacking = move || {
                Archive::new(GzDecoder::new(&archive[..])).unpack(into)?;
                // Venvs get pip from the wheel ensurepip keeps, and nothing reads manuals or terminfo
                if let Ok(entries) = fs::read_dir(trimmed.join(SCRIPTS)) {
                    for entry in entries.flatten() {
                        if entry.file_name().to_string_lossy().starts_with("pip") {
                            fs::remove_file(entry.path()).ok();
                        }
                    }
                }
                fs::remove_dir_all(trimmed.join(SITE)).ok();
                fs::remove_dir_all(trimmed.join("share")).ok();
                io::Result::Ok(())
            };
            let result = task::spawn_blocking(unpacking).await.expect("unpack");
            result.map_err(failed)?;

            // Compiled by timestamp before anything runs it, since the code can't write its bytecode
            // later and SOURCE_DATE_EPOCH would have every import hash its source
            let python = unpacked.join(HOME_BIN).join(EXE);
            let mut compiling = Command::new(python);
            compiling
                .args(["-I", "-m", "compileall", "-q", "-j0"])
                .args(["--invalidation-mode", "timestamp"])
                .arg(&unpacked);
            finished(compiling).await.map_err(failed)?;
            fs::rename(&unpacked, &home).map_err(failed)?;
            fs::remove_dir(&folder).ok();
            info!("python: runtime installed [version={PYTHON}+{RELEASE}]");
        }

        if !module.exists() {
            // Tests load the module cargo built, never a released one
            if cfg!(test) {
                let shown = module.display();
                return Err(format!("{shown}: not built"));
            }

            info!("python: downloading worker module [version={VERSION}]");
            let base = format!("https://github.com/libsmol/smol/releases/download/v{VERSION}");
            let url = format!("{base}/SHA256SUMS");
            let sums = download(&client, &url).await?;
            let sums = String::from_utf8_lossy(&sums);
            let name = format!("smol-worker-{target}");
            let Some(sha) = summed(&sums, &name) else {
                return Err(format!("{name}: not in SHA256SUMS"));
            };
            let url = format!("{base}/{name}");
            let bytes = fetched(&client, &url, &name, sha).await?;

            let id = new_id();
            let written = store.join(format!(".{id}"));
            let mut file = File::create(&written).map_err(failed)?;
            file.write_all(&bytes).map_err(failed)?;
            // A crash could otherwise leave an empty module under the name that means installed
            file.sync_all().map_err(failed)?;
            fs::rename(&written, &module).map_err(failed)?;
            info!("python: worker module installed [version={VERSION}]");
        }

        Ok(Self {
            store,
            home,
            module,
            _held: holds,
        })
    }

    pub(crate) fn interpreter(&self) -> PathBuf {
        self.home.join(HOME_BIN).join(EXE)
    }
}

/// The Python a session's worker runs: the runtime, as the venv kept for the working directory
pub(crate) struct Python {
    pub(crate) runtime: Runtime,
    /// Where the code installs packages
    pub(crate) venv: PathBuf,
    /// Shared, and naming the working directory so the venv goes once that is gone
    _held: File,
}

impl Python {
    /// Also removes in the background the venvs of working directories that are gone,
    /// once no session used them for `MIN_IDLE`
    pub(crate) async fn install(config: &Path, cwd: &Path) -> Result<Self, String> {
        let runtime = Runtime::install(config).await?;
        let named = runtime.home.file_name().unwrap_or_default();
        let named = named.to_string_lossy();
        let venv = venv_for(&runtime.store, cwd, &named);
        let venvs = venv.parent().expect("venvs");
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(venvs);
            format!("{shown}: {reason}")
        };
        fs::create_dir_all(venvs).map_err(failed)?;

        // A working directory that isn't UTF-8 can't be named, and its venv stays
        let record = cwd.to_str().unwrap_or_default();
        let hold = held(&venv, record).await?;

        let own = lock_of(&venv);
        let swept = venvs.to_owned();
        let span = Span::current();
        // Tokio runs every session on one thread, which removing a large venv would hold for seconds
        let sweeping = move || {
            let _entered = span.enter();
            sweep_venvs(&swept, &own);
        };
        task::spawn_blocking(sweeping);

        Ok(Self {
            runtime,
            venv,
            _held: hold,
        })
    }

    /// Holds `python` and `pip`, which programs the code starts find first on `PATH`
    pub(crate) fn bin(&self) -> PathBuf {
        self.venv.join(SCRIPTS)
    }

    /// Complete once `venv` has installed pip, which it does after the interpreter.
    /// Only looked up, never read or followed, since the code can write the venv
    fn ready(&self) -> bool {
        let bin = self.bin();
        let found = |name: &str| fs::symlink_metadata(bin.join(name)).is_ok();
        found(EXE) && found(PIP)
    }
}

/// The worker's command, with the venv made or completed first when it lacks `python` or `pip`
pub(crate) async fn command(sandbox: &Sandbox) -> Result<Command, String> {
    let python = &sandbox.python;
    let runtime = &python.runtime;
    let venv = python.bin().join(EXE);

    if !python.ready() {
        // Another session in this directory may have made it while this one waited
        let _lock = locked(&runtime.store).await?;
        if !python.ready() {
            let mut repairing = loading(sandbox, "repair", &venv);
            repairing.stdin(Stdio::null());
            let shown = tilde(&python.venv);
            let failed = |e: io::Error| {
                let reason = cause(&e);
                format!("{shown}: {reason}")
            };
            let (mut group, mut pieces) = Group::start(repairing).map_err(failed)?;

            let mut output = Clip::new(KEPT_BYTES);
            // Read while it runs, since a full pipe would stall it
            let exit = loop {
                select! {
                    exit = group.exited() => break exit,
                    Some(piece) = pieces.recv() => {
                        if let Piece::Output(bytes) = piece {
                            output.push(&bytes);
                        }
                    }
                }
            };
            drain(&mut pieces, &mut output).await;

            if exit != Some(Exit::Exited(0)) {
                let (said, _) = output.text();
                // The last line, where a traceback names its exception, keeps the error on one line
                let last = said
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty());
                let reason = match (last, exit) {
                    (Some(last), _) => last.to_owned(),
                    (None, Some(exit)) => exit.to_string(),
                    (None, None) => String::new(),
                };
                return Err(format!("{shown}: {reason}"));
            }
            info!("python: venv made [path={shown}]");
        }
    }

    let serving = loading(sandbox, "serve", &venv);
    Ok(serving)
}

/// Starts the runtime's interpreter, which the code can't write, unlike the venv's.
/// Its site at start reads only the runtime, since `-I` leaves out the working directory,
/// the user's site and the environment, and multiprocessing passes these flags on
fn loading(sandbox: &Sandbox, function: &str, venv: &Path) -> Command {
    let runtime = &sandbox.python.runtime;
    let interpreter = runtime.interpreter();
    let mut command = sandbox.command(&interpreter);
    command
        .args(["-I", "-B", "-X", "utf8", "-c", LOADER])
        .arg(&runtime.module)
        .arg(function)
        .arg(venv);
    command
}

/// The store and the worker module
#[cfg(not(test))]
fn stored(config: &Path) -> (PathBuf, PathBuf) {
    let store = config.join(STORE);
    let module = format!("worker-{VERSION}{DLL_SUFFIX}");
    let module = store.join(module);
    (store, module)
}

/// Tests share the store in the profile folder and load the module `cargo test` built in `deps`,
/// which only `cargo build` copies up
#[cfg(test)]
fn stored(_: &Path) -> (PathBuf, PathBuf) {
    let exe = env::current_exe().expect("test binary");
    let deps = exe.parent().expect("deps");
    let built = deps.parent().expect("profile");
    let module = format!("{DLL_PREFIX}_smol{DLL_SUFFIX}");
    (built.join(STORE), deps.join(module))
}

/// Among the venvs of the runtime, named after the working directory
#[cfg(not(test))]
fn venv_for(store: &Path, cwd: &Path, runtime: &str) -> PathBuf {
    let name = venv_name(cwd);
    store.join(VENVS).join(runtime).join(name)
}

/// Tests share one venv in the store of the profile folder
#[cfg(test)]
fn venv_for(store: &Path, _: &Path, runtime: &str) -> PathBuf {
    store.join(VENVS).join(runtime).join("venv")
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

/// Removes the runtimes and modules no session used for `MIN_IDLE`, with each runtime's venvs,
/// and what stopped installs left
fn sweep_store(store: &Path, current: &[PathBuf], venvs: &Path) {
    let Ok(entries) = fs::read_dir(store) else {
        return;
    };

    // Held through the sweep, or an install that starts meanwhile would lose its folder
    let store_lock = File::options().write(true).open(store.join(STORE_LOCK));
    // Every install holds it, so a dot name found while it is free is what a stopped install left
    let installing = match &store_lock {
        Ok(file) => file.try_lock().is_err(),
        Err(_) => true,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let runtime = name.starts_with("cpython-");
        let dotted = name.starts_with('.');
        // What a later version keeps here is not this one's to remove
        let known = runtime || dotted || name.starts_with("worker-");
        // A lock file stays, since another session may be about to lock the one it opened
        if !known || current.contains(&path) || name.ends_with(".lock") {
            continue;
        }

        let locking = lock_of(&path);
        // Kept locked until the removal is done
        let lock = File::options().write(true).open(locking);
        let used = match &lock {
            Ok(file) => match file.try_lock() {
                Ok(()) => file.metadata(),
                Err(_) => continue,
            },
            // What no session ever held has none, like what an install that stopped left
            Err(e) if e.kind() == io::ErrorKind::NotFound => entry.metadata(),
            Err(_) => continue,
        };
        let leftover = dotted && !installing;
        if !leftover && !unused(used) {
            continue;
        }

        // First, so the runtime that names them is found again when this stops halfway
        if runtime {
            let named = venvs.join(&*name);
            fs::remove_dir_all(named).ok();
        }
        // Moved aside first, so a removal that stops halfway leaves nothing that looks installed
        let id = new_id();
        let aside = store.join(format!(".{id}"));
        if fs::rename(&path, &aside).is_err() {
            continue;
        }
        fs::remove_dir_all(&aside)
            .or_else(|_| fs::remove_file(&aside))
            .ok();
        info!("python: removed [name={name}]");
    }
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

/// The sha256 that a `SHA256SUMS` file lists for the file
fn summed<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    let suffix = format!("  {name}");
    sums.lines().find_map(|line| line.strip_suffix(&suffix))
}

/// Refuses what doesn't match the sha256
async fn fetched(client: &Client, url: &str, name: &str, sha: &str) -> Result<Vec<u8>, String> {
    let data = download(client, url).await?;
    let digest = Sha256::digest(&data);
    let digest = hex(&digest);
    match digest == sha {
        true => Ok(data),
        false => Err(format!("{name}: checksum mismatch")),
    }
}

async fn download(client: &Client, url: &str) -> Result<Vec<u8>, String> {
    // The URL leads the message, so the error's own text leaves it out
    let failed = |e: reqwest::Error| {
        let e = e.without_url();
        let reason = http_cause(&e);
        format!("{url}: {reason}")
    };
    let response = client.get(url).send().await.map_err(failed)?;
    let response = response.error_for_status().map_err(failed)?;
    let body = response.bytes().await.map_err(failed)?;
    Ok(body.to_vec())
}

fn hex(bytes: &[u8]) -> String {
    let digits: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    digits.concat()
}

/// Waits off the runtime's thread while another libsmol installs
async fn locked(store: &Path) -> Result<File, String> {
    let path = store.join(STORE_LOCK);
    lock(path, "", File::lock).await
}

/// Waits off the runtime's thread while another session removes it,
/// and dates the lock, which says when a session last used the item
async fn held(item: &Path, record: &str) -> Result<File, String> {
    let path = lock_of(item);
    let file = lock(path, record, File::lock_shared).await?;
    // Windows allows changing the time of a file that other sessions hold
    file.set_modified(SystemTime::now()).ok();
    Ok(file)
}

/// Runs it to the end with no output, in a group that goes when libsmol does
async fn finished(mut command: Command) -> io::Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mark = reaper::prepare(command.as_std_mut());
    let child = command.spawn()?;
    let mut group = Group::new(child, mark)?;
    group.exited().await;
    Ok(())
}

/// Next to the item, hidden from the code with the config directory, of which only the item is granted
fn lock_of(item: &Path) -> PathBuf {
    let mut path = item.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// Writes `record` into a lock that is still empty, before any lock forbids it on Windows
async fn lock(
    path: PathBuf,
    record: &str,
    locking: fn(&File) -> io::Result<()>,
) -> Result<File, String> {
    let shown = tilde(&path);
    let record = record.to_owned();
    let opening = move || {
        // Never truncated, since Windows refuses to write a file that another session holds
        let mut file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        // A record that can't be written only keeps the venv from being cleaned up
        if !record.is_empty() && file.metadata()?.len() == 0 {
            file.write_all(record.as_bytes()).ok();
        }
        // A file system without locks only loses the protection, as the history does
        locking(&file).ok();
        io::Result::Ok(file)
    };
    let opened = task::spawn_blocking(opening).await.expect("lock");
    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("{shown}: {reason}")
    };
    opened.map_err(failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::tests::temp_dir;
    use crate::utils::block_on;
    use crate::worker::python::{
        held, lock_of, summed, sweep_store, sweep_venvs, venv_name, Python, Runtime, MIN_IDLE,
    };
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// The runtime and venv tests share
    pub(crate) fn installed(config: &Path, cwd: &Path) -> Python {
        let installing = Python::install(config, cwd);
        block_on(installing).expect("python")
    }

    /// A file holding the text, last changed at the time
    fn dated(path: &Path, text: &str, time: SystemTime) {
        fs::write(path, text).expect("write");
        let file = File::options().write(true).open(path).expect("open");
        file.set_modified(time).expect("date");
    }

    #[test]
    fn store_swept() {
        let config = temp_dir("store");
        let store = config.join("runtime");
        let venvs = config.join("venvs");
        let now = SystemTime::now();
        let aged = now - MIN_IDLE - Duration::from_secs(60);

        let current = store.join("cpython-3.14.10+20261026");
        let old = store.join("cpython-3.14.9+20260901");
        let old_venv = venvs.join("cpython-3.14.9+20260901/naïve");
        for folder in [&current, &old, &old_venv] {
            fs::create_dir_all(folder).expect("folder");
            let lock = lock_of(folder);
            dated(&lock, "", aged);
        }
        fs::write(store.join("lock"), "").expect("store lock");
        for (name, lock) in [
            ("worker-0.4.0.so", Some(aged)),
            ("worker-0.4.1.so", Some(now)),
            ("worker-0.4.2.so", None),
            ("worker-0.4.3.so", Some(aged)),
        ] {
            let module = store.join(name);
            fs::write(&module, "").expect("module");
            if let Some(time) = lock {
                let lock = lock_of(&module);
                dated(&lock, "", time);
            }
        }
        // Named as a removal or a stopped install does
        fs::write(store.join(".1a1925684a1-117463bc"), "").expect("leftover");
        for name in ["worker-0.3.0.so", "résumé"] {
            let unheld = store.join(name);
            dated(&unheld, "", aged);
        }
        let held_lock = store.join("worker-0.4.3.so.lock");
        let holding = File::open(held_lock).expect("open");
        holding.lock_shared().expect("hold");

        let kept = [current];
        sweep_store(&store, &kept, &venvs);
        let mut left: Vec<String> = Vec::new();
        for entry in fs::read_dir(&store).expect("store").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            left.push(name);
        }
        left.sort();
        let venvs_left = venvs.join("cpython-3.14.9+20260901").exists();
        drop(holding);
        fs::remove_dir_all(&config).ok();

        let expected = [
            "cpython-3.14.10+20261026",
            "cpython-3.14.10+20261026.lock",
            "cpython-3.14.9+20260901.lock",
            "lock",
            "résumé",
            "worker-0.4.0.so.lock",
            "worker-0.4.1.so",
            "worker-0.4.1.so.lock",
            "worker-0.4.2.so",
            "worker-0.4.3.so",
            "worker-0.4.3.so.lock",
        ];
        assert_eq!(left, expected);
        assert!(!venvs_left);
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

    /// The runtime's own interpreter, for programs tests start outside the sandbox
    pub(crate) fn interpreter() -> PathBuf {
        let config = temp_dir("runtime");
        let installing = Runtime::install(&config);
        let runtime = block_on(installing).expect("runtime");
        runtime.interpreter()
    }

    #[test]
    fn sums_parsed() {
        let sums = "1f2e  smol-aarch64-apple-darwin.tar.gz\n\
                    9a8b  smol-worker-aarch64-apple-darwin\n\
                    3c4d  smol-worker-x86_64-apple-darwin\n";
        assert_eq!(
            summed(sums, "smol-worker-x86_64-apple-darwin"),
            Some("3c4d")
        );
        assert_eq!(summed(sums, "smol-worker-apple-darwin"), None);
    }

    #[test]
    fn marks_use() {
        let dir = temp_dir("held");
        fs::create_dir_all(&dir).expect("dir");
        let item = dir.join("item");
        let holding = held(&item, "");
        let first = block_on(holding).expect("hold");
        first.set_modified(SystemTime::UNIX_EPOCH).expect("age");

        // Another session takes it while the first still holds it
        let holding = held(&item, "");
        let second = block_on(holding).expect("hold");
        let lock = lock_of(&item);
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
