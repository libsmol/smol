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
/// and each has the sha256 of its `install_only_stripped` archive
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
#[cfg(not(test))]
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

/// The Python a session's worker runs, kept in place while the session lasts
pub(crate) struct Python {
    store: PathBuf,
    /// The runtime, which the code only reads
    pub(crate) home: PathBuf,
    /// The worker module, which the code only reads
    pub(crate) module: PathBuf,
    /// Kept for the working directory, where the code installs packages
    pub(crate) venv: PathBuf,
    /// Shared locks on the runtime, the module and the venv, which no other session removes
    _held: [File; 3],
}

impl Python {
    /// Downloads the runtime and the worker module that are missing,
    /// and removes those of other versions, and venvs of working directories that are gone,
    /// that no session holds
    pub(crate) async fn install(config: &Path, cwd: &Path) -> Result<Self, String> {
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

        let runtime = format!("cpython-{PYTHON}+{RELEASE}");
        let (store, module, venv) = places(config, cwd, &runtime);
        let home = store.join(&runtime);
        let failed = |e: io::Error| {
            let reason = cause(&e);
            let shown = tilde(&store);
            format!("runtime: {shown}: {reason}")
        };
        let venvs = venv.parent().expect("venvs");
        for folder in [&store, venvs] {
            fs::create_dir_all(folder).map_err(failed)?;
        }
        // Taken before looking, so a removal that is under way finishes first
        let runtime_held = held(&home, "").await?;
        let module_held = held(&module, "").await?;
        // A venv's lock names its working directory, unless that isn't UTF-8 and so can't be told
        let record = cwd.to_str().unwrap_or_default();
        let venv_held = held(&venv, record).await?;

        if !home.exists() || !module.exists() {
            let _lock = locked(&store).await?;
            let builder = Client::builder().read_timeout(DOWNLOAD_WAIT);
            let client = http_client(builder)?;

            if !home.exists() {
                info!("python: downloading runtime [version={PYTHON}+{RELEASE}]");
                let name = format!("{runtime}-{target}-install_only_stripped.tar.gz");
                let url = format!(
                    "https://github.com/astral-sh/python-build-standalone/releases/download/{RELEASE}/{name}"
                );
                let archive = download(&client, &url).await?;
                checked(&name, &archive, sha)?;

                // Inside the store, because a rename moves a folder only within one file system
                let id = new_id();
                let unpacked = store.join(format!(".{id}"));
                let into = unpacked.clone();
                let unpacking = move || Archive::new(GzDecoder::new(&archive[..])).unpack(into);
                let result = task::spawn_blocking(unpacking).await.expect("unpack");
                result.map_err(failed)?;
                let unpacked = unpacked.join("python");
                // The code can't write the runtime's bytecode, and nothing has run it yet.
                // Bytecode only speeds up imports, so files that fail to compile don't stop this
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
                let name = format!("smol-worker-{target}");
                let url = format!("{base}/SHA256SUMS");
                let sums = download(&client, &url).await?;
                let sums = String::from_utf8_lossy(&sums);
                let suffix = format!("  {name}");
                let listed = sums.lines().find_map(|line| line.strip_suffix(&suffix));
                let Some(sha) = listed else {
                    return Err(format!("runtime: {name}: not in SHA256SUMS"));
                };
                let url = format!("{base}/{name}");
                let bytes = download(&client, &url).await?;
                checked(&name, &bytes, sha)?;

                let id = new_id();
                let written = store.join(format!(".{id}"));
                fs::write(&written, bytes).map_err(failed)?;
                fs::rename(&written, &module).map_err(failed)?;
                info!("python: worker module installed [version={VERSION}]");
            }

            // Lock files stay, since another session may be about to lock the one it opened
            let entries = fs::read_dir(&store).map_err(failed)?;
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let current = path == home || path == module;
                if current || name == LOCK || name.ends_with(".lock") {
                    continue;
                }
                // Unfinished installs start with a dot, and this one holds the install lock
                let unfinished = name.starts_with('.');
                let lock = match unfinished {
                    true => None,
                    false => opened(&lock_of(&path)).ok(),
                };
                // Kept locked until the removal is done
                let free = lock.as_ref().is_some_and(|file| file.try_lock().is_ok());
                if !unfinished && !free {
                    continue;
                }
                fs::remove_dir_all(&path)
                    .or_else(|_| fs::remove_file(&path))
                    .ok();
                // The venvs of a runtime that is gone
                let venvs = config.join(VENVS).join(&*name);
                fs::remove_dir_all(venvs).ok();
            }
        }

        // A lock that names no working directory is never removed
        let own = lock_of(&venv);
        let entries = fs::read_dir(venvs).map_err(failed)?;
        for entry in entries.flatten() {
            let lock = entry.path();
            let named = lock
                .extension()
                .is_some_and(|extension| extension == "lock");
            if lock == own || !named {
                continue;
            }
            let recorded = fs::read_to_string(&lock).unwrap_or_default();
            if recorded.is_empty() || Path::new(&recorded).exists() {
                continue;
            }
            let Ok(opened) = File::open(&lock) else {
                continue;
            };
            if opened.try_lock().is_err() {
                continue;
            }
            let gone = lock.with_extension("");
            fs::remove_dir_all(gone).ok();
            // Windows removes no file that is still open
            drop(opened);
            fs::remove_file(&lock).ok();
        }

        Ok(Self {
            store,
            home,
            module,
            venv,
            _held: [runtime_held, module_held, venv_held],
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
    let runtime = python.home.join(HOME_BIN).join(EXE);
    let venv = python.bin().join(EXE);

    if !python.ready() {
        // Another session in this directory may have made it while this one waited
        let _lock = locked(&python.store).await?;
        if !python.ready() {
            let mut repairing = loading(sandbox, &runtime, "repair", &venv);
            repairing.stdin(Stdio::null());
            let failed = |e: io::Error| {
                let reason = cause(&e);
                format!("venv: {reason}")
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
                return Err(format!("venv: {reason}"));
            }
            let shown = tilde(&python.venv);
            info!("python: venv made [path={shown}]");
        }
    }

    let serving = loading(sandbox, &runtime, "serve", &venv);
    Ok(serving)
}

/// Starts the runtime's interpreter, which the code can't write, unlike the venv's.
/// Without site, nothing the code wrote runs before the module confines itself
fn loading(sandbox: &Sandbox, runtime: &Path, function: &str, venv: &Path) -> Command {
    let mut command = sandbox.command(runtime);
    command
        .args(["-I", "-S", "-B", "-X", "utf8", "-c", LOADER])
        .arg(&sandbox.python.module)
        .arg(function)
        .arg(venv);
    command
}

/// The store, the worker module and the venv, which is named after the working directory
#[cfg(not(test))]
fn places(config: &Path, cwd: &Path, runtime: &str) -> (PathBuf, PathBuf, PathBuf) {
    let store = config.join(STORE);
    let module = format!("worker-{VERSION}{DLL_SUFFIX}");
    let module = store.join(module);

    let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
    let hash = hex(&digest);
    let named = cwd.file_name().unwrap_or_default().to_string_lossy();
    let named: String = named.chars().take(MAX_NAME_CHARS).collect();
    let name = format!("{named}-{}", &hash[..16]);
    let venv = config.join(VENVS).join(runtime).join(name);
    (store, module, venv)
}

/// Tests share one runtime and venv, and load the worker module `cargo test` built next to them,
/// which only `cargo build` copies up into the profile folder
#[cfg(test)]
fn places(_: &Path, _: &Path, _: &str) -> (PathBuf, PathBuf, PathBuf) {
    let exe = env::current_exe().expect("test binary");
    let deps = exe.parent().expect("deps");
    let built = deps.parent().expect("profile");
    let module = format!("{DLL_PREFIX}_smol{DLL_SUFFIX}");
    let venv = built.join(VENVS).join("venv");
    (built.join(STORE), deps.join(module), venv)
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

fn checked(name: &str, data: &[u8], sha: &str) -> Result<(), String> {
    let digest = Sha256::digest(data);
    let digest = hex(&digest);
    match digest == sha {
        true => Ok(()),
        false => Err(format!("runtime: {name}: checksum mismatch")),
    }
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

/// Never truncated, since Windows refuses to write a file that another session holds
fn opened(path: &Path) -> io::Result<File> {
    File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
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
        let mut file = opened(&path)?;
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
        format!("runtime: {shown}: {reason}")
    };
    opened.map_err(failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::utils::block_on;
    use crate::worker::python::Python;
    use std::path::Path;

    /// The runtime and venv tests share
    pub(crate) fn installed(config: &Path, cwd: &Path) -> Python {
        let installing = Python::install(config, cwd);
        block_on(installing).expect("python")
    }
}
