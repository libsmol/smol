use crate::acp::rpc::Peer;
use crate::history::HistoryStore;
use agent_client_protocol_schema::v1::ContentBlock;
use chrono::DateTime;
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub use crate::acp::serve::run;

mod mcp;
mod rpc;
mod serve;
mod session;
mod translator;

/// Every `_smol/` name is listed here and must match smol's client.rs
const NOTICE: &str = "_smol/notice";
const MCP: &str = "_smol/mcp";
const QUEUED: &str = "_smol/queued";
const UPDATE: &str = "_smol/update";
/// Marks prompts and replayed tasks in `_meta`, and also names the goal state notification
const GOAL: &str = "_smol/goal";
const CLEAR_GOAL: &str = "_smol/clear_goal";
/// Replied to like a prompt, and the tool calls it opens carry the same marker in `_meta`
const SHELL: &str = "_smol/shell";

pub(crate) struct App {
    pub(crate) peer: Arc<Peer>,
    pub(crate) store: HistoryStore,
    pub(crate) config: PathBuf,
}

fn prompt_parts(blocks: &[ContentBlock]) -> (String, Vec<PathBuf>) {
    let mut texts = Vec::new();
    let mut files = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => texts.push(text.text.as_str()),
            ContentBlock::ResourceLink(link) => {
                let url = Url::parse(&link.uri).ok();
                match url.and_then(|url| url.to_file_path().ok()) {
                    Some(path) => files.push(path),
                    // Clients don't always repeat links like web pages in the text
                    None => texts.push(link.uri.as_str()),
                }
            }
            _ => {}
        }
    }

    let text = texts.join("\n");
    (text, files)
}

fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("serialize")
}

fn rfc3339(epoch: f64) -> String {
    let seconds = epoch.floor() as i64;
    let time = DateTime::from_timestamp(seconds, 0).unwrap_or_default();

    time.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use crate::acp::prompt_parts;
    use crate::acp::rpc::Lines;
    use agent_client_protocol_schema::v1::{ContentBlock, ResourceLink};
    use reqwest::Url;
    use serde_json::Value;
    use std::env;

    pub(crate) fn drained(sent: &mut Lines) -> Vec<Value> {
        let mut messages = Vec::new();
        while let Ok(Ok(line)) = sent.try_recv() {
            let message = serde_json::from_str(&line).expect("json");
            messages.push(message);
        }
        messages
    }

    #[test]
    fn links_kept() {
        let path = env::temp_dir().join("résumé.md");
        let uri = Url::from_file_path(&path).expect("uri");
        let file = ResourceLink::new("résumé.md", uri.as_str());
        let page = ResourceLink::new("Café", "https://example.com/café");
        let blocks = [
            ContentBlock::from("Summarize these"),
            ContentBlock::ResourceLink(file),
            ContentBlock::ResourceLink(page),
        ];
        let (text, files) = prompt_parts(&blocks);
        assert_eq!(text, "Summarize these\nhttps://example.com/café");
        assert_eq!(files, [path]);
    }
}
