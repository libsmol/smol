use crate::prompt::{self, TOOL};
use crate::tools::attach::read_copy;
use crate::utils::{excerpt, http_cause, http_client};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::env;
use std::future::Future;
use std::mem;
use std::ops::Not;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

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
const ARGUMENT: &str = "code";
/// Servers only accept tool names of letters, digits, `_` and `-` up to this length
const MAX_NAME_CHARS: usize = 64;
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
        calls: Vec<ToolCall>,
    },
    Tool {
        id: String,
        parts: Vec<Part>,
    },
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

    fn takes(&self, _kind: MediaKind) -> bool {
        false
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelSpec {
    pub(crate) id: String,
    pub(crate) effort: Option<String>,
    pub(crate) context: Option<u64>,
    /// DeepSeek counts reasoning toward it, and a small value cuts the reply off mid-reasoning
    pub(crate) max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) image: bool,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) video: bool,
}

pub(crate) struct OpenAIModel {
    client: Client,
    id: String,
    api: String,
    key: Option<String>,
    effort: Option<String>,
    window: Option<u64>,
    max_tokens: Option<u64>,
    image: bool,
    video: bool,
}

impl OpenAIModel {
    pub(crate) fn new(
        spec: &ModelSpec,
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
            id: spec.id.clone(),
            api,
            key,
            effort: spec.effort.clone(),
            window: spec.context,
            max_tokens: spec.max_tokens,
            image: spec.image,
            video: spec.video,
        })
    }

    /// Media follows the tag naming it, and `attached` takes images a tool message can't hold
    fn content(&self, messages: &[&[Part]], mut attached: Option<&mut Vec<Value>>) -> Value {
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
                let read = accepted.then(|| read_copy(&media.file));
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
                let kind = match media.kind {
                    MediaKind::Image => "image_url",
                    MediaKind::Video => "video_url",
                };
                let block = json!({ "type": kind, kind: { "url": url } });
                if let Some(attached) = attached.as_deref_mut() {
                    attached.push(json!({ "type": "text", "text": tag }));
                    attached.push(block);
                    lines.push(tag);
                    continue;
                }

                lines.push(tag);
                let text = mem::take(&mut lines).join("\n");
                blocks.push(json!({ "type": "text", "text": text }));
                blocks.push(block);
            }
        }

        // Without any media sent the content stays a plain string
        let text = lines.join("\n");
        match blocks.is_empty() {
            true => json!(text),
            false if lines.is_empty() => Value::Array(blocks),
            false => {
                blocks.push(json!({ "type": "text", "text": text }));
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
        // Adjacent user messages are merged into one
        let mut sent = Vec::new();
        // Tool messages can't hold images, which go in a user message after all the step's replies
        let mut attached = Vec::new();
        let together = |a: &ChatMessage, b: &ChatMessage| {
            matches!((a, b), (ChatMessage::User(_), ChatMessage::User(_)))
        };
        for group in conversation.chunk_by(together) {
            let message = match &group[0] {
                ChatMessage::System(text) => json!({ "role": "system", "content": text }),
                ChatMessage::User(_) => {
                    let mut merged = Vec::new();
                    for message in group {
                        if let ChatMessage::User(parts) = message {
                            merged.push(parts.as_slice());
                        }
                    }
                    let mut content = self.content(&merged, None);
                    // Images open the user message that follows the replies instead of a new one
                    if !attached.is_empty() {
                        let mut blocks = mem::take(&mut attached);
                        match content {
                            Value::Array(rest) => blocks.extend(rest),
                            text => blocks.push(json!({ "type": "text", "text": text })),
                        }
                        content = Value::Array(blocks);
                    }
                    json!({ "role": "user", "content": content })
                }
                ChatMessage::Assistant {
                    text,
                    reasoning,
                    calls,
                } => {
                    let content = match text.is_empty() && !calls.is_empty() {
                        true => Value::Null,
                        false => json!(text),
                    };
                    let mut message = json!({ "role": "assistant", "content": content });
                    if let Some(reasoning) = reasoning {
                        message["reasoning_content"] = json!(reasoning);
                    }

                    let mut called = Vec::new();
                    for call in calls {
                        let function = json!({ "name": call.name, "arguments": call.arguments });
                        let call =
                            json!({ "id": call.id, "type": "function", "function": function });
                        called.push(call);
                    }
                    if !called.is_empty() {
                        message["tool_calls"] = Value::Array(called);
                    }
                    message
                }
                ChatMessage::Tool { id, parts } => {
                    let content = self.content(&[parts.as_slice()], Some(&mut attached));
                    json!({ "role": "tool", "content": content, "tool_call_id": id })
                }
            };

            let answering = matches!(group[0], ChatMessage::Tool { .. });
            if !answering && !attached.is_empty() {
                let images = mem::take(&mut attached);
                sent.push(json!({ "role": "user", "content": images }));
            }
            sent.push(message);
        }
        if !attached.is_empty() {
            sent.push(json!({ "role": "user", "content": attached }));
        }

        let mut body = Map::new();
        // Leaving out the effort still has the server reason at its default effort
        let effort = self.effort.as_ref().filter(|_| purpose != Purpose::Title);
        if let Some(effort) = effort {
            body.insert("reasoning_effort".into(), json!(effort));
        }
        body.insert("model".into(), json!(self.id));
        body.insert("messages".into(), Value::Array(sent));
        if let Some(max_tokens) = self.max_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));
        if purpose == Purpose::Step {
            let code = json!({ "type": "string", "description": "The Python code to run." });
            let parameters = json!({
                "type": "object",
                "properties": { ARGUMENT: code },
                "required": [ARGUMENT],
            });
            let function =
                json!({ "name": TOOL, "description": prompt::TOOL_DOC, "parameters": parameters });
            let tool = json!({ "type": "function", "function": function });
            body.insert("tools".into(), json!([tool]));
        }

        let api = self.api.trim_end_matches('/');
        let url = format!("{api}/chat/completions");
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

            let mut stream = Stream::default();
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

    fn takes(&self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Image => self.image,
            MediaKind::Video => self.video,
        }
    }
}

/// Whether the key, the address or something else made listing fail
#[derive(Debug, PartialEq)]
pub(crate) enum ListError {
    Key(String),
    Address(String),
    Other(String),
}

pub(crate) async fn listed(api: &str, key: &str) -> Result<Vec<ModelSpec>, ListError> {
    let builder = Client::builder().timeout(LIST_WAIT);
    let client = http_client(builder).map_err(ListError::Other)?;
    let api = api.trim_end_matches('/');
    let url = format!("{api}/models");
    let mut request = client.get(&url);
    if real_key(key) {
        request = request.bearer_auth(key);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            let reason = http_cause(&e);
            let message = format!("models: {url}: {reason}");
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
        let message = format!("models: {url}: {reason}");
        let error = match code {
            401 | 403 => ListError::Key(message),
            404 => ListError::Address(message),
            _ => ListError::Other(message),
        };
        return Err(error);
    }

    let Some(entries) = json["data"].as_array() else {
        let message = format!("models: {url}: not a model list");
        return Err(ListError::Address(message));
    };
    let mut specs = Vec::new();
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
        let spec = ModelSpec {
            id: id.to_owned(),
            context,
            ..ModelSpec::default()
        };
        specs.push(spec);
    }
    if specs.is_empty() {
        let message = format!("models: {url}: none listed");
        return Err(ListError::Other(message));
    }

    specs.sort_by(|a, b| a.id.cmp(&b.id));
    specs.dedup_by(|a, b| a.id == b.id);
    Ok(specs)
}

#[derive(Default)]
struct Stream {
    pending: Vec<u8>,
    usage: Option<TokenUsage>,
    finish: Option<Finish>,
    /// The call still being received, whose `eval` code is handed over as it arrives
    call: Option<Call>,
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
    /// Returns true on `[DONE]`
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

            // After a 200 it's a transient upstream error unless `code` blames the request
            let error = &chunk["error"];
            if !error.is_null() {
                let raw = String::from_utf8_lossy(data);
                let said = error_text(&chunk, &raw);
                let message = format!("stream: {said}");

                let code = error["code"].as_u64();
                let code = code.and_then(|code| u16::try_from(code).ok());
                let transient = code.is_none_or(retryable);
                return Err(ModelError {
                    message,
                    transient,
                    retry_after: None,
                });
            }

            let choice = &chunk["choices"][0];
            if let Some(reason) = choice["finish_reason"].as_str() {
                let reason = match reason {
                    "stop" | "end" | "tool_calls" | "function_call" => Finish::Stop,
                    "length" => Finish::Length,
                    other => Finish::Other(other.to_owned()),
                };
                self.finish = Some(reason);
            }

            let reported = &chunk["usage"];
            if reported.is_object() {
                let input = reported["prompt_tokens"].as_u64().unwrap_or(0);
                let output = reported["completion_tokens"].as_u64().unwrap_or(0);
                let mut usage = TokenUsage::new(input, output);
                usage.cached_tokens = reported["prompt_tokens_details"]["cached_tokens"].as_u64();
                self.usage = Some(usage);
            }

            let delta = &choice["delta"];
            let thinking = delta["reasoning_content"].as_str();
            let fallback = delta["reasoning"].as_str();
            // Keep empty reasoning since DeepSeek returns 400
            // if this turn's calls come back without it
            let reasoning = match thinking {
                Some("") | None => fallback.or(thinking),
                thinking => thinking,
            };
            let content = delta["content"].as_str().filter(|text| !text.is_empty());
            let deltas = [
                reasoning.map(StreamDelta::Reasoning),
                content.map(StreamDelta::Content),
            ];
            for streamed in deltas.into_iter().flatten() {
                on_delta(streamed);
            }

            if let Some(parts) = delta["tool_calls"].as_array() {
                for part in parts {
                    let index = part["index"].as_u64().unwrap_or(0);
                    let id = part["id"].as_str().unwrap_or_default();
                    let function = &part["function"];
                    let name = function["name"].as_str().unwrap_or_default();
                    let arguments = function["arguments"].as_str().unwrap_or_default();

                    // Calls arrive in sequence, so the previous one is done once the next starts
                    // Some providers keep `index` at 0 and some proxies change ids per chunk,
                    // so only a new id with a name starts a call
                    let current = match &self.call {
                        Some(call) if call.index == index => {
                            let opens = !id.is_empty() && !name.is_empty();
                            !opens || call.id.is_empty() || call.id == id
                        }
                        _ => false,
                    };
                    if !current {
                        self.hand_over(on_delta);
                    }

                    let started = Call {
                        index,
                        ..Call::default()
                    };
                    let call = self.call.get_or_insert(started);
                    if call.id.is_empty() {
                        call.id.push_str(id);
                    }
                    // Some providers repeat the full name in every chunk
                    if call.name != name {
                        call.name.push_str(name);
                    }
                    call.arguments.push_str(arguments);

                    if call.name != TOOL {
                        continue;
                    }

                    let decoded = decode(&call.arguments, ARGUMENT, &mut call.code);
                    if !decoded.is_empty() {
                        let delta = StreamDelta::CallCode(&decoded);
                        on_delta(delta);
                    }
                }
            }

            if self.finish.is_some() {
                self.hand_over(on_delta);
            }
        }

        Ok(false)
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
fn decode(arguments: &str, field: &str, value: &mut Decoding) -> String {
    let mut decoded = String::new();
    if value.closed {
        return decoded;
    }

    let at = match value.at {
        Some(at) => at,
        None => {
            let field = format!("\"{field}\"");
            let Some(key) = arguments.find(&field) else {
                return decoded;
            };
            let after = arguments[key + field.len()..].trim_start();
            let Some(value) = after.strip_prefix(':') else {
                return decoded;
            };
            let Some(text) = value.trim_start().strip_prefix('"') else {
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
                value.closed = true;
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

    value.at = Some(at + used);
    decoded
}

/// Avoids serde_json because code often holds escapes like `\d` that JSON rejects
fn lone_string(arguments: &str) -> Option<String> {
    let inside = arguments.trim().strip_prefix('{')?;
    let named = inside.trim_start().strip_prefix('"')?;
    let (field, _) = named.split_once('"')?;

    let mut value = Decoding::default();
    let code = decode(arguments, field, &mut value);
    let at = value.at.filter(|_| value.closed)?;
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

    // There is no overall timeout, so any timeout outside connecting is a read timeout
    let reason = match timeout && !connect {
        true => {
            let seconds = IDLE_WAIT.as_secs();
            format!("timed out after {seconds}s without data")
        }
        false => http_cause(error),
    };
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
    use crate::tests::{temp_dir, write_file};
    use crate::utils::block_on;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    const DONE: &str = "HTTP/1.1 200 OK\r\ncontent-length: 14\r\n\r\ndata: [DONE]\n\n";

    pub(crate) fn png(file: &str) -> Media {
        Media {
            kind: MediaKind::Image,
            mime: "image/png".to_owned(),
            file: PathBuf::from(file),
            from_call: false,
        }
    }

    /// Feeds the body in 5-byte pieces that split both lines and multibyte characters
    fn parsed(lines: &[&str]) -> (Result<Finished, ModelError>, String, String) {
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let mut stream = Stream::default();
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
    fn served(
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
        let model = OpenAIModel::new(spec, Some(api), None).expect("model");
        let mut ignore = |_: StreamDelta| {};
        let generating = model.generate(conversation, purpose, &mut ignore);
        let finished = block_on(generating);

        let request = server.join().expect("server");
        let start = request.windows(4).position(|window| window == b"\r\n\r\n");
        let start = start.expect("headers") + 4;
        let body = serde_json::from_slice(&request[start..]).expect("body");
        (finished, body)
    }

    fn hello() -> [ChatMessage; 1] {
        [ChatMessage::user("Hello")]
    }

    /// Answers one request with `response` and returns what was listed and the request head
    fn listing(response: &str, key: &str) -> (Result<Vec<ModelSpec>, ListError>, String) {
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
            let (finished, _) = served(&response, &spec, &conversation, Purpose::Step);
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
        let (finished, _) = served(&response, &spec, &conversation, Purpose::Step);
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
        let (finished, _) = served(&response, &spec, &conversation, Purpose::Step);
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
        let (finished, _) = served(&response, &spec, &conversation, Purpose::Step);
        let error = finished.expect_err("page");
        assert_eq!(error.message, "http 200: text/html, not an event stream");
        assert!(!error.transient);

        let body = r#"{"error":{"message":"The model does not exist"}}"#;
        let length = body.len();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {length}\r\n\r\n{body}");
        let (finished, _) = served(&response, &spec, &conversation, Purpose::Step);
        let error = finished.expect_err("json");
        assert_eq!(error.message, "http 200: The model does not exist");
    }

    #[test]
    fn long_retry_after() {
        let conversation = hello();
        let spec = ModelSpec::default();
        let response =
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1e30\r\ncontent-length: 0\r\n\r\n";
        let (error, _) = served(response, &spec, &conversation, Purpose::Step);
        let error = error.expect_err("busy");
        assert!(error.transient);
        assert_eq!(error.retry_after, None);

        let body = r#"{"error":{"message":"Quota used up"}}"#;
        let length = body.len();
        let response = format!("HTTP/1.1 429 Too Many Requests\r\nretry-after: 120\r\ncontent-length: {length}\r\n\r\n{body}");
        let (error, _) = served(&response, &spec, &conversation, Purpose::Step);
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
        let (error, _) = served(&response, &spec, &conversation, Purpose::Step);
        let error = error.expect_err("page");
        assert_eq!(error.message, "http 503: Service Unavailable");
        assert!(error.transient);

        let response = format!("HTTP/1.1 524 Timeout\r\ncontent-type: text/html\r\ncontent-length: {length}\r\n\r\n{page}");
        let (error, _) = served(&response, &spec, &conversation, Purpose::Step);
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
    fn media_parts() {
        let dir = temp_dir("parts");
        let picture = dir.join("chart.png");
        write_file(&picture, b"png");
        let media = |label: &str, kind, mime: &str, file: PathBuf| {
            let mime = mime.to_owned();
            let media = Media {
                kind,
                mime,
                file,
                from_call: false,
            };
            Part::Media(label.to_owned(), media)
        };
        let image = media("chart.png", MediaKind::Image, "image/png", picture);
        let video = media(
            "clip.mp4",
            MediaKind::Video,
            "video/mp4",
            dir.join("clip.mp4"),
        );
        let gone = media(
            "old.png",
            MediaKind::Image,
            "image/png",
            dir.join("gone.png"),
        );

        let first = ChatMessage::User(vec![Part::Text("Look at this".to_owned()), image]);
        let second = ChatMessage::User(vec![Part::Text("And a video".to_owned()), video, gone]);
        let conversation = [first, second];

        let image = ModelSpec {
            image: true,
            ..ModelSpec::default()
        };
        let (finished, body) = served(DONE, &image, &conversation, Purpose::Step);
        finished.expect("finished");
        let expected = json!([
            {"type": "text", "text": "Look at this\n<image path=\"chart.png\"/>"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
            {"type": "text", "text": "And a video\nThe video clip.mp4 is not shown: the current model does not take videos.\nThe image old.png is not shown: its saved copy is gone."},
        ]);
        assert_eq!(body["messages"][0]["content"], expected);

        let text = ModelSpec::default();
        let (_, unseen) = served(DONE, &text, &conversation, Purpose::Step);
        let expected =
            "Look at this\nThe image chart.png is not shown: the current model does not take images.\n\n\
            And a video\nThe video clip.mp4 is not shown: the current model does not take videos.\n\
            The image old.png is not shown: the current model does not take images.";
        assert_eq!(unseen["messages"][0]["content"], expected);

        let both = ModelSpec {
            video: true,
            ..image
        };
        let greeting = hello();
        let (_, plain) = served(DONE, &both, &greeting, Purpose::Step);
        assert_eq!(plain["messages"][0]["content"], "Hello");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn request_options() {
        let conversation = hello();
        let spec = ModelSpec {
            effort: Some("max".to_owned()),
            ..ModelSpec::default()
        };
        let (_, step) = served(DONE, &spec, &conversation, Purpose::Step);
        assert_eq!(step["reasoning_effort"], "max");
        assert!(step.get("max_tokens").is_none(), "{step}");
        let function = &step["tools"][0]["function"];
        assert_eq!(function["name"], "eval");
        assert_eq!(function["parameters"]["required"][0], "code");

        let (_, summary) = served(DONE, &spec, &conversation, Purpose::Summary);
        assert_eq!(summary["reasoning_effort"], "max");
        assert!(summary.get("tools").is_none(), "{summary}");

        let (_, title) = served(DONE, &spec, &conversation, Purpose::Title);
        assert!(title.get("reasoning_effort").is_none(), "{title}");
        assert!(title.get("tools").is_none(), "{title}");

        let capped = ModelSpec {
            max_tokens: Some(65536),
            ..spec
        };
        let (_, capped) = served(DONE, &capped, &conversation, Purpose::Step);
        assert_eq!(capped["max_tokens"], 65536);
    }

    #[test]
    fn calls_sent() {
        let dir = temp_dir("calls");
        let picture = dir.join("chart.png");
        write_file(&picture, b"png");
        let media = Media {
            file: picture,
            ..png("chart.png")
        };

        let call = |id: &str, code: &str| ToolCall::eval(id.to_owned(), code);
        let asked = ChatMessage::Assistant {
            text: "Let me look.".to_owned(),
            reasoning: Some("Run it first".to_owned()),
            calls: vec![call("call_1", "print('café')")],
        };
        let printed = ChatMessage::Tool {
            id: "call_1".to_owned(),
            parts: vec![Part::Text("café".to_owned())],
        };
        let silent = ChatMessage::Assistant {
            text: String::new(),
            reasoning: None,
            calls: vec![
                call("call_2", "view('chart.png')"),
                call("call_3", "view('chart.png')"),
                ToolCall::unread("call_4".to_owned(), "functions.résumé"),
                ToolCall::unread("call_5".to_owned(), ""),
            ],
        };
        let viewed = Part::Media("chart.png".to_owned(), media);
        let quiet = Part::Text("(no output)".to_owned());
        let first = ChatMessage::Tool {
            id: "call_2".to_owned(),
            parts: vec![quiet, viewed.clone()],
        };
        let second = ChatMessage::Tool {
            id: "call_3".to_owned(),
            parts: vec![viewed],
        };
        let refused = |id: &str| ChatMessage::Tool {
            id: id.to_owned(),
            parts: vec![Part::Text("No such tool".to_owned())],
        };
        let update = ChatMessage::user("Task update:\nUse the café colors");
        let conversation = [
            asked,
            printed,
            silent,
            first,
            second,
            refused("call_4"),
            refused("call_5"),
            update,
        ];

        let spec = ModelSpec {
            image: true,
            ..ModelSpec::default()
        };
        let (_, body) = served(DONE, &spec, &conversation, Purpose::Step);
        let expected = json!([
            {
                "role": "assistant",
                "content": "Let me look.",
                "reasoning_content": "Run it first",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "eval", "arguments": "{\"code\":\"print('café')\"}"}}],
            },
            {"role": "tool", "content": "café", "tool_call_id": "call_1"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_2", "type": "function", "function": {"name": "eval", "arguments": "{\"code\":\"view('chart.png')\"}"}},
                    {"id": "call_3", "type": "function", "function": {"name": "eval", "arguments": "{\"code\":\"view('chart.png')\"}"}},
                    {"id": "call_4", "type": "function", "function": {"name": "functions_r_sum_", "arguments": "{}"}},
                    {"id": "call_5", "type": "function", "function": {"name": "_", "arguments": "{}"}},
                ],
            },
            {"role": "tool", "content": "(no output)\n<image path=\"chart.png\"/>", "tool_call_id": "call_2"},
            {"role": "tool", "content": "<image path=\"chart.png\"/>", "tool_call_id": "call_3"},
            {"role": "tool", "content": "No such tool", "tool_call_id": "call_4"},
            {"role": "tool", "content": "No such tool", "tool_call_id": "call_5"},
            {"role": "user", "content": [
                {"type": "text", "text": "<image path=\"chart.png\"/>"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                {"type": "text", "text": "<image path=\"chart.png\"/>"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                {"type": "text", "text": "Task update:\nUse the café colors"},
            ]},
        ]);
        assert_eq!(body["messages"], expected);

        // Without a following user message the images get one of their own
        let (_, ended) = served(DONE, &spec, &conversation[..7], Purpose::Step);
        let images = &expected[7]["content"].as_array().expect("blocks")[..4];
        let alone = json!({ "role": "user", "content": images });
        assert_eq!(ended["messages"][7], alone);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stream_parsing() {
        let (finished, content, reasoning) = parsed(&[
            r#"data: {"choices":[{"delta":{"role":"assistant","reasoning_content":"Think about the façade"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":"","reasoning":", then the piñata"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"Hello"}}],"usage":null,"error":null}"#,
            r#"data: {"choices":[{"delta":{"content":", world 🍕"}}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}}"#,
            "data: [DONE]\r",
        ]);

        assert_eq!(content, "Hello, world 🍕");
        assert_eq!(reasoning, "Think about the façade, then the piñata");

        let finished = finished.expect("parse");
        let usage = finished.usage.expect("usage");
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 2));
        assert_eq!(usage.cached_tokens, Some(8));
    }

    #[test]
    fn tool_calls_collected() {
        let (finished, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"content":"Let me work it out."}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"eval","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"code\": \"print('caf"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"é')\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"ls","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(finished.expect("parse").finish, Finish::Stop);
        assert_eq!(content, "Let me work it out.print('café')[end][ls {}]");

        let (_, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"eval","arguments":"{\"code\": \"a = 'naïve'\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"eval","arguments":"{\"code\": \"print("}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"a)\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(content, "a = 'naïve'[end]print(a)[end]");

        let (_, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"{\"code\": \"print('jalapeño')\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"eval","arguments":"{\"code\": \"print('piñata')\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(content, "print('jalapeño')[end]print('piñata')[end]");

        let (_, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"{\"code\": \"pri"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"nt('crème')\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(content, "print('crème')[end]");

        let (_, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"{\"code\": \"pri"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"arguments":"nt('brûlée')\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(content, "print('brûlée')[end]");

        let (finished, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"eval","arguments":"{\"code\": \"pri"}}]},"finish_reason":"length"}]}"#,
        ]);
        assert_eq!(finished.expect("parse").finish, Finish::Length);
        assert_eq!(content, "pri");

        let (finished, content, _) = parsed(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"eval","arguments":"{\"code\": \"print('café"}}]},"finish_reason":"content_filter"}]}"#,
        ]);
        let filtered = Finish::Other("content_filter".to_owned());
        assert_eq!(finished.expect("parse").finish, filtered);
        assert_eq!(content, "print('café");
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
    fn loose_arguments() {
        let called = |arguments: &str| {
            let call =
                json!({ "index": 0, "function": { "name": "eval", "arguments": arguments } });
            let delta = json!({ "tool_calls": [call] });
            let chunk = json!({ "choices": [{ "delta": delta, "finish_reason": "tool_calls" }] });
            let line = format!("data: {chunk}");
            let (_, content, _) = parsed(&[&line, "data: [DONE]"]);
            content
        };

        let sloppy = "{\"code\": \"import re\\nprint(re.findall(r'\\d+', 'café1\tpiñata22'))\"}";
        let code = "import re\nprint(re.findall(r'\\d+', 'café1\tpiñata22'))[end]";
        assert_eq!(called(sloppy), code);

        let renamed =
            "{ \"content\" : \"import re\\nprint(re.findall(r'\\d+', 'café1\tpiñata22'))\" }";
        assert_eq!(called(renamed), code);
        let several = r#"{"command": "ls résumés", "cwd": "."}"#;
        assert_eq!(called(several), format!("[eval {several}]"));
        let number = r#"{"code": 1}"#;
        assert_eq!(called(number), format!("[eval {number}]"));
    }

    #[test]
    fn stream_endings() {
        let finish = |lines: &[&str]| parsed(lines).0.map(|finished| finished.finish);
        let partial = r#"data: {"choices":[{"delta":{"content":"Half a sentence"}}]}"#;

        let done = finish(&[partial, "data: [DONE]"]).expect("parse");
        assert_eq!(done, Finish::Stop);

        let cut = finish(&[partial]).expect_err("cut");
        assert_eq!(cut.message, "stream: ended early");
        assert!(cut.transient);
    }

    #[test]
    fn stream_errors() {
        let failed = |line: &str| parsed(&[line]).0.expect_err("error");

        let upstream = failed(r#"data: {"error":{"message":"Upstream\n  overloaded","code":502}}"#);
        assert_eq!(upstream.message, "stream: Upstream overloaded");
        assert!(upstream.transient);

        let bare = failed(r#"data: {"error":"overloaded"}"#);
        assert_eq!(bare.message, "stream: overloaded");
        assert!(bare.transient);

        let refused = failed(r#"data: {"error":{"message":"bad","code":400}}"#);
        assert!(!refused.transient);

        let cut = failed(r#"data: {"choices":[{"delta":{"content":"Hal"#);
        assert_eq!(cut.message, "stream: ended mid-event");
        assert!(cut.transient);

        let garbled = failed("data: {proxy notice: try again later}");
        assert_eq!(
            garbled.message,
            "stream: not JSON: {proxy notice: try again later}"
        );
        assert!(garbled.transient);
    }

    #[test]
    fn empty_reasoning_kept() {
        let lines = [
            r#"data: {"choices":[{"delta":{"reasoning_content":"","reasoning":"","content":"Sure"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":"","reasoning":"Hmm"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":null,"content":" thing"}}]}"#,
        ];
        let lines = lines.join("\n") + "\n";
        let mut thoughts = Vec::new();
        let mut on_delta = |delta: StreamDelta| {
            if let StreamDelta::Reasoning(text) = delta {
                thoughts.push(text.to_owned());
            }
        };

        let mut stream = Stream::default();
        stream.feed(lines.as_bytes(), &mut on_delta).expect("feed");
        assert_eq!(thoughts, ["", "Hmm"]);
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
        let specs = listed.expect("listed");
        let found: Vec<(&str, Option<u64>)> = specs
            .iter()
            .map(|spec| (spec.id.as_str(), spec.context))
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
        let model = OpenAIModel::new(&spec, Some(api), None).expect("model");
        let conversation = hello();

        let mut ignore = |_: StreamDelta| {};
        let generating = model.generate(&conversation, Purpose::Step, &mut ignore);
        let waited = block_on(generating).expect_err("idle");
        assert_eq!(waited.message, "timed out after 1s without data");
        assert!(waited.transient);
    }
}
