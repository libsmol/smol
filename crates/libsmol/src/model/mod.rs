use crate::config::{Kind, ModelSpec};
use crate::prompt::{self, ARGUMENT, TOOL};
use crate::tools::attach::read_copy;
use crate::utils::{excerpt, http_cause, http_client, read_cause};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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

/// A reply's output items in order, which only the provider that wrote them reads back,
/// without the text and calls the step holds itself
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Sealed {
    /// The `[model.<name>]` table, since no other provider can open them
    pub(crate) provider: String,
    pub(crate) items: Vec<Value>,
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
    /// The display name and the media, whose file is read only when a request is sent
    Media(String, Media),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
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
    /// An output item of the reply, without the text and call it holds
    Sealed {
        provider: &'a str,
        item: &'a Value,
    },
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
    provider: String,
    kind: Kind,
    id: String,
    api: String,
    key: Option<String>,
    effort: Option<String>,
    summary: Option<String>,
    window: Option<u64>,
    max_tokens: Option<u64>,
    image: bool,
    video: bool,
}

impl OpenAIModel {
    pub(crate) fn new(
        provider: &str,
        spec: &ModelSpec,
        kind: Kind,
        api: Option<String>,
        key: Option<String>,
    ) -> Result<Self, ModelError> {
        // A key from the environment only goes to the default OpenAI endpoint
        let key = match api.is_some() {
            true => key,
            false => key.or_else(|| env::var("OPENAI_API_KEY").ok()),
        };
        let key = key.filter(|key| real_key(key));
        let api = api.unwrap_or_else(|| DEFAULT_API.to_owned());

        let builder = Client::builder()
            .read_timeout(IDLE_WAIT)
            .tcp_keepalive(KEEPALIVE)
            .tcp_keepalive_interval(KEEPALIVE_INTERVAL)
            .tcp_keepalive_retries(KEEPALIVE_RETRIES);
        let client = http_client(builder).map_err(ModelError::new)?;

        Ok(Self {
            client,
            provider: provider.to_owned(),
            kind,
            id: spec.id.clone(),
            api,
            key,
            effort: spec.effort.clone(),
            summary: spec.summary.clone(),
            window: spec.context,
            max_tokens: spec.max_tokens,
            image: spec.image,
            video: spec.video,
        })
    }

    /// Leaving it out still has the server reason at its default effort
    fn effort_for(&self, purpose: Purpose) -> Option<&str> {
        self.effort.as_deref().filter(|_| purpose != Purpose::Title)
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

                let accepted = self.takes(media.kind);
                let read = accepted.then(|| read_copy(&media.file, media.kind));
                let bytes = match read {
                    Some(Ok(bytes)) => bytes,
                    Some(Err(_)) => {
                        let note = prompt::missing(label, media);
                        lines.push(note);
                        continue;
                    }
                    None => {
                        let note = prompt::unseen(label, media);
                        lines.push(note);
                        continue;
                    }
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
        let (path, body) = match self.kind {
            Kind::Completions => {
                let body = completions::body(self, conversation, purpose);
                ("chat/completions", body)
            }
            Kind::Responses => {
                let body = responses::body(self, conversation, purpose);
                ("responses", body)
            }
        };

        let api = self.api.trim_end_matches('/');
        let url = format!("{api}/{path}");
        let mut request = self.client.post(&url).json(&body);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }

        // Dropping the future drops the request and its connection, so the server stops generating
        Box::pin(async move {
            let mut response = match request.send().await {
                Ok(response) => response,
                Err(e) => return Err(network_error(&e)),
            };

            let header = response.headers().get(CONTENT_TYPE);
            let header = header
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let (mime, _) = header.split_once(';').unwrap_or((header, ""));
            let mime = mime.trim().to_ascii_lowercase();

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
                let json: Value = serde_json::from_str(&text).unwrap_or_default();
                // An error page from a fronting proxy is only useful for its status
                let said = match (json.is_null(), mime.as_str()) {
                    (true, "text/html") => status.canonical_reason().unwrap_or_default().to_owned(),
                    _ => error_text(&json, &text),
                };

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
            stream.finished()
        })
    }

    fn context_window(&self) -> Option<u64> {
        self.window
    }

    fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    fn takes(&self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Image => self.image,
            MediaKind::Video => self.video,
        }
    }
}

/// Adjacent user messages are merged into one
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

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or_default();
    if !status.is_success() {
        // A page from a web server or proxy is only useful for its status
        let said = match json.is_null() {
            true => status.canonical_reason().unwrap_or_default().to_owned(),
            false => error_text(&json, &text),
        };
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
    kind: Kind,
    provider: String,
    pending: Vec<u8>,
    usage: Option<TokenUsage>,
    finish: Option<Finish>,
    /// The call still being received, whose `eval` code is handed over as it arrives
    call: Option<Call>,
    reasoned: bool,
    /// A summary part or reasoning item started after the last reasoning text
    parted: bool,
}

#[derive(Default)]
struct Call {
    index: u64,
    id: String,
    name: String,
    arguments: String,
    code: Decoding,
}

#[derive(Default)]
struct Decoding {
    /// The next byte of the arguments to decode, `None` until the opening quote is found
    at: Option<usize>,
    closed: bool,
}

impl Stream {
    fn new(kind: Kind, provider: &str) -> Self {
        Self {
            kind,
            provider: provider.to_owned(),
            pending: Vec::new(),
            usage: None,
            finish: None,
            call: None,
            reasoned: false,
            parted: false,
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

            let ended = match self.kind {
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
            StreamDelta::Sealed { provider, item } => {
                let mark = format!("[{provider} {item}]");
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
        let model = OpenAIModel::new("mock", spec, kind, Some(api), None).expect("model");
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

    /// Answers one request with `response` and returns what was listed and the request head
    fn listing(response: &str, key: &str) -> (Result<Vec<Offered>, ListError>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let response = response.to_owned();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0; 8192];
            while !request.ends_with(b"\r\n\r\n") {
                let Ok(read @ 1..) = connection.read(&mut buffer) else {
                    break;
                };
                request.extend_from_slice(&buffer[..read]);
            }

            connection.write_all(response.as_bytes()).ok();
            connection.shutdown(Shutdown::Write).ok();
            while connection.read(&mut buffer).is_ok_and(|read| read > 0) {}
            String::from_utf8_lossy(&request).into_owned()
        });

        let api = format!("http://{address}/v1/");
        let listing = listed(&api, key);
        let result = block_on(listing);

        let request = server.join().expect("server");
        (result, request.to_lowercase())
    }

    fn reply(status: &str, mime: &str, body: &str) -> String {
        let length = body.len();
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {mime}\r\ncontent-length: {length}\r\n\r\n{body}"
        )
    }

    #[test]
    fn unescape_pieces() {
        let cases = [
            ("\\nüber", Some(('\n', 2))),
            ("\\\"résumé", Some(('"', 2))),
            ("\\d+café", Some(('\\', 1))),
            ("\\u00e9clair café", Some(('é', 6))),
            ("\\ud83d\\ude00 naïve", Some(('😀', 12))),
            ("\\uzzzz", Some(('\u{FFFD}', 6))),
            ("\\ud83d\\u00e9", Some(('\u{FFFD}', 6))),
            ("\\ud83d\"}", Some(('\u{FFFD}', 6))),
            ("\\u00e", None),
            ("\\ud83d\\ude", None),
            ("\\ud83d\\", None),
            ("\\", None),
        ];
        for (text, expected) in cases {
            assert_eq!(unescaped(text), expected, "{text}");
        }
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
    fn last_line_unterminated() {
        let conversation = hello();
        let spec = ModelSpec::default();
        let body = r#"data: {"choices":[{"delta":{"content":"Voilà"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#;
        let length = body.len();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\n\r\n{body}");
        let (finished, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        let expected = Finished {
            finish: Finish::Stop,
            usage: Some(TokenUsage::new(12, 3)),
        };
        assert_eq!(finished, Ok(expected));
    }

    #[test]
    fn page_refused() {
        let conversation = hello();
        let spec = ModelSpec::default();
        let page = "<html>Please sign in first</html>";
        let length = page.len();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {length}\r\n\r\n{page}");
        let (finished, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        let error = finished.expect_err("page");
        assert_eq!(error.message, "http 200: text/html, not an event stream");
        assert!(!error.transient);

        let body = r#"{"error":{"message":"The model does not exist"}}"#;
        let length = body.len();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {length}\r\n\r\n{body}");
        let (finished, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        let error = finished.expect_err("json");
        assert_eq!(error.message, "http 200: The model does not exist");
    }

    #[test]
    fn long_retry_after() {
        let conversation = hello();
        let spec = ModelSpec::default();
        let response =
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1e30\r\ncontent-length: 0\r\n\r\n";
        let (error, _) = served(
            Kind::Completions,
            response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        let error = error.expect_err("busy");
        assert!(error.transient);
        assert_eq!(error.retry_after, None);

        let body = r#"{"error":{"message":"Quota used up"}}"#;
        let length = body.len();
        let response = format!("HTTP/1.1 429 Too Many Requests\r\nretry-after: 120\r\ncontent-length: {length}\r\n\r\n{body}");
        let (error, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        let error = error.expect_err("quota");
        assert_eq!(error.message, "http 429: Quota used up (retry after 120s)");
        assert!(!error.transient);
    }

    #[test]
    fn error_page_reason() {
        let conversation = hello();
        let spec = ModelSpec::default();
        let page = "<html><head><title>503 Service Temporarily Unavailable</title></head><body>gateway</body></html>";
        let length = page.len();
        let response = format!("HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/html\r\ncontent-length: {length}\r\n\r\n{page}");
        let (error, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        let error = error.expect_err("page");
        assert_eq!(error.message, "http 503: Service Unavailable");
        assert!(error.transient);

        let response = format!("HTTP/1.1 524 Timeout\r\ncontent-type: text/html\r\ncontent-length: {length}\r\n\r\n{page}");
        let (error, _) = served(
            Kind::Completions,
            &response,
            &spec,
            &conversation,
            Purpose::Step,
        );
        assert_eq!(error.expect_err("page").message, "http 524");
    }

    #[test]
    fn from_call_optional() {
        let saved = r#"{"kind": "image", "mime": "image/png", "file": "chart.png"}"#;
        let old: Media = serde_json::from_str(saved).expect("deserialize");
        assert_eq!(old, png("chart.png"));
        let resaved = serde_json::to_string(&old).expect("serialize");
        assert!(!resaved.contains("from_call"), "{resaved}");
    }

    #[test]
    fn code_streamed() {
        let arguments = r#"{"code": "print(\"café\\t\")\nnaïve😀\\", "x": 1}"#;
        let parsed: Value = serde_json::from_str(arguments).expect("json");
        let expected = parsed["code"].as_str().expect("code");

        let mut code = Decoding::default();
        let mut decoded = String::new();
        let ends = arguments.char_indices().map(|(at, c)| at + c.len_utf8());
        for end in ends {
            let part = decode(&arguments[..end], ARGUMENT, &mut code);
            decoded.push_str(&part);
        }
        assert_eq!(decoded, expected);
        assert!(code.closed);
    }

    #[test]
    fn models_listed() {
        let body = r#"{"object":"list","data":[
            {"id":"zeta-piñata","context_window":1048576},
            {"id":"alpha-café","context_length":8192},
            {"id":"alpha-café"},
            {"id":"mid-naïve","max_model_len":32768},
            {"id":"mistral-crème","max_context_length":131072},
            {"id":"gpt-plain","owned_by":"system"},
            {"object":"model"}
        ]}"#;
        let response = reply("200 OK", "application/json", body);
        let (listed, request) = listing(&response, "sk-test");

        let offered = listed.expect("listed");
        let found: Vec<(&str, Option<u64>)> = offered
            .iter()
            .map(|model| (model.id.as_str(), model.context))
            .collect();
        let expected = [
            ("alpha-café", Some(8192)),
            ("gpt-plain", None),
            ("mid-naïve", Some(32768)),
            ("mistral-crème", Some(131072)),
            ("zeta-piñata", Some(1048576)),
        ];
        assert_eq!(found, expected);
        assert!(request.starts_with("get /v1/models http/1.1"), "{request}");
        assert!(
            request.contains("authorization: bearer sk-test"),
            "{request}"
        );

        for key in ["", "none"] {
            let (_, request) = listing(&response, key);
            assert!(!request.contains("authorization"), "{request}");
        }
    }

    #[test]
    fn listing_failures() {
        let refused = reply(
            "401 Unauthorized",
            "application/json",
            r#"{"error":{"message":"Invalid key for café"}}"#,
        );
        let missing = reply("404 Not Found", "text/plain", "404 page not found");
        let page = reply("200 OK", "text/html", "<html>Welcome to the café</html>");
        let empty = reply("200 OK", "application/json", r#"{"data":[]}"#);
        let broken = reply("500 Internal Server Error", "text/plain", "");
        let cases = [
            (refused, "http 401: Invalid key for café"),
            (missing, "http 404: Not Found"),
            (page, "not a model list"),
            (empty, "none listed"),
            (broken, "http 500: Internal Server Error"),
        ];

        let mut kinds = Vec::new();
        for (response, said) in cases {
            let (listed, _) = listing(&response, "sk-test");
            let error = listed.expect_err("error");
            let (kind, message) = match &error {
                ListError::Key(message) => ("key", message),
                ListError::Address(message) => ("address", message),
                ListError::Other(message) => ("other", message),
            };
            assert!(
                message.starts_with("models: http://127.0.0.1:"),
                "{message}"
            );
            assert!(
                message.ends_with(&format!("/v1/models: {said}")),
                "{message}"
            );
            kinds.push(kind);
        }
        assert_eq!(kinds, ["key", "address", "address", "other", "other"]);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let api = format!("http://{address}/v1");
        let listing = listed(&api, "");
        let error = block_on(listing).expect_err("closed");
        let ListError::Address(message) = error else {
            panic!("{error:?}");
        };
        let expected = format!("models: {api}/models: cannot connect to {address}:");
        assert!(message.starts_with(&expected), "{message}");
    }

    #[test]
    fn silent_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        thread::spawn(move || {
            let mut held = Vec::new();
            for connection in listener.incoming() {
                held.push(connection);
            }
        });

        let api = format!("http://{address}/v1");
        let spec = ModelSpec::default();
        let kind = Kind::Completions;
        let model = OpenAIModel::new("mock", &spec, kind, Some(api), None).expect("model");
        let conversation = hello();

        let mut ignore = |_: StreamDelta| {};
        let generating = model.generate(&conversation, Purpose::Step, &mut ignore);
        let waited = block_on(generating).expect_err("idle");
        assert_eq!(waited.message, "received no data for 1s");
        assert!(waited.transient);
    }
}
