use crate::prompt;
use reaper::cause;
use std::io;

pub(crate) mod attach;
pub(crate) mod files;
pub(crate) mod mcp;
pub(crate) mod web;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolError {
    #[error("{}", prompt::failure(.action, .path, &cause(.source)))]
    Os {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("{0}")]
    Failed(String),
}

impl ToolError {
    fn os(action: &'static str, path: &str, source: io::Error) -> Self {
        let path = path.to_owned();
        Self::Os {
            action,
            path,
            source,
        }
    }
}
