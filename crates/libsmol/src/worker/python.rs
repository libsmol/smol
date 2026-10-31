use crate::memory::Exit;
use crate::spawn::{drain, Clip, Group, Piece, KEPT_BYTES};
use crate::utils::{http_cause, http_client, new_id};
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
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tar::Archive;
use tokio::process::Command;
use tokio::select;
use tokio::task;
use tracing::info;

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
/// Holds the runtimes, the worker modules and their locks in the config directory
const STORE: &str = "runtime";
/// Holds a folder of venvs for each runtime in the config directory
const VENVS: &str = "venvs";
const LOCK: &str = "lock";
const MAX_NAME_CHARS: usize = 48;
const DOWNLOAD_WAIT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const HOME_BIN: &str = "bin";
#[cfg(windows)]
const HOME_BIN: &str = "";
#[cfg(unix)]
const VENV_BIN: &str = "bin";
#[cfg(windows)]
const VENV_BIN: &str = "Scripts";
#[cfg(unix)]
const EXE: &str = "python";
#[cfg(windows)]
const EXE: &str = "python.exe";
#[cfg(unix)]
const PIP: &str = "pip";
#[cfg(windows)]
const PIP: &str = "pip.exe";
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
    /// Downloads what is missing, and then removes what other versions left that no session holds
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
            return Err(format!("runtime: {target}: no build"));
        };

        let (store, module) = stored(config);
        let named = format!("cpython-{PYTHON}+{RELEASE}");
        let home = store.join(named);
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(&store);
            format!("runtime: {shown}: {reason}")
        };
        fs::create_dir_all(&store).map_err(failed)?;
        // Taken before looking, which waits out a removal under way
        let home_held = held(&home, "").await?;
        let module_held = held(&module, "").await?;
        let holds = [home_held, module_held];
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
            let unpacked = store.join(format!(".{id}"));
            let into = unpacked.clone();
            let unpacking = move || Archive::new(GzDecoder::new(&archive[..])).unpack(into);
            let result = task::spawn_blocking(unpacking).await.expect("unpack");
            result.map_err(failed)?;
            let unpacked = unpacked.join("python");

            // Compiled before anything runs it, since the code can't write its bytecode later
            let python = unpacked.join(HOME_BIN).join(EXE);
            let mut compiling = Command::new(python);
            compiling
                .args(["-I", "-m", "compileall", "-q", "-j0"])
                .arg(&unpacked)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            compiling.status().await.map_err(failed)?;
            fs::rename(&unpacked, &home).map_err(failed)?;
            info!("python: runtime installed [version={PYTHON}+{RELEASE}]");
        }

        if !module.exists() {
            // Tests load the module cargo built, never a released one
            if cfg!(test) {
                let shown = module.display();
                return Err(format!("runtime: {shown}: not built"));
            }
            info!("python: downloading worker module [version={VERSION}]");
            let base = format!("https://github.com/libsmol/smol/releases/download/v{VERSION}");
            let url = format!("{base}/SHA256SUMS");
            let sums = download(&client, &url).await?;
            let sums = String::from_utf8_lossy(&sums);
            let name = format!("smol-worker-{target}");
            let Some(sha) = summed(&sums, &name) else {
                return Err(format!("runtime: {name}: not in SHA256SUMS"));
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

        // Lock files stay, since another session may be about to lock the one it opened.
        // What no session ever held has none, like what an install that stopped left
        let entries = fs::read_dir(&store).map_err(failed)?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let current = path == home || path == module;
            if current || name == LOCK || name.ends_with(".lock") {
                continue;
            }
            let locking = lock_of(&path);
            let lock = File::options().write(true).open(locking);
            // Kept locked until the removal is done
            let free = match &lock {
                Ok(file) => file.try_lock().is_ok(),
                Err(e) => e.kind() == io::ErrorKind::NotFound,
            };
            if !free {
                continue;
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
            let venvs = config.join(VENVS).join(&*name);
            fs::remove_dir_all(venvs).ok();
            info!("python: removed [name={name}]");
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
    /// Also removes the venvs of working directories that are gone, unless a session holds them
    pub(crate) async fn install(config: &Path, cwd: &Path) -> Result<Self, String> {
        let runtime = Runtime::install(config).await?;
        let named = runtime.home.file_name().unwrap_or_default();
        let named = named.to_string_lossy();
        let venv = venv_for(config, cwd, &named);
        let venvs = venv.parent().expect("venvs");
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(venvs);
            format!("venv: {shown}: {reason}")
        };
        fs::create_dir_all(venvs).map_err(failed)?;
        // A working directory that isn't UTF-8 can't be named, and its venv stays
        let record = cwd.to_str().unwrap_or_default();
        let hold = held(&venv, record).await?;

        let own = lock_of(&venv);
        let entries = fs::read_dir(venvs).map_err(failed)?;
        for entry in entries.flatten() {
            let lock = entry.path();
            let locking = lock.extension().is_some_and(|ext| ext == "lock");
            if lock == own || !locking {
                continue;
            }
            let recorded = fs::read_to_string(&lock).unwrap_or_default();
            // One that can't be looked up now, like on a drive that is away, is not gone
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

        Ok(Self {
            runtime,
            venv,
            _held: hold,
        })
    }

    /// Holds `python` and `pip`, which programs the code starts find first on `PATH`
    pub(crate) fn bin(&self) -> PathBuf {
        self.venv.join(VENV_BIN)
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
            // Names the venv, which the user deletes to start over when it can't be fixed
            let shown = tilde(&python.venv);
            let failed = |e: io::Error| {
                let reason = cause(&e);
                format!("venv: {shown}: {reason}")
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
                let said = said.trim();
                let reason = match exit {
                    Some(exit) if said.is_empty() => exit.to_string(),
                    _ => said.to_owned(),
                };
                return Err(format!("venv: {shown}: {reason}"));
            }
            info!("python: venv made [path={shown}]");
        }
    }

    let serving = loading(sandbox, "serve", &venv);
    Ok(serving)
}

/// Starts the runtime's interpreter, which the code can't write, unlike the venv's.
/// Without site, nothing the code wrote runs before the module confines itself
fn loading(sandbox: &Sandbox, function: &str, venv: &Path) -> Command {
    let runtime = &sandbox.python.runtime;
    let interpreter = runtime.interpreter();
    let mut command = sandbox.command(&interpreter);
    command
        .args(["-I", "-S", "-B", "-X", "utf8", "-c", LOADER])
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
fn venv_for(config: &Path, cwd: &Path, runtime: &str) -> PathBuf {
    let name = venv_name(cwd);
    config.join(VENVS).join(runtime).join(name)
}

/// Tests share one venv in the profile folder
#[cfg(test)]
fn venv_for(_: &Path, _: &Path, runtime: &str) -> PathBuf {
    let exe = env::current_exe().expect("test binary");
    let built = exe.parent().and_then(Path::parent).expect("profile");
    built.join(VENVS).join(runtime).join("venv")
}

/// The folder's name, kept short for the path's sake, and a hash of the whole path
fn venv_name(cwd: &Path) -> String {
    let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
    let hash = hex(&digest);
    let named = cwd.file_name().unwrap_or_default().to_string_lossy();
    let named: String = named.chars().take(MAX_NAME_CHARS).collect();
    format!("{named}-{}", &hash[..16])
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
        false => Err(format!("runtime: {name}: checksum mismatch")),
    }
}

async fn download(client: &Client, url: &str) -> Result<Vec<u8>, String> {
    let failed = |e: reqwest::Error| {
        let reason = http_cause(&e);
        format!("runtime: {url}: {reason}")
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
    let path = store.join(LOCK);
    lock(path, "", File::lock).await
}

/// Waits off the runtime's thread while another session removes it
async fn held(item: &Path, record: &str) -> Result<File, String> {
    let path = lock_of(item);
    lock(path, record, File::lock_shared).await
}

/// Next to the item, out of the code's sight because only the item itself is granted
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
        locking(&file)?;
        io::Result::Ok(file)
    };
    let opened = task::spawn_blocking(opening).await.expect("lock");
    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("lock: {shown}: {reason}")
    };
    opened.map_err(failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::tests::temp_dir;
    use crate::utils::block_on;
    use crate::worker::python::{summed, venv_name, Python, Runtime};
    use std::path::{Path, PathBuf};

    /// The runtime and venv tests share
    pub(crate) fn installed(config: &Path, cwd: &Path) -> Python {
        let installing = Python::install(config, cwd);
        block_on(installing).expect("python")
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
    fn venvs_named() {
        let name = venv_name(Path::new("/home/zoë/naïve café"));
        assert!(name.starts_with("naïve café-"), "{name}");
        assert_eq!(name.chars().count(), "naïve café-".chars().count() + 16);

        let long = "🐍".repeat(100);
        let path = Path::new("/home").join(&long);
        let name = venv_name(&path);
        assert_eq!(name.chars().count(), 48 + 1 + 16, "{name}");
        assert_ne!(
            venv_name(Path::new("/a/work")),
            venv_name(Path::new("/b/work"))
        );
    }
}
