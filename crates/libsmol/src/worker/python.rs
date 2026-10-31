use crate::memory::Exit;
use crate::spawn::{drain, Clip, Group, KEPT_BYTES};
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
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tar::Archive;
use tokio::process::Command;
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
/// Holds the runtime and the worker module in the config directory
const STORE: &str = "python";
const LOCK: &str = "lock";
/// Venvs name the folder of the interpreter they run as their home
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
/// Loads the worker module from the path in `sys.argv[1]` whatever its file name,
/// then calls its function named in `sys.argv[2]`
const LOADER: &str = "import sys
from importlib.machinery import ExtensionFileLoader
from importlib.util import module_from_spec, spec_from_loader
loader = ExtensionFileLoader('_smol', sys.argv[1])
module = module_from_spec(spec_from_loader('_smol', loader))
loader.exec_module(module)
getattr(module, sys.argv[2])()";

/// Where a session's worker finds Python, made to exist by `command`
pub(crate) struct Python {
    /// Shared by every session and only read by the code
    pub(crate) store: PathBuf,
    home: PathBuf,
    module: PathBuf,
    /// Kept for each working directory, where the code installs packages
    pub(crate) venv: PathBuf,
}

impl Python {
    pub(crate) fn new(config: &Path, cwd: &Path) -> Self {
        let (store, module, venv) = places(config, cwd);
        let home = format!("cpython-{PYTHON}+{RELEASE}");
        let home = store.join(home);
        Self {
            store,
            home,
            module,
            venv,
        }
    }

    /// Holds `python` and `pip`, so programs the code starts find them first on `PATH`
    pub(crate) fn bin(&self) -> PathBuf {
        self.venv.join(VENV_BIN)
    }

    /// Installs what is missing first
    pub(crate) async fn command(&self, sandbox: &Sandbox) -> Result<Command, String> {
        if !self.home.exists() || !self.module.exists() {
            self.install().await?;
        }
        self.renew(sandbox).await?;

        let python = self.bin().join(EXE);
        let command = self.loading(sandbox, &python, "serve");
        Ok(command)
    }

    /// Without site, nothing the code could have written runs before the module confines itself
    fn loading(&self, sandbox: &Sandbox, python: &Path, function: &str) -> Command {
        let mut command = sandbox.command(python);
        command
            .args(["-I", "-S", "-B", "-X", "utf8", "-c", LOADER])
            .arg(&self.module)
            .arg(function);
        command
    }

    /// Downloads the runtime and the worker module, and removes those of other versions
    async fn install(&self) -> Result<(), String> {
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
        let _lock = locked(&self.store).await?;
        let client = http_client(Client::builder())?;

        if !self.home.exists() {
            let name = format!("cpython-{PYTHON}+{RELEASE}-{target}-install_only_stripped.tar.gz");
            let url = format!(
                "https://github.com/astral-sh/python-build-standalone/releases/download/{RELEASE}/{name}"
            );
            let archive = download(&client, &url).await?;
            checked(&name, &archive, sha)?;

            // Unpacked next to it, so the rename that puts it in place stays on one device
            let unpacked = self.store.join(new_id());
            let into = unpacked.clone();
            let unpacking = move || Archive::new(GzDecoder::new(&archive[..])).unpack(into);
            let failed = |e: io::Error| {
                let reason = cause(&e);
                format!("runtime: {name}: {reason}")
            };
            let result = task::spawn_blocking(unpacking).await.expect("unpack");
            result.map_err(failed)?;
            fs::rename(unpacked.join("python"), &self.home).map_err(failed)?;
            info!("python: runtime installed [version={PYTHON}+{RELEASE}]");
        }

        if !self.module.exists() {
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
            let module = download(&client, &url).await?;
            checked(&name, &module, sha)?;

            let written = self.store.join(new_id());
            let failed = |e: io::Error| {
                let reason = cause(&e);
                format!("runtime: {name}: {reason}")
            };
            fs::write(&written, module).map_err(failed)?;
            fs::rename(&written, &self.module).map_err(failed)?;
            info!("python: worker module installed [version={VERSION}]");
        }

        // A worker of another version that still runs keeps the files it already opened
        let Ok(entries) = fs::read_dir(&self.store) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.home || path == self.module || entry.file_name() == LOCK {
                continue;
            }
            fs::remove_dir_all(&path)
                .or_else(|_| fs::remove_file(&path))
                .ok();
        }
        Ok(())
    }

    /// Makes the venv anew unless it runs the current runtime, which it has no way to switch to
    async fn renew(&self, sandbox: &Sandbox) -> Result<(), String> {
        if self.current() {
            return Ok(());
        }
        // Another session in this directory may have made it while this one waited
        let _lock = locked(&self.store).await?;
        if self.current() {
            return Ok(());
        }

        // The code can write the venv, so only a confined Python may touch it
        let home = self.home.join(HOME_BIN);
        let python = home.join(EXE);
        let mut command = self.loading(sandbox, &python, "renew");
        command.stdin(Stdio::null());
        let failed = |e: io::Error| {
            let reason = cause(&e);
            format!("venv: {reason}")
        };
        let (mut group, mut pieces) = Group::start(command).map_err(failed)?;
        let exit = group.exited().await;
        let mut output = Clip::new(KEPT_BYTES);
        drain(&mut pieces, &mut output).await;

        if exit != Some(Exit::Exited(0)) {
            let (said, _) = output.text();
            let said = said.trim();
            return Err(format!("venv: {said}"));
        }
        let shown = tilde(&self.venv);
        info!("python: venv created [path={shown}]");
        Ok(())
    }

    /// The venv names the current runtime as its home
    fn current(&self) -> bool {
        let config = self.venv.join("pyvenv.cfg");
        let text = fs::read_to_string(config).unwrap_or_default();
        let named = text.lines().find_map(|line| line.strip_prefix("home = "));
        let named = named.and_then(|named| fs::canonicalize(named).ok());
        let home = self.home.join(HOME_BIN);
        named.is_some() && named == fs::canonicalize(home).ok()
    }
}

/// The store, the worker module and the venv, which is named after the working directory
#[cfg(not(test))]
fn places(config: &Path, cwd: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let store = config.join(STORE);
    let module = format!("worker-{VERSION}{DLL_SUFFIX}");
    let module = store.join(module);

    let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
    let hash = hex(&digest);
    let mut name = cwd.file_name().unwrap_or_default().to_owned();
    name.push("-");
    name.push(&hash[..16]);
    let venv = config.join("venvs").join(name);
    (store, module, venv)
}

/// Tests share one runtime and venv, and load the worker module `cargo test` built next to them,
/// which only `cargo build` copies up into the profile folder
#[cfg(test)]
fn places(_: &Path, _: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let exe = env::current_exe().expect("test binary");
    let deps = exe.parent().expect("deps");
    let built = deps.parent().expect("profile");
    let module = format!("{DLL_PREFIX}_smol{DLL_SUFFIX}");
    (built.join(STORE), deps.join(module), built.join("venv"))
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

/// Waits off the runtime's thread while another libsmol holds the lock
async fn locked(store: &Path) -> Result<File, String> {
    let path = store.join(LOCK);
    let locking = move || {
        let file = File::create(&path)?;
        file.lock()?;
        io::Result::Ok(file)
    };
    let locked = task::spawn_blocking(locking).await.expect("lock");
    let failed = |e: io::Error| {
        let reason = cause(&e);
        format!("runtime: {LOCK}: {reason}")
    };
    locked.map_err(failed)
}
