use crate::acp::rpc::Peer;
use crate::agent::Task;
use crate::history::HistoryStore;
use crate::tools::attach::Mention;
use agent_client_protocol_schema::v1::{ContentBlock, EmbeddedResourceResource};
use chrono::DateTime;
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub use crate::acp::serve::run;

mod rpc;
mod serve;
mod servers;
mod session;
mod translator;

/// Every `_smol/` name is listed here and must match smol's client.rs
const NOTICE: &str = "_smol/notice";
const MCP: &str = "_smol/mcp";
const QUEUED: &str = "_smol/queued";
const UPDATE: &str = "_smol/update";
/// Replied to like a prompt, and the tool calls it opens carry the same marker in `_meta`
const SHELL: &str = "_smol/shell";

pub(crate) struct App {
    pub(crate) peer: Arc<Peer>,
    pub(crate) store: HistoryStore,
    pub(crate) config: PathBuf,
}

fn task_of(blocks: &[ContentBlock]) -> Task {
    let mut texts = Vec::new();
    let mut mentions = Vec::new();
    for block in blocks {
        let (uri, text) = match block {
            ContentBlock::Text(text) => {
                texts.push(text.text.as_str());
                continue;
            }
            ContentBlock::ResourceLink(link) => (link.uri.as_str(), None),
            ContentBlock::Resource(embedded) => match &embedded.resource {
                EmbeddedResourceResource::TextResourceContents(sent) => {
                    (sent.uri.as_str(), Some(sent.text.clone()))
                }
                _ => continue,
            },
            _ => continue,
        };

        let url = Url::parse(uri).ok();
        let lines = url.as_ref().and_then(Url::fragment).and_then(line_range);
        // `to_file_path` takes any scheme, so zed:///agent/... would become /agent/...
        let local = url.filter(|url| url.scheme() == "file");
        let path = local.and_then(|url| url.to_file_path().ok());

        let mention = match (path, text) {
            (Some(path), text) => Mention::File { path, lines, text },
            (None, Some(text)) => {
                let uri = uri.to_owned();
                Mention::Resource { uri, text }
            }
            // Clients don't always repeat links like web pages in the text
            (None, None) => {
                texts.push(uri);
                continue;
            }
        };
        mentions.push(mention);
    }

    let text = texts.join("\n");
    Task { text, mentions }
}

/// A line range in a URI fragment, written L10, L10-20, L10-L20 or L10:20
fn line_range(fragment: &str) -> Option<(u32, u32)> {
    let range = fragment.strip_prefix('L')?;
    let (first, last) = match range.split_once([':', '-']) {
        Some((first, last)) => (first, last.trim_start_matches('L')),
        None => (range, range),
    };

    let first: u32 = first.parse().ok()?;
    let last: u32 = last.parse().ok()?;
    (first >= 1 && last >= first).then_some((first, last))
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
    use crate::acp::rpc::Lines;
    use crate::acp::{line_range, task_of};
    use crate::tools::attach::Mention;
    use agent_client_protocol_schema::v1::{
        ContentBlock, EmbeddedResource, EmbeddedResourceResource, ResourceLink,
        TextResourceContents,
    };
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
        let task = task_of(&blocks);
        assert_eq!(task.text, "Summarize these\nhttps://example.com/café");
        let file = Mention::File {
            path,
            lines: None,
            text: None,
        };
        assert_eq!(task.mentions, [file]);
    }

    #[test]
    fn selections_kept() {
        let path = env::temp_dir().join("crème.rs");
        let mut uri = Url::from_file_path(&path).expect("uri");
        uri.set_query(Some("column=5"));
        uri.set_fragment(Some("L10:12"));
        let picked = ResourceLink::new("crème.rs (10-12)", uri.as_str());
        let selected = "fn café() {\n    brûlée()\n}";
        let sent = |uri: &str, text: &str| {
            let contents = TextResourceContents::new(text, uri);
            let resource = EmbeddedResourceResource::TextResourceContents(contents);
            ContentBlock::Resource(EmbeddedResource::new(resource))
        };
        let diagnostics = "zed:///agent/diagnostics?include_warnings=true";
        let blocks = [
            ContentBlock::ResourceLink(picked),
            sent(uri.as_str(), selected),
            sent(diagnostics, "error: unused café"),
        ];

        let task = task_of(&blocks);
        let file = |text: Option<&str>| Mention::File {
            path: path.clone(),
            lines: Some((10, 12)),
            text: text.map(str::to_owned),
        };
        let resource = Mention::Resource {
            uri: diagnostics.to_owned(),
            text: "error: unused café".to_owned(),
        };
        assert_eq!(task.mentions, [file(None), file(Some(selected)), resource]);
    }

    #[test]
    fn line_ranges_read() {
        let cases = [
            ("L10", Some((10, 10))),
            ("L10:20", Some((10, 20))),
            ("L10-20", Some((10, 20))),
            ("L10-L20", Some((10, 20))),
            ("L20:10", None),
            ("L0", None),
            ("café", None),
            ("L", None),
        ];
        for (fragment, range) in cases {
            assert_eq!(line_range(fragment), range, "{fragment}");
        }
    }
}
