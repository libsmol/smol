use crate::model::ModelError;
use std::result;

mod acp;
mod agent;
mod code;
mod config;
mod history;
mod memory;
mod model;
mod prompt;
mod setup;
mod spawn;
mod tools;
mod utils;
mod worker;

pub use crate::acp::run;
pub use crate::setup::{run as setup, SETUP_ARG};

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
}

pub type Result<T, E = Error> = result::Result<T, E>;

#[cfg(test)]
mod tests {
    use crate::model::{ChatMessage, Part};
    use crate::utils::{new_id, Interrupt};
    use serde_json::Value;
    use std::env;
    use std::fs;
    use std::ops::{Deref, DerefMut};
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

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
