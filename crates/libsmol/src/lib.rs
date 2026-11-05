use crate::model::ModelError;
use crate::worker::guest;
use std::ffi::OsString;
use std::result;

mod acp;
mod agent;
mod config;
mod history;
mod model;
mod prompt;
mod spawn;
mod steps;
mod tools;
mod utils;
mod worker;

pub use crate::acp::run;
pub use crate::config::setup::{run as setup, SETUP_ARG};

/// Runs as a venv's python or as a session's worker when started as one with `args`,
/// and returns its exit status
pub fn embedded(args: &[OsString]) -> Option<i32> {
    // The worker crate, which the worker module shadows here
    ::worker::main(args).or_else(|| guest::main(args))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    /// No model.toml, which clients recognize by -32000 and answer by running setup
    #[error("config: {0}")]
    Unconfigured(String),
    #[error("setup: {0}")]
    Setup(String),
    #[error("session: {0}: not found")]
    SessionNotFound(String),
    #[error("acp: {0}")]
    Acp(String),
    #[error("acp: parse error: {0}")]
    Parse(String),
    #[error("acp: invalid request")]
    InvalidRequest,
    #[error("acp: {0}: method not found")]
    MethodNotFound(String),
    /// Has no label because callers write one into the text
    #[error("{0}")]
    InvalidParams(String),
    /// A task kept out of the history, which the client recognizes by -32602
    #[error("task: {0}")]
    Refused(String),
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("python: {0}")]
    Python(String),
    #[error("history: {0}")]
    History(String),
    /// Has no label because the memories write one into the text
    #[error("{0}")]
    Memory(String),
    #[error("memory: {0}: not found")]
    MemoryNotFound(i64),
}

pub type Result<T, E = Error> = result::Result<T, E>;

#[cfg(test)]
mod tests {
    use crate::model::{ChatMessage, Part};
    use crate::utils::{new_id, Interrupt};
    use serde_json::Value;
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::ops::{Deref, DerefMut};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::thread;
    use std::time::Duration;

    /// Tests start workers and python from the test binary, which has no main of its own
    #[ctor::ctor]
    fn divert() {
        let args: Vec<OsString> = env::args_os().collect();
        if let Some(status) = crate::embedded(&args) {
            process::exit(status);
        }
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

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let id = new_id();
        let name = format!("smol-{name}-{id}");
        env::temp_dir().join(name)
    }

    pub(crate) struct Temp<T> {
        pub(crate) value: T,
        pub(crate) dir: PathBuf,
    }

    impl<T> Deref for Temp<T> {
        type Target = T;

        fn deref(&self) -> &T {
            &self.value
        }
    }

    impl<T> DerefMut for Temp<T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.value
        }
    }

    impl<T> Drop for Temp<T> {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// Waits for the code to start before interrupting
    pub(crate) fn cancel_soon(interrupt: &Interrupt, turn: u64) {
        let stopping = interrupt.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            stopping.cancel(turn);
        });
    }

    pub(crate) fn write_file(path: &Path, data: &[u8]) {
        let parent = path.parent().expect("a parent");
        fs::create_dir_all(parent).expect("create dirs");
        fs::write(path, data).expect("write");
    }

    pub(crate) fn content(message: &ChatMessage) -> String {
        let mut lines = Vec::new();
        match message {
            ChatMessage::System(text) => lines.push(text.clone()),
            ChatMessage::Assistant { text, calls, .. } => {
                if !text.is_empty() {
                    lines.push(text.clone());
                }
                for call in calls {
                    let arguments: Value = serde_json::from_str(&call.arguments).expect("json");
                    let code = arguments["code"].as_str().unwrap_or_default();
                    let shown = format!("{}:\n{code}", call.name);
                    lines.push(shown);
                }
            }
            ChatMessage::User(parts) | ChatMessage::Tool { parts, .. } => {
                for part in parts {
                    if let Part::Text(text) = part {
                        lines.push(text.clone());
                    }
                }
            }
        }
        lines.join("\n")
    }
}
