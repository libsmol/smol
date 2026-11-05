use crate::config::{Access, Skill};
use crate::worker::sandbox::{Rules, Sandbox};
use crate::worker::venv::Venv;
use reaper::{cause, tilde};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    ".config/git/credentials",
    ".pgpass",
    ".vault-token",
    ".terraform.d/credentials.tfrc.json",
    ".password-store",
    ".local/share/keyrings",
    "Library/Keychains",
    ".smol",
    ".claude",
    ".claude.json",
    ".codex",
];

#[cfg(unix)]
pub(crate) const PASSED_ENV: &[&str] = &[
    "HOME", "USER", "LOGNAME", "PATH", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ",
];
#[cfg(windows)]
pub(crate) const PASSED_ENV: &[&str] = &[
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

/// Where a session's workers run, which they share while the session lives
pub(crate) struct Workplace {
    pub(crate) cwd: PathBuf,
    /// Copies of the images and videos the code viewed
    pub(crate) copies: PathBuf,
    pub(crate) venv: Venv,
    pub(crate) sandbox: Sandbox,
    writable: Option<Vec<PathBuf>>,
}

impl Workplace {
    /// Decides what the code may reach, on top of agent.toml's `[allow]` and `[deny]`
    pub(crate) fn open(
        cwd: PathBuf,
        copies: PathBuf,
        venv: Venv,
        config: &Path,
        skills: &[Skill],
        access: &Access,
    ) -> Result<Self, String> {
        // A Landlock rule holds on to the folder that exists when the rule is made.
        // An earlier session's code may have swapped one for a link, which the grant would follow
        for folder in [&copies, &venv.path] {
            let failed = |e: io::Error| {
                let shown = tilde(folder);
                let reason = cause(&e);
                format!("{shown}: {reason}")
            };
            let swapped = fs::symlink_metadata(folder).is_ok_and(|found| !found.is_dir());
            if swapped {
                let removed = fs::remove_file(folder).or_else(|_| fs::remove_dir(folder));
                removed.map_err(failed)?;
            }
            fs::create_dir_all(folder).map_err(failed)?;
        }

        let mut rules = Rules::default();
        for name in PASSED_ENV {
            rules.env.push(name.to_string());
        }
        rules.env.extend(access.env.iter().cloned());
        rules.write = vec![cwd.clone(), copies.clone(), venv.path.clone()];
        rules.write.extend(access.write.iter().cloned());
        for skill in skills {
            if let Some(folder) = skill.path.parent() {
                rules.read.push(folder.to_owned());
            }
        }
        // The worker and the venv's python are libsmol's own executable, wherever it is installed
        rules.read.push(worker::executable());
        rules.hidden.push(config.to_owned());
        if let Some(home) = env::home_dir() {
            for name in CREDENTIALS {
                rules.hidden.push(home.join(name));
            }
        }
        rules.hidden.extend(access.hidden.iter().cloned());

        let sandbox = Sandbox::new(&rules)?;
        let mut existing = Vec::new();
        for path in &access.write {
            if path.exists() {
                existing.push(path.clone());
            }
        }
        let writable = sandbox.enforced().then_some(existing);

        Ok(Self {
            cwd,
            copies,
            venv,
            sandbox,
            writable,
        })
    }

    /// The places in `[allow] write` that existed when the session opened, which the code may write
    /// besides its own, or `None` when files aren't isolated
    pub(crate) fn writable(&self) -> Option<&[PathBuf]> {
        self.writable.as_deref()
    }
}

impl Drop for Workplace {
    fn drop(&mut self) {
        // Removed only here, and only when empty, because the worker's rule is on the folder made at the start
        fs::remove_dir(&self.copies).ok();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::config::Access;
    use crate::worker::venv::tests::shared_venv;
    use crate::worker::workplace::Workplace;
    use std::path::Path;

    /// A session in `cwd` whose config folder is `config`, with its copies in it
    pub(crate) fn opened(cwd: &Path, config: &Path, access: &Access) -> Workplace {
        let copies = config.join("copies");
        let venv = shared_venv(config, cwd);
        let opening = Workplace::open(cwd.to_owned(), copies, venv, config, &[], access);
        opening.expect("workplace")
    }
}
