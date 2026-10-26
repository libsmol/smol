use crate::config::Skill;
use crate::utils::new_id;
use crate::worker::Output;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reaper::cause;
use std::env;
use std::fs;
use std::io;
#[cfg(windows)]
use std::path::Prefix;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use tracing::warn;
use wasmtime_wasi::{FsPerms, WasiCtx};

#[cfg(not(windows))]
const ZONEINFO: &str = "/usr/share/zoneinfo";
const PEM_LINE_CHARS: usize = 64;

/// The guest can't read the system's certificate store, so it gets the roots as one PEM file
static ROOTS: LazyLock<Option<String>> = LazyLock::new(|| {
    let found = rustls_native_certs::load_native_certs();
    if let Some(error) = found.errors.first() {
        warn!("worker: root certificates partly unread, {error}");
    }
    if found.certs.is_empty() {
        return None;
    }

    let mut pem = String::new();
    for cert in &found.certs {
        let encoded = STANDARD.encode(cert);
        pem.push_str("-----BEGIN CERTIFICATE-----\n");
        for line in encoded.as_bytes().chunks(PEM_LINE_CHARS) {
            pem.push_str(&String::from_utf8_lossy(line));
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
    }
    Some(pem)
});

/// What a session's code can see, which outlives restarts and removes its temporary folder when dropped
pub(crate) struct Sandbox {
    pub(crate) cwd: PathBuf,
    pub(crate) copies: PathBuf,
    mounts: Vec<Mount>,
    env: Vec<(String, String)>,
    temp: PathBuf,
}

struct Mount {
    host: PathBuf,
    /// Checked paths must resolve under one of these
    real: PathBuf,
    guest: String,
    perms: FsPerms,
}

impl Sandbox {
    pub(crate) fn new(cwd: &Path, copies: &Path, skills: &[Skill]) -> Result<Self, String> {
        #[cfg(windows)]
        if guest_path(cwd).starts_with("//") {
            let shown = cwd.display();
            return Err(format!("{shown}: only folders on a drive are supported"));
        }

        let id = new_id();
        let name = format!("smol-{id}");
        let temp = env::temp_dir().join(name);
        let scratch = temp.join("tmp");
        if let Err(e) = fs::create_dir_all(&scratch) {
            let reason = cause(&e);
            return Err(format!("{}: {reason}", temp.display()));
        }
        let mut sandbox = Self {
            cwd: cwd.to_owned(),
            copies: copies.to_owned(),
            mounts: Vec::new(),
            env: Vec::new(),
            temp: temp.clone(),
        };

        sandbox.mount(cwd, FsPerms::ReadWrite)?;
        for skill in skills {
            let Some(folder) = skill.path.parent() else {
                continue;
            };
            let real = dunce::canonicalize(folder).unwrap_or_default();
            let covered = sandbox
                .mounts
                .iter()
                .any(|mount| real.starts_with(&mount.real));
            if covered {
                continue;
            }
            if let Err(reason) = sandbox.mount(folder, FsPerms::ReadOnly) {
                warn!("worker: skill not mounted, {reason}");
            }
        }
        // `tmp` stays writable inside the read-only folder because the longest mount wins
        sandbox.mount(&temp, FsPerms::ReadOnly)?;
        sandbox.mount(&scratch, FsPerms::ReadWrite)?;

        let mut vars = Vec::new();
        #[cfg(unix)]
        for name in ["HOME", "USER", "LOGNAME"] {
            if let Ok(value) = env::var(name) {
                vars.push((name.to_owned(), value));
            }
        }
        #[cfg(windows)]
        if let Some(home) = env::var_os("USERPROFILE") {
            let home = guest_path(Path::new(&home));
            vars.push(("HOME".to_owned(), home));
        }
        #[cfg(windows)]
        if let Ok(user) = env::var("USERNAME") {
            vars.push(("USER".to_owned(), user.clone()));
            vars.push(("LOGNAME".to_owned(), user));
        }
        if let Ok(path) = env::var("PATH") {
            vars.push(("PATH".to_owned(), path));
        }
        vars.push(("LANG".to_owned(), "C.UTF-8".to_owned()));
        let zone = env::var("TZ").or_else(|_| iana_time_zone::get_timezone());
        if let Ok(zone) = zone {
            vars.push(("TZ".to_owned(), zone));
        }

        #[cfg(not(windows))]
        let zones = Path::new(ZONEINFO);
        #[cfg(not(windows))]
        if zones.is_dir() && sandbox.mount(zones, FsPerms::ReadOnly).is_ok() {
            vars.push(("TZDIR".to_owned(), ZONEINFO.to_owned()));
            vars.push(("PYTHONTZPATH".to_owned(), ZONEINFO.to_owned()));
        }
        vars.push(("TMPDIR".to_owned(), guest_path(&scratch)));

        if let Some(pem) = &*ROOTS {
            let roots = temp.join("roots.pem");
            match fs::write(&roots, pem) {
                Ok(()) => vars.push(("SSL_CERT_FILE".to_owned(), guest_path(&roots))),
                Err(e) => {
                    let reason = cause(&e);
                    warn!("worker: root certificates unwritten, {reason}");
                }
            }
        }
        sandbox.env = vars;
        Ok(sandbox)
    }

    /// Mounts the folder at its own path, so the code sees the paths the prompt shows
    fn mount(&mut self, host: &Path, perms: FsPerms) -> Result<(), String> {
        let real = match dunce::canonicalize(host) {
            Ok(real) => real,
            Err(e) => {
                let reason = cause(&e);
                return Err(format!("{}: {reason}", host.display()));
            }
        };
        self.mounts.push(Mount {
            host: host.to_owned(),
            real,
            guest: guest_path(host),
            perms,
        });
        Ok(())
    }

    pub(crate) fn context(&self, output: Output) -> Result<WasiCtx, String> {
        let mut builder = WasiCtx::builder();
        builder
            .stdout(output.clone())
            .stderr(output)
            .inherit_network()
            .allow_ip_name_lookup(true)
            .allow_tcp(true)
            .allow_udp(true);
        for (name, value) in &self.env {
            builder.env(name, value);
        }
        for mount in &self.mounts {
            let opened = builder.preopened_dir(&mount.host, &mount.guest, mount.perms);
            if let Err(e) = opened {
                return Err(format!("{}: {e}", mount.host.display()));
            }
        }
        let cwd = guest_path(&self.cwd);
        builder.initial_cwd(cwd);
        Ok(builder.build())
    }

    pub(crate) fn reach(&self, path: &Path) -> io::Result<Reach> {
        let real = dunce::canonicalize(path)?;
        let inside = self
            .mounts
            .iter()
            .any(|mount| real.starts_with(&mount.real));
        if inside {
            return Ok(Reach::Inside);
        }

        let lexical = normalized(path);
        let started = self
            .mounts
            .iter()
            .any(|mount| lexical.starts_with(&mount.host));
        match started {
            true => Ok(Reach::Escaped),
            false => Ok(Reach::Outside),
        }
    }
}

/// Where a path leads, which decides how the guest's own `open()` of it fails
pub(crate) enum Reach {
    Inside,
    /// A link leads out of the mount the path starts in
    Escaped,
    /// The path starts in no mount
    Outside,
}

/// Drops `.` and `..` without looking at the files, as the guest's working directory does
pub(crate) fn normalized(path: &Path) -> PathBuf {
    let mut kept = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                kept.pop();
            }
            Component::CurDir => {}
            other => kept.push(other),
        }
    }
    kept
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.temp).ok();
    }
}

#[cfg(not(windows))]
pub(crate) fn guest_path(host: &Path) -> String {
    host.to_string_lossy().into_owned()
}

#[cfg(not(windows))]
pub(crate) fn host_path(guest: &str) -> PathBuf {
    PathBuf::from(guest)
}

/// wasi-libc takes only paths starting with `/` as absolute, so `C:\x` is `/c/x`
#[cfg(windows)]
pub(crate) fn guest_path(host: &Path) -> String {
    let mut guest = String::new();
    for part in host.components() {
        match part {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    let letter = char::from(letter).to_ascii_lowercase();
                    guest.push('/');
                    guest.push(letter);
                }
                _ => guest.push_str("//"),
            },
            Component::RootDir => {}
            other => {
                if !guest.is_empty() {
                    guest.push('/');
                }
                guest.push_str(&other.as_os_str().to_string_lossy());
            }
        }
    }
    guest
}

#[cfg(windows)]
pub(crate) fn host_path(guest: &str) -> PathBuf {
    let mut parts = guest.splitn(3, '/');
    let (Some(""), Some(drive)) = (parts.next(), parts.next()) else {
        return PathBuf::from(guest);
    };
    let letter = drive.chars().next().filter(char::is_ascii_alphabetic);
    let (Some(letter), 1) = (letter, drive.len()) else {
        return PathBuf::from(guest);
    };
    let rest = parts.next().unwrap_or_default().replace('/', "\\");
    let letter = letter.to_ascii_uppercase();
    PathBuf::from(format!("{letter}:\\{rest}"))
}
