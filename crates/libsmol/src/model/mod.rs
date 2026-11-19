use crate::config::{Kind, ModelSpec, Provider};
use crate::prompt::{self, ARGUMENT, TOOL};
use crate::tools::attach::read_copy;
use crate::utils::{content_type, excerpt, http_cause, http_client, read_cause};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::env;
use std::future::Future;
use std::mem;
use std::ops::Not;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

mod completions;
mod responses;

/// With a long context the server may send nothing for a long time before the first token
const IDLE_WAIT: Duration = match cfg!(test) {
    true => Duration::from_secs(1),
    false => Duration::from_secs(300),
};
/// A peer whose machine drops off sends no FIN, and without probes we would wait out `IDLE_WAIT`
const KEEPALIVE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_RETRIES: u32 = 3;
/// Don't retry when the server asks for a longer wait
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Servers only accept tool names of letters, digits, `_` and `-` up to this length
pub(crate) const MAX_NAME_CHARS: usize = 64;

pub(crate) const DEFAULT_API: &str = "https://api.openai.com/v1";
const LIST_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChatMessage {
    System(String),
    /// The parts are joined with newlines when sent
    User(Vec<Part>),
    Assistant {
        text: String,
        reasoning: Option<String>,
        sealed: Option<Sealed>,
        calls: Vec<ToolCall>,
    },
    Tool {
        id: String,
        parts: Vec<Part>,
    },
}

/// The output items of a Responses reply, or one item shaped like a Chat Completions reply,
/// without the text and calls the step holds itself, which only their provider reads back
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Sealed {
    /// The `[model.<name>]` table, since no other provider can open them
    pub(crate) provider: String,
    pub(crate) kind: Kind,
    pub(crate) items: Vec<Value>,
    /// Bytes of reply text received as each item ended, which give each message its own text back
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ends: Vec<usize>,
}

impl ChatMessage {
    pub(crate) fn user(text: impl Into<String>) -> Self {
        let parts = vec![Part::Text(text.into())];
        ChatMessage::User(parts)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The JSON text exactly as sent
    pub(crate) arguments: String,
}

impl ToolCall {
    pub(crate) fn eval(id: String, code: &str) -> Self {
        let arguments = json!({ ARGUMENT: code });
        Self {
            id,
            name: TOOL.to_owned(),
            arguments: arguments.to_string(),
        }
    }

    /// Servers reject non-JSON arguments and invalid names, so the reply gives the original name
    pub(crate) fn unread(id: String, name: &str) -> Self {
        let mut sent = String::new();
        for c in name.chars().take(MAX_NAME_CHARS) {
            let allowed = c.is_ascii_alphanumeric() || c == '_' || c == '-';
            let c = match allowed {
                true => c,
                false => '_',
            };
            sent.push(c);
        }
        if sent.is_empty() {
            sent.push('_');
        }

        Self {
            id,
            name: sent,
            arguments: "{}".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Part {
    Text(String),
    /// The display name and media the model takes, whose copy is read only when a request is sent
    Media(String, Media),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Media {
    pub(crate) kind: MediaKind,
    pub(crate) mime: String,
    /// The file name in the copies directory, which still resolves after the config directory moves
    pub(crate) file: PathBuf,
    /// Returned by MCP, so the name is the function called rather than a path
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) from_call: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    pub(crate) fn word(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    /// The part of the input served from the prompt cache, which some servers leave out on a miss
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_tokens: Option<u64>,
}

impl TokenUsage {
    pub(crate) fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cached_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Step,
    Summary,
    Title,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finish {
    Stop,
    Length,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Finished {
    pub(crate) finish: Finish,
    pub(crate) usage: Option<TokenUsage>,
}

pub(crate) enum StreamDelta<'a> {
    Reasoning(&'a str),
    Content(&'a str),
    /// The code of an `eval` tool call, decoded as it streams in
    CallCode(&'a str),
    CallEnd(Result<(), Unread<'a>>),
    /// All the reply holds so far beyond its text and calls, which only its provider reads
    Sealed(&'a Sealed),
}

/// A call to another tool, or one whose arguments yield no code, carried exactly as received
pub(crate) struct Unread<'a> {
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ModelError {
    message: String,
    pub(crate) transient: bool,
    pub(crate) retry_after: Option<Duration>,
}

impl ModelError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
            retry_after: None,
        }
    }

    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
            retry_after: None,
        }
    }
}

pub(crate) type Generating<'a> =
    Pin<Box<dyn Future<Output = Result<Finished, ModelError>> + Send + 'a>>;

pub(crate) trait Model: Send + Sync {
    /// Dropping the returned future stops generation
    fn generate<'a>(
        &'a self,
        conversation: &'a [ChatMessage],
        purpose: Purpose,
        on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
    ) -> Generating<'a>;

    fn context_window(&self) -> Option<u64> {
        None
    }

    /// Sent with every request, and servers may refuse one whose input and this together pass the window
    fn max_tokens(&self) -> Option<u64> {
        None
    }

    fn takes(&self, _kind: MediaKind) -> bool {
        false
    }
}

pub(crate) struct OpenAIModel {
    client: Client,
    /// The `[model.<name>]` table it comes from
    provider: String,
    kind: Kind,
    spec: ModelSpec,
    url: String,
    key: Option<String>,
    /// Where the media it sends was copied to
    copies: PathBuf,
}

impl OpenAIModel {
    pub(crate) fn new(
        name: &str,
        provider: &Provider,
        spec: ModelSpec,
        copies: PathBuf,
    ) -> Result<Self, ModelError> {
        // A key from the environment only goes to the default OpenAI endpoint
        let key = provider.api_key.clone();
        let key = match provider.api.is_some() {
            true => key,
            false => key.or_else(|| env::var("OPENAI_API_KEY").ok()),
        };
        let key = key.filter(|key| real_key(key));
        let api = provider.api.as_deref().unwrap_or(DEFAULT_API);
        let api = api.trim_end_matches('/');
        let path = match provider.kind {
            Kind::Completions => "chat/completions",
            Kind::Responses => "responses",
        };

        let builder = Client::builder()
            .read_timeout(IDLE_WAIT)
            .tcp_keepalive(KEEPALIVE)
            .tcp_keepalive_interval(KEEPALIVE_INTERVAL)
            .tcp_keepalive_retries(KEEPALIVE_RETRIES);
        let client = http_client(builder).map_err(ModelError::new)?;

        Ok(Self {
            client,
            provider: name.to_owned(),
            kind: provider.kind,
            spec,
            url: format!("{api}/{path}"),
            key,
            copies,
        })
    }

    /// Leaving it out still has the server reason at its default effort
    fn effort_for(&self, purpose: Purpose) -> Option<&str> {
        let effort = self.spec.effort.as_deref();
        effort.filter(|_| purpose != Purpose::Title)
    }

    /// Sealed items go back only to the provider and API that wrote them
    fn opens<'a>(&self, sealed: Option<&'a Sealed>) -> Option<&'a Sealed> {
        let sealed = sealed?;
        let ours = sealed.provider == self.provider && sealed.kind == self.kind;
        ours.then_some(sealed)
    }

    /// Media follows the tag naming it, and `attached` takes images a tool message can't hold
    fn content(&self, messages: &[&[Part]], mut attached: Option<&mut Vec<Value>>) -> Value {
        let text_type = match self.kind {
            Kind::Completions => "text",
            Kind::Responses => "input_text",
        };
        let mut blocks = Vec::new();
        let mut lines = Vec::new();
        for parts in messages {
            // Merged messages are separated by a blank line, except right after media
            if !lines.is_empty() {
                lines.push(String::new());
            }

            for part in *parts {
                let (label, media) = match part {
                    Part::Text(text) => {
                        lines.push(text.clone());
                        continue;
                    }
                    Part::Media(label, media) => (label, media),
                };

                let copy = self.copies.join(&media.file);
                let Ok(bytes) = read_copy(&copy, media.kind) else {
                    let note = prompt::missing(label, media);
                    lines.push(note);
                    continue;
                };

                let tag = prompt::media_tag(label, media);
                let data = STANDARD.encode(bytes);
                let url = format!("data:{};base64,{data}", media.mime);
                let block = match self.kind {
                    Kind::Completions => {
                        let kind = match media.kind {
                            MediaKind::Image => "image_url",
                            MediaKind::Video => "video_url",
                        };
                        json!({ "type": kind, kind: { "url": url } })
                    }
                    // The configuration gives this API no videos, and its messages require `detail`
                    Kind::Responses => {
                        json!({ "type": "input_image", "image_url": url, "detail": "auto" })
                    }
                };

                if let Some(attached) = attached.as_deref_mut() {
                    attached.push(json!({ "type": text_type, "text": tag }));
                    attached.push(block);
                    lines.push(tag);
                    continue;
                }

                lines.push(tag);
                let text = mem::take(&mut lines).join("\n");
                blocks.push(json!({ "type": text_type, "text": text }));
                blocks.push(block);
            }
        }

        // Without any media sent the content stays a plain string
        let text = lines.join("\n");
        match blocks.is_empty() {
            true => json!(text),
            false if lines.is_empty() => Value::Array(blocks),
            false => {
                blocks.push(json!({ "type": text_type, "text": text }));
                Value::Array(blocks)
            }
        }
    }
}

impl Model for OpenAIModel {
    /// Never sends `stop` because some servers use it to cut off reasoning
    fn generate<'a>(
        &'a self,
        conversation: &'a [ChatMessage],
        purpose: Purpose,
        on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
    ) -> Generating<'a> {
        let body = match self.kind {
            Kind::Completions => completions::body(self, conversation, purpose),
            Kind::Responses => responses::body(self, conversation, purpose),
        };

        let mut request = self.client.post(&self.url).json(&body);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }

        // Dropping the future drops the request and its connection, so the server stops generating
        Box::pin(async move {
            let mut response = match request.send().await {
                Ok(response) => response,
                Err(e) => return Err(network_error(&e)),
            };

            let (mime, _) = content_type(response.headers());
            let status = response.status();
            if !status.is_success() {
                let headers = response.headers();
                let seconds = |name| {
                    let value = headers.get(name)?.to_str().ok()?;
                    value.trim().parse::<f64>().ok()
                };
                let millis = seconds("retry-after-ms").map(|ms| ms / 1000.0);
                let wait = millis.or_else(|| seconds("retry-after"));
                let retry_after = wait.and_then(|wait| Duration::try_from_secs_f64(wait).ok());

                let text = response.text().await.unwrap_or_default();
                let said = said(status, &mime, &text);
                let code = status.as_u16();
                let mut message = prompt::http_status(code, &said);
                let mut transient = retryable(code);
                if let Some(wait) = retry_after.filter(|wait| *wait > MAX_RETRY_WAIT) {
                    let seconds = wait.as_secs_f64().ceil() as u64;
                    message = format!("{message} (retry after {seconds}s)");
                    transient = false;
                }
                return Err(ModelError {
                    message,
                    transient,
                    retry_after,
                });
            }

            // A wrong address often returns a web page, which read as a stream always looks cut off
            if mime == "text/html" || mime == "application/json" {
                let text = response.text().await.unwrap_or_default();
                let json: Value = serde_json::from_str(&text).unwrap_or_default();
                let message = match json["error"].is_null() {
                    true => format!("http 200: {mime}, not an event stream"),
                    false => {
                        let said = error_text(&json, &text);
                        format!("http 200: {said}")
                    }
                };
                return Err(ModelError::new(message));
            }

            let mut stream = Stream::new(self.kind, &self.provider);
            loop {
                let fed = match response.chunk().await {
                    Ok(Some(chunk)) => stream.feed(&chunk, on_delta),
                    // The last line may have no trailing newline
                    Ok(None) => stream.feed(b"\n", on_delta).map(|_| true),
                    Err(e) => Err(network_error(&e)),
                };

                match fed {
                    Ok(true) => break,
                    Ok(false) => {}
                    // Once all the code is in, it doesn't matter how the stream broke
                    Err(_) if stream.finish.is_some() || stream.written() => break,
                    Err(e) => return Err(e),
                }
            }

            if stream.finish.is_none() && stream.written() {
                stream.finish = Some(Finish::Stop);
                stream.hand_over(on_delta);
            }
            stream.seal(on_delta);
            stream.finished()
        })
    }

    fn context_window(&self) -> Option<u64> {
        self.spec.context
    }

    fn max_tokens(&self) -> Option<u64> {
        self.spec.max_tokens
    }

    fn takes(&self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Image => self.spec.image,
            MediaKind::Video => self.spec.video,
        }
    }
}

/// Adjacent user messages go together, since both APIs send them merged into one
fn grouped(conversation: &[ChatMessage]) -> impl Iterator<Item = &[ChatMessage]> {
    let together = |a: &ChatMessage, b: &ChatMessage| {
        matches!((a, b), (ChatMessage::User(_), ChatMessage::User(_)))
    };
    conversation.chunk_by(together)
}

/// The parts of a group of user messages, one slice for each message
fn merged(group: &[ChatMessage]) -> Vec<&[Part]> {
    let mut merged = Vec::new();
    for message in group {
        if let ChatMessage::User(parts) = message {
            merged.push(parts.as_slice());
        }
    }
    merged
}

fn tool_parameters() -> Value {
    let code = json!({ "type": "string", "description": prompt::ARGUMENT_DOC });
    json!({
        "type": "object",
        "properties": { ARGUMENT: code },
        "required": [ARGUMENT],
    })
}

/// Whether the key, the address or something else made listing fail
#[derive(Debug, PartialEq)]
pub(crate) enum ListError {
    Key(String),
    Address(String),
    Other(String),
}

/// A model an endpoint lists
#[derive(Debug)]
pub(crate) struct Offered {
    pub(crate) id: String,
    pub(crate) context: Option<u64>,
}

pub(crate) async fn listed(api: &str, key: &str) -> Result<Vec<Offered>, ListError> {
    let builder = Client::builder().timeout(LIST_WAIT);
    let client = http_client(builder).map_err(ListError::Other)?;
    let api = api.trim_end_matches('/');
    let url = format!("{api}/models");
    let failed = |reason: &str| prompt::failure("models", &url, reason);
    let mut request = client.get(&url);
    if real_key(key) {
        request = request.bearer_auth(key);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            let reason = http_cause(&e);
            let message = failed(&reason);
            let error = match e.is_connect() {
                true => ListError::Address(message),
                false => ListError::Other(message),
            };
            return Err(error);
        }
    };

    let (mime, _) = content_type(response.headers());
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let said = said(status, &mime, &text);
        let code = status.as_u16();
        let reason = prompt::http_status(code, &said);
        let message = failed(&reason);
        let error = match code {
            401 | 403 => ListError::Key(message),
            404 => ListError::Address(message),
            _ => ListError::Other(message),
        };
        return Err(error);
    }

    let json: Value = serde_json::from_str(&text).unwrap_or_default();
    let Some(entries) = json["data"].as_array() else {
        let message = failed("not a model list");
        return Err(ListError::Address(message));
    };
    let mut offered = Vec::new();
    for entry in entries {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        // OpenAI's own list has no window, and other endpoints each name it their own way
        let windows = [
            &entry["context_window"],
            &entry["context_length"],
            &entry["max_context_length"],
            &entry["max_model_len"],
        ];
        let context = windows.into_iter().find_map(Value::as_u64);
        let model = Offered {
            id: id.to_owned(),
            context,
        };
        offered.push(model);
    }
    if offered.is_empty() {
        let message = failed("none listed");
        return Err(ListError::Other(message));
    }

    offered.sort_by(|a, b| a.id.cmp(&b.id));
    offered.dedup_by(|a, b| a.id == b.id);
    Ok(offered)
}

struct Stream {
    pending: Vec<u8>,
    usage: Option<TokenUsage>,
    finish: Option<Finish>,
    /// The call still being received, whose `eval` code is handed over as it arrives
    call: Option<Call>,
    /// Bytes of reply text handed over, where the next sealed item ends
    text: usize,
    sealed: Sealed,
    /// Only Responses breaks the text into paragraphs
    paragraphs: Paragraphs,
    /// Only Chat Completions sends fields the protocol doesn't define, which seal into one item
    gathered: Gathered,
}

#[derive(Default)]
struct Paragraphs {
    reasoned: bool,
    /// A summary part or reasoning item started after the last reasoning text
    parted: bool,
    said: bool,
    /// A message item started after the last text
    opened: bool,
}

#[derive(Default)]
struct Gathered {
    /// The reply's fields beyond those of the protocol
    extras: Map<String, Value>,
    /// The calls handed over, each with its id and fields beyond those of the protocol
    called: Vec<Map<String, Value>>,
}

#[derive(Default)]
struct Call {
    index: u64,
    id: String,
    name: String,
    arguments: String,
    code: Decoding,
    extras: Map<String, Value>,
}

#[derive(Default)]
struct Decoding {
    /// The next byte of the arguments to decode, `None` until the opening quote is found
    at: Option<usize>,
    closed: bool,
}

impl Stream {
    fn new(kind: Kind, provider: &str) -> Self {
        let sealed = Sealed {
            provider: provider.to_owned(),
            kind,
            items: Vec::new(),
            ends: Vec::new(),
        };
        Self {
            pending: Vec::new(),
            usage: None,
            finish: None,
            call: None,
            text: 0,
            sealed,
            paragraphs: Paragraphs::default(),
            gathered: Gathered::default(),
        }
    }

    /// Returns true on `[DONE]` or the event that ends a response
    fn feed(
        &mut self,
        bytes: &[u8],
        on_delta: &mut dyn FnMut(StreamDelta),
    ) -> Result<bool, ModelError> {
        self.pending.extend_from_slice(bytes);

        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };

            // Lines of a CRLF stream end in \r
            let data = data.trim_ascii();
            if data == b"[DONE]" {
                self.finish.get_or_insert(Finish::Stop);
                self.hand_over(on_delta);
                return Ok(true);
            }

            // A stream that breaks off mid-line also fails to parse here
            let chunk: Value = match serde_json::from_slice(data) {
                Ok(chunk) => chunk,
                Err(e) => {
                    let message = match e.is_eof() {
                        true => "stream: ended mid-event".to_owned(),
                        false => {
                            let raw = String::from_utf8_lossy(data);
                            let said = excerpt(&raw);
                            format!("stream: not JSON: {said}")
                        }
                    };
                    return Err(ModelError::transient(message));
                }
            };

            let ended = match self.sealed.kind {
                Kind::Completions => self.chunk(&chunk, data, on_delta).map(|()| false),
                Kind::Responses => self.event(&chunk, data, on_delta),
            };
            if ended? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// The `eval` code the arguments complete is handed over as it arrives
    fn add_arguments(&mut self, arguments: &str, on_delta: &mut dyn FnMut(StreamDelta)) {
        let Some(call) = &mut self.call else {
            return;
        };
        call.arguments.push_str(arguments);
        if call.name != TOOL {
            return;
        }

        let decoded = decode(&call.arguments, ARGUMENT, &mut call.code);
        if !decoded.is_empty() {
            let delta = StreamDelta::CallCode(&decoded);
            on_delta(delta);
        }
    }

    /// The last call received is `eval` and its code is complete
    fn written(&self) -> bool {
        let call = self.call.as_ref();
        call.is_some_and(|call| call.name == TOOL && call.code.closed)
    }

    /// A cut-off reply has incomplete arguments, so the call is withheld and reported as cut off
    fn hand_over(&mut self, on_delta: &mut dyn FnMut(StreamDelta)) {
        let Some(mut call) = self.call.take() else {
            return;
        };
        if self.finish == Some(Finish::Length) {
            return;
        }

        // Decoded as while streaming,
        // keeping escapes JSON rejects and bare control characters as written
        let ours = call.name == TOOL;
        let decoded = match ours {
            true => decode(&call.arguments, ARGUMENT, &mut call.code),
            false => String::new(),
        };
        // Models often misname the argument, so a lone string argument still counts as code
        let renamed = match ours && call.code.at.is_none() {
            true => lone_string(&call.arguments),
            false => None,
        };
        let read = call.code.closed || renamed.is_some();
        let rest = renamed.unwrap_or(decoded);
        // When the server stops for another reason,
        // an incomplete call isn't malformed and the stop reason is reported
        let stopped = matches!(self.finish, Some(Finish::Other(_)));
        if stopped && !read {
            return;
        }

        if read && !rest.is_empty() {
            let delta = StreamDelta::CallCode(&rest);
            on_delta(delta);
        }
        if self.sealed.kind == Kind::Completions {
            let mut kept = mem::take(&mut call.extras);
            if !call.id.is_empty() {
                kept.insert("id".to_owned(), json!(call.id));
            }
            self.gathered.called.push(kept);
        }

        let ended = match read {
            true => Ok(()),
            false => Err(Unread {
                name: &call.name,
                arguments: &call.arguments,
            }),
        };
        let delta = StreamDelta::CallEnd(ended);
        on_delta(delta);
    }

    /// Hands over reply text, which moves where the next sealed item ends
    fn say(&mut self, text: &str, on_delta: &mut dyn FnMut(StreamDelta)) {
        self.text += text.len();
        on_delta(StreamDelta::Content(text));
    }

    /// Seals an item that ends where the text handed over so far does
    fn seal_item(&mut self, item: Value, on_delta: &mut dyn FnMut(StreamDelta)) {
        self.sealed.items.push(item);
        self.sealed.ends.push(self.text);
        on_delta(StreamDelta::Sealed(&self.sealed));
    }

    /// Seals the fields Chat Completions gathered as one item shaped like the reply,
    /// whose `tool_calls` hold those of every call in order
    fn seal(&mut self, on_delta: &mut dyn FnMut(StreamDelta)) {
        let gathered = &mut self.gathered;
        let extra = |call: &Map<String, Value>| call.keys().any(|key| key != "id");
        if gathered.extras.is_empty() && !gathered.called.iter().any(extra) {
            return;
        }

        let mut reply = mem::take(&mut gathered.extras);
        let called = mem::take(&mut gathered.called);
        if !called.is_empty() {
            let called = called.into_iter().map(Value::Object).collect();
            reply.insert("tool_calls".to_owned(), Value::Array(called));
        }
        let item = Value::Object(reply);
        self.seal_item(item, on_delta);
    }

    fn finished(self) -> Result<Finished, ModelError> {
        let Some(finish) = self.finish else {
            let message = "stream: ended early".to_owned();
            return Err(ModelError::transient(message));
        };

        Ok(Finished {
            finish,
            usage: self.usage,
        })
    }
}

/// Stops before an escape that hasn't fully arrived and waits for later chunks
fn decode(arguments: &str, field: &str, decoding: &mut Decoding) -> String {
    let mut decoded = String::new();
    if decoding.closed {
        return decoded;
    }

    let at = match decoding.at {
        Some(at) => at,
        None => {
            let quoted = format!("\"{field}\"");
            let Some(start) = arguments.find(&quoted) else {
                return decoded;
            };
            let after = arguments[start + quoted.len()..].trim_start();
            let Some(rest) = after.strip_prefix(':') else {
                return decoded;
            };
            let Some(text) = rest.trim_start().strip_prefix('"') else {
                return decoded;
            };
            arguments.len() - text.len()
        }
    };

    let rest = &arguments[at..];
    let mut used = 0;
    while let Some(c) = rest[used..].chars().next() {
        match c {
            '"' => {
                decoding.closed = true;
                used += 1;
                break;
            }
            '\\' => {
                let Some((escaped, length)) = unescaped(&rest[used..]) else {
                    break;
                };
                decoded.push(escaped);
                used += length;
            }
            c => {
                decoded.push(c);
                used += c.len_utf8();
            }
        }
    }

    decoding.at = Some(at + used);
    decoded
}

/// Avoids serde_json because code often holds escapes like `\d` that JSON rejects
fn lone_string(arguments: &str) -> Option<String> {
    let inside = arguments.trim().strip_prefix('{')?;
    let named = inside.trim_start().strip_prefix('"')?;
    let (field, _) = named.split_once('"')?;

    let mut decoding = Decoding::default();
    let code = decode(arguments, field, &mut decoding);
    let at = decoding.at.filter(|_| decoding.closed)?;
    let after = arguments[at..].trim();
    (after == "}").then_some(code)
}

/// `text` starts with a backslash, and the result is `None` until the escape has fully arrived
fn unescaped(text: &str) -> Option<(char, usize)> {
    let next = text[1..].chars().next()?;
    let simple = match next {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'u' => {
            let Some(hex) = text.get(2..6) else {
                return (text.len() >= 6).then_some((char::REPLACEMENT_CHARACTER, 2));
            };
            let Ok(high) = u32::from_str_radix(hex, 16) else {
                return Some((char::REPLACEMENT_CHARACTER, 6));
            };
            if !(0xD800..0xDC00).contains(&high) {
                let c = char::from_u32(high).unwrap_or(char::REPLACEMENT_CHARACTER);
                return Some((c, 6));
            }

            // A lone high surrogate not followed by `\u` stops waiting for the low one
            let after = &text[6..];
            if !after.starts_with("\\u") && !"\\u".starts_with(after) {
                return Some((char::REPLACEMENT_CHARACTER, 6));
            }
            let Some(low) = text.get(6..12) else {
                return (text.len() >= 12).then_some((char::REPLACEMENT_CHARACTER, 6));
            };
            let low = u32::from_str_radix(&low[2..], 16);
            let Ok(low @ 0xDC00..=0xDFFF) = low else {
                return Some((char::REPLACEMENT_CHARACTER, 6));
            };
            let joined = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
            let c = char::from_u32(joined).unwrap_or(char::REPLACEMENT_CHARACTER);
            return Some((c, 12));
        }
        '"' | '\\' | '/' => next,
        // A non-JSON escape like a regex `\d` keeps its backslash,
        // and the next character stands alone
        _ => return Some(('\\', 1)),
    };

    let length = 1 + next.len_utf8();
    Some((simple, length))
}

/// An empty key and `none` both stand for no key
fn real_key(key: &str) -> bool {
    !key.is_empty() && key != "none"
}

/// What a failed response says, except an error page from a web server or proxy,
/// which is only useful for its status
fn said(status: StatusCode, mime: &str, text: &str) -> String {
    let json: Value = serde_json::from_str(text).unwrap_or_default();
    match (json.is_null(), mime) {
        (true, "text/html") => status.canonical_reason().unwrap_or_default().to_owned(),
        _ => error_text(&json, text),
    }
}

fn error_text(json: &Value, raw: &str) -> String {
    let fields = [
        &json["error"]["message"],
        &json["error"],
        &json["message"],
        &json["detail"],
    ];
    let said = fields.into_iter().find_map(Value::as_str).unwrap_or(raw);
    excerpt(said)
}

fn retryable(code: u16) -> bool {
    matches!(code, 408 | 409 | 429) || code >= 500
}

fn network_error(error: &reqwest::Error) -> ModelError {
    let timeout = error.is_timeout();
    let connect = error.is_connect();
    // `chunk()` wraps every read error as `Decode`,
    // which without decompression means the network dropped
    let body = error.is_body() || error.is_decode();

    let reason = read_cause(error, IDLE_WAIT);
    let message = match body {
        true => format!("stream: {reason}"),
        false => reason,
    };

    let transient = timeout || connect || error.is_request() || body;
    ModelError {
        message,
        transient,
        retry_after: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::model::*;
    use crate::utils::block_on;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    pub(crate) const DONE: &str = "HTTP/1.1 200 OK\r\ncontent-length: 14\r\n\r\ndata: [DONE]\n\n";

    /// Reaches `api` without a key, and reads the copies of media from where their names lead
    pub(crate) fn mocked(kind: Kind, api: String, spec: &ModelSpec) -> OpenAIModel {
        let provider = Provider {
            kind,
            api: Some(api),
            api_key: None,
            models: Vec::new(),
        };
        let built = OpenAIModel::new("mock", &provider, spec.clone(), PathBuf::new());
        built.expect("model")
    }

    pub(crate) fn png(file: &str) -> Media {
        Media {
            kind: MediaKind::Image,
            mime: "image/png".to_owned(),
            file: PathBuf::from(file),
            from_call: false,
        }
    }

    /// Feeds the body in 5-byte pieces that split both lines and multibyte characters
    pub(crate) fn parsed(
        kind: Kind,
        lines: &[&str],
    ) -> (Result<Finished, ModelError>, String, String) {
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let mut stream = Stream::new(kind, "mock");
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut on_delta = |delta: StreamDelta| match delta {
            StreamDelta::Content(text) => content.push_str(text),
            StreamDelta::Reasoning(text) => reasoning.push_str(text),
            StreamDelta::CallCode(code) => content.push_str(code),
            StreamDelta::CallEnd(Ok(())) => content.push_str("[end]"),
            StreamDelta::CallEnd(Err(unread)) => {
                let end = format!("[{} {}]", unread.name, unread.arguments);
                content.push_str(&end);
            }
            StreamDelta::Sealed(sealed) => {
                let item = sealed.items.last().expect("item");
                let mark = format!("[{} {item}]", sealed.provider);
                content.push_str(&mark);
            }
        };

        let mut fed = Ok(false);
        for bytes in body.as_bytes().chunks(5) {
            fed = stream.feed(bytes, &mut on_delta);
            if !matches!(fed, Ok(false)) {
                break;
            }
        }
        if fed.is_ok() {
            stream.seal(&mut on_delta);
        }
        let finished = fed.and_then(|_| stream.finished());

        (finished, content, reasoning)
    }

    /// Replies once with `response`, shuts the write side and returns the request body it received
    pub(crate) fn served(
        kind: Kind,
        response: &str,
        spec: &ModelSpec,
        conversation: &[ChatMessage],
        purpose: Purpose,
    ) -> (Result<Finished, ModelError>, Value) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let response = response.to_owned();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept");
            // hyper ignores a response arriving before the request is sent, and the body is JSON
            let mut request = Vec::new();
            let mut buffer = [0; 8192];
            while !request.ends_with(b"}") {
                let Ok(read @ 1..) = connection.read(&mut buffer) else {
                    break;
                };
                request.extend_from_slice(&buffer[..read]);
            }

            connection.write_all(response.as_bytes()).ok();
            connection.shutdown(Shutdown::Write).ok();
            while connection.read(&mut buffer).is_ok_and(|read| read > 0) {}
            request
        });

        let api = format!("http://{address}/v1");
        let model = mocked(kind, api, spec);
        let mut ignore = |_: StreamDelta| {};
        let generating = model.generate(conversation, purpose, &mut ignore);
        let finished = block_on(generating);

        let request = server.join().expect("server");
        let start = request.windows(4).position(|window| window == b"\r\n\r\n");
        let start = start.expect("headers") + 4;
        let body = serde_json::from_slice(&request[start..]).expect("body");
        (finished, body)
    }

    pub(crate) fn hello() -> [ChatMessage; 1] {
        [ChatMessage::user("Hello")]
    }

    #[test]
    fn broken_stream() {
        let conversation = hello();
        let spec = ModelSpec::default();
        let broken = |data: &str| {
            let chunk = format!("{data}\n\n");
            let length = chunk.len();
            // No terminating empty chunk, so the connection drops right after `chunk`
            let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{length:x}\r\n{chunk}\r\n");
            let (finished, _) = served(
                Kind::Completions,
                &response,
                &spec,
                &conversation,
                Purpose::Step,
            );
            finished
        };

        let stopped = r#"data: {"choices":[{"delta":{"content":"Sure"},"finish_reason":"stop"}]}"#;
        assert_eq!(broken(stopped).expect("finished").finish, Finish::Stop);

        let call = |index: u64, arguments: &str| {
            let call =
                json!({ "index": index, "function": { "name": "eval", "arguments": arguments } });
            let chunk = json!({ "choices": [{ "delta": { "tool_calls": [call] } }] });
            format!("data: {chunk}")
        };
        let called = call(0, r#"{"code": "print('café')"}"#);
        assert_eq!(broken(&called).expect("handed over").finish, Finish::Stop);
        let half = call(1, r#"{"code": "print('"#);
        let unfinished = format!("{called}\n\n{half}");
        let error = broken(&unfinished).expect_err("unfinished");
        assert!(error.transient);

        // The connection closes cleanly but the last event is only half there
        let ended = format!("{called}\n\ndata: {{\"choices\":[{{\"delta\":");
        let length = ended.len();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\n\r\n{ended}");
        let (finished, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        assert_eq!(finished.expect("ended").finish, Finish::Stop);

        let partial = r#"data: {"choices":[{"delta":{"content":"Halfway through"}}]}"#;
        let error = broken(partial).expect_err("broken");
        assert!(error.message.starts_with("stream: "), "{error}");
        assert!(error.transient);
    }

    #[test]
    fn from_call_optional() {
        let saved = r#"{"kind": "image", "mime": "image/png", "file": "chart.png"}"#;
        let old: Media = serde_json::from_str(saved).expect("deserialize");
        assert_eq!(old, png("chart.png"));
        let resaved = serde_json::to_string(&old).expect("serialize");
        assert!(!resaved.contains("from_call"), "{resaved}");
    }
}
