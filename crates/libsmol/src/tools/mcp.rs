use crate::config::{McpConfig, McpKind};
use crate::prompt;
use crate::spawn::Process;
use crate::tools::attach::returned;
use crate::utils::{excerpt, http_cause, http_client};
use crate::worker::host::{Called, Calling};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reaper::{cause, terminal_view};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Client;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, CancelledNotificationParam,
    ClientNotification, ClientRequest, ContentBlock, ImageContent, PaginatedRequestParams,
    RequestId, ResourceContents, ServerResult,
};
use rmcp::service::{
    serve_client, ClientInitializeError, Peer, PeerRequestOptions, RunningService,
    RunningServiceCancellationToken, ServiceError,
};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransportConfig, StreamableHttpClientWorker, StreamableHttpError,
};
use rmcp::transport::WorkerTransport;
use rmcp::RoleClient;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::error;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::{self, JoinHandle};
use tokio::time;

const HANDSHAKE_WAIT: Duration = match cfg!(test) {
    true => Duration::from_secs(5),
    false => Duration::from_secs(30),
};
const MAX_STDERR_LINES: usize = 5;
/// Progress redrawn with `\r` can run for megabytes without a newline
const MAX_STDERR_BYTES: usize = 16 * 1024;
/// The server may still be writing stderr when the connection drops
const STDERR_WAIT: Duration = Duration::from_secs(1);
const CLOSED_EARLY: &str = "connection closed during the handshake";

/// The third argument is the `server.function` from the code and labels images but not errors
type CallFn = dyn Fn(String, Map<String, Value>, String) -> Calling + Send + Sync;

#[derive(Clone)]
pub(crate) struct Server {
    pub(crate) name: String,
    pub(crate) description: String,
    /// Passed to Python as is to build signatures and docs
    pub(crate) tools: Vec<Value>,
    pub(crate) call: Arc<CallFn>,
}

/// A connected server that disconnects when dropped
pub(crate) struct Link {
    service: RunningService<RoleClient, ()>,
    /// Reads stderr until the server exits and returns the last few lines
    said: Option<JoinHandle<String>>,
}

impl Link {
    /// Returns when the connection ended and the last lines the server wrote to stderr
    pub(crate) async fn ended(self) -> (Instant, String) {
        self.service.waiting().await.ok();
        let at = Instant::now();
        let said = last_lines(self.said).await;
        (at, said)
    }
}

/// The server's process group is dropped together with the connection
struct Connection {
    peer: Peer<RoleClient>,
    token: Option<RunningServiceCancellationToken>,
    _process: Option<Process>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            token.cancel();
        }
    }
}

/// An interrupt drops the call's future, and without a cancel the server still finishes the call
struct Unanswered {
    peer: Peer<RoleClient>,
    id: Option<RequestId>,
}

impl Drop for Unanswered {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };

        let params = CancelledNotificationParam::new(Some(id), None);
        let cancelled = CancelledNotification::new(params);
        let cancelled = ClientNotification::CancelledNotification(cancelled);
        let peer = self.peer.clone();
        let sending = async move {
            peer.send_notification(cancelled).await.ok();
        };
        task::spawn(sending);
    }
}

/// Images and videos in call results are copied into `copies`
pub(crate) async fn connect(
    server: &McpConfig,
    cwd: &Path,
    copies: PathBuf,
) -> Result<(Server, Link), String> {
    let mut process = None;
    let mut said = None;
    let handshake = async {
        let service = match &server.kind {
            McpKind::Stdio {
                command: program,
                args,
                env,
            } => {
                let path = reaper::lookup(program);
                let mut command = Command::new(path);
                command
                    .args(args)
                    .envs(env)
                    .current_dir(cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let std = command.as_std_mut();
                let mark = reaper::prepare(std);

                let unstarted = |e: io::Error| {
                    let shown = program.display();
                    let reason = cause(&e);
                    format!("start: {shown}: {reason}")
                };
                let spawned = command.spawn();
                let mut child = spawned.map_err(unstarted)?;
                let stdout = child.stdout.take().expect("piped stdout");
                let stdin = child.stdin.take().expect("piped stdin");
                let mut stderr = child.stderr.take().expect("piped stderr");

                // Read until it exits because the server blocks once the pipe fills up
                let reading = async move {
                    let mut chunk = [0; 4096];
                    let mut tail = Vec::new();
                    let mut cut = false;
                    while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
                        tail.extend_from_slice(&chunk[..read]);
                        let over = tail.len().saturating_sub(MAX_STDERR_BYTES);
                        tail.drain(..over);
                        cut |= over > 0;
                    }

                    // A cut tail may start inside a character or an escape sequence, up to its first line break
                    let broken = tail.iter().position(|byte| matches!(byte, b'\n' | b'\r'));
                    if let Some(at) = broken.filter(|_| cut) {
                        tail.drain(..at);
                    }
                    said_lines(&tail)
                };
                said = Some(task::spawn(reading));

                let started = Process::new(child, mark);
                let started = started.map_err(unstarted)?;
                process = Some(started);
                serve_client((), (stdout, stdin)).await
            }
            McpKind::Http { url, headers } => {
                let builder = Client::builder();
                let client = http_client(builder)?;
                let config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                let mut worker = StreamableHttpClientWorker::new(client, config);
                for (name, value) in headers {
                    // Values may hold secrets and are never printed
                    let invalid = |part| format!("header: {name}: invalid {part}");
                    let header = name.parse::<HeaderName>();
                    let header = header.map_err(|_| invalid("name"))?;
                    let value = value.parse::<HeaderValue>();
                    let value = value.map_err(|_| invalid("value"))?;
                    worker.config.custom_headers.insert(header, value);
                }

                let transport = WorkerTransport::spawn(worker);
                serve_client((), transport).await
            }
        };

        let service = match service {
            Ok(service) => service,
            Err(e) => {
                let reason = root_cause(&e, CLOSED_EARLY);
                return Err(format!("connect: {reason}"));
            }
        };

        // Some servers send an empty or repeated cursor with the last page
        let mut listed = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = None;
        loop {
            let params = PaginatedRequestParams::default().with_cursor(cursor);
            let page = match service.list_tools(Some(params)).await {
                Ok(page) => page,
                Err(e) => {
                    let reason = root_cause(&e, prompt::NO_SERVER);
                    return Err(format!("list tools: {reason}"));
                }
            };
            listed.extend(page.tools);

            cursor = page.next_cursor.filter(|next| !next.is_empty());
            let Some(next) = &cursor else {
                break;
            };
            if !seen.insert(next.clone()) {
                break;
            }
        }

        Ok((service, listed))
    };

    let timed = time::timeout(HANDSHAKE_WAIT, handshake).await;
    let connected = match timed {
        Ok(connected) => connected,
        Err(_) => {
            let seconds = HANDSHAKE_WAIT.as_secs();
            Err(format!("connect: timed out after {seconds}s"))
        }
    };
    let (service, listed) = match connected {
        Ok(connected) => connected,
        Err(reason) => {
            // The reason is usually in its stderr, which only ends once a hung server is killed
            drop(process.take());
            let reading = said.take();
            let lines = last_lines(reading).await;
            let message = match lines.is_empty() {
                true => reason,
                false => format!("{reason}\n{lines}"),
            };
            return Err(message);
        }
    };

    let description = server.description.clone().unwrap_or_default();

    let mut tools = Vec::new();
    for tool in &listed {
        let tool = serde_json::to_value(tool).expect("serialize");
        tools.push(tool);
    }

    let peer = service.peer().clone();
    let token = service.cancellation_token();
    let connection = Arc::new(Connection {
        peer,
        token: Some(token),
        _process: process,
    });
    let call = move |function: String, arguments: Map<String, Value>, label: String| -> Calling {
        let connection = Arc::clone(&connection);
        let copies = copies.clone();
        Box::pin(async move {
            let mut media = Vec::new();
            let calling = async {
                let params = CallToolRequestParams::new(function).with_arguments(arguments);
                let request = CallToolRequest::new(params);
                let request = ClientRequest::CallToolRequest(request);
                let options = PeerRequestOptions::no_options();
                let sent = connection.peer.send_cancellable_request(request, options);
                let handle = sent.await.map_err(|e| root_cause(&e, prompt::NO_SERVER))?;

                let mut unanswered = Unanswered {
                    peer: handle.peer.clone(),
                    id: Some(handle.id.clone()),
                };
                let replied = handle.await_response().await;
                unanswered.id = None;

                let replied = replied.map_err(|e| root_cause(&e, prompt::NO_SERVER))?;
                let result = match replied {
                    ServerResult::CallToolResult(result) => result,
                    _ => {
                        let unexpected = ServiceError::UnexpectedResponse;
                        return Err(root_cause(&unexpected, prompt::NO_SERVER));
                    }
                };

                let bytes = |base64: &str| base64.len() * 3 / 4;
                let mut texts = Vec::new();
                for block in result.content {
                    let text = match block {
                        ContentBlock::Text(text) => text.text,
                        ContentBlock::Image(ImageContent {
                            data, mime_type, ..
                        }) => {
                            let size = bytes(&data);
                            let copies = copies.clone();
                            let copying = move || {
                                let decoded = STANDARD.decode(&data).ok()?;
                                returned(&copies, &decoded)
                            };
                            let copied = task::spawn_blocking(copying).await.expect("copy");
                            match copied {
                                // The `<image>` tag in the observation already reports it
                                Some(Ok(copy)) => {
                                    media.push((label.clone(), copy));
                                    continue;
                                }
                                Some(Err(reason)) => prompt::unkept(&label, &reason),
                                None => prompt::unshown("image", &label, &mime_type, size),
                            }
                        }
                        ContentBlock::Audio(audio) => {
                            let size = bytes(&audio.data);
                            prompt::unshown("audio", &label, &audio.mime_type, size)
                        }
                        ContentBlock::Resource(embedded) => match embedded.resource {
                            ResourceContents::TextResourceContents { text, .. } => text,
                            ResourceContents::BlobResourceContents {
                                uri,
                                mime_type,
                                blob,
                                ..
                            } => {
                                let mime = mime_type.as_deref().unwrap_or("binary");
                                let size = bytes(&blob);
                                let resource = format!("resource {uri}");
                                prompt::unshown(&resource, &label, mime, size)
                            }
                            _ => continue,
                        },
                        ContentBlock::ResourceLink(link) => prompt::linked(&link.uri),
                        _ => continue,
                    };
                    texts.push(text);
                }

                if result.is_error.unwrap_or(false) {
                    let message = match texts.is_empty() {
                        true => prompt::SILENT_ERROR.to_owned(),
                        false => texts.join("; "),
                    };
                    return Err(message);
                }

                let text = match (texts.is_empty(), &result.structured_content) {
                    (true, Some(content)) => content.to_string(),
                    _ => texts.join("\n"),
                };
                let mut reply = Map::new();
                reply.insert("text".to_owned(), Value::String(text));
                if let Some(content) = result.structured_content {
                    reply.insert("structured".to_owned(), content);
                }
                Ok(Value::Object(reply))
            };

            let result = calling.await;
            Called { result, media }
        })
    };

    let server = Server {
        name: server.name.clone(),
        description,
        tools,
        call: Arc::new(call),
    };
    let link = Link { service, said };
    Ok((server, link))
}

async fn last_lines(said: Option<JoinHandle<String>>) -> String {
    let Some(reading) = said else {
        return String::new();
    };
    let waited = time::timeout(STDERR_WAIT, reading).await;
    match waited {
        Ok(Ok(lines)) => lines,
        _ => String::new(),
    }
}

/// The last lines of `tail` as a terminal shows them, each cut short
fn said_lines(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let text = terminal_view(&text);

    let mut lines = Vec::new();
    for line in text.lines() {
        let line = excerpt(line);
        if !line.is_empty() {
            lines.push(line);
        }
    }

    let start = lines.len().saturating_sub(MAX_STDERR_LINES);
    lines[start..].join("\n")
}

/// Follows rmcp errors by type where they lack `source` and returns `closed` when the peer hung up
fn root_cause(top: &(dyn error::Error + 'static), closed: &str) -> String {
    let mut current = top;
    loop {
        if let Some(http) = current.downcast_ref::<reqwest::Error>() {
            return http_cause(http);
        }

        // The peer closed the connection, seen as end of stream or a broken pipe on write
        let starting = current.downcast_ref::<ClientInitializeError>();
        let serving = current.downcast_ref::<ServiceError>();
        let piped = current.downcast_ref::<io::Error>().map(io::Error::kind);
        let ended = matches!(starting, Some(ClientInitializeError::ConnectionClosed(_)))
            || matches!(serving, Some(ServiceError::TransportClosed))
            || matches!(
                piped,
                Some(io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected)
            );
        if ended {
            return closed.to_owned();
        }

        let mut inner = current.source();
        if let Some(ClientInitializeError::TransportError { error, .. }) = starting {
            inner = Some(error);
        }
        if let Some(ServiceError::TransportSend(error)) = serving {
            inner = Some(error);
        }
        let posting = current.downcast_ref::<StreamableHttpError<reqwest::Error>>();
        if let Some(StreamableHttpError::Client(error)) = posting {
            inner = Some(error);
        }

        match inner {
            Some(next) => current = next,
            None => return cause(current),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::config::{McpConfig, McpKind};
    use crate::tests::{interpreter, temp_dir};
    use crate::tools::mcp::{connect, said_lines, Link, Server};
    use crate::utils::block_on;
    use serde_json::{json, Map};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::{task, time};

    pub(crate) const SCRIPT: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/mcp_test_server.py");
    const CWD: &str = env!("CARGO_MANIFEST_DIR");

    /// Tests that never call `mixed` never create `copies`
    pub(crate) fn fake() -> Server {
        let copies = temp_dir("copies");
        let (server, link) = started(copies);

        // Dropping the link would disconnect
        let kept = async {
            let ending = link.ended();
            task::spawn(ending);
        };
        block_on(kept);

        server
    }

    pub(crate) fn started(copies: PathBuf) -> (Server, Link) {
        let args = vec![SCRIPT.to_owned()];
        let server = stdio("fake", args);
        let cwd = Path::new(CWD);

        let connecting = connect(&server, cwd, copies);
        block_on(connecting).expect("connect")
    }

    fn stdio(name: &str, args: Vec<String>) -> McpConfig {
        let kind = McpKind::Stdio {
            command: interpreter(),
            args,
            env: BTreeMap::new(),
        };
        McpConfig {
            name: name.to_owned(),
            description: None,
            enabled: true,
            kind,
        }
    }

    fn connect_error(server: &McpConfig) -> String {
        let cwd = Path::new(CWD);
        let copies = temp_dir("copies");

        let connecting = connect(server, cwd, copies);
        let Err(failed) = block_on(connecting) else {
            panic!("connected");
        };
        failed
    }

    #[test]
    fn lists_tools() {
        let server = fake();
        let names: Vec<&str> = server
            .tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        let expected = [
            "add",
            "fail",
            "wait",
            "echo-text",
            "mixed",
            "exit",
            "cancelled",
            "stats",
            "counts",
            "bare",
        ];
        assert_eq!(names, expected);
    }

    #[test]
    fn structured_sent() {
        let server = fake();
        let called = |tool: &str| {
            let label = format!("fake.{tool}");
            let calling = (server.call)(tool.to_owned(), Map::new(), label);
            block_on(calling).result
        };

        let stats = json!({"text": "3 cafés counted", "structured": {"count": 3}});
        assert_eq!(called("stats"), Ok(stats));
        let bare = json!({"text": "{\"count\":3}", "structured": {"count": 3}});
        assert_eq!(called("bare"), Ok(bare));
    }

    #[test]
    fn cancel_sent() {
        let server = fake();

        // An answered call sends no cancel
        let label = "fake.cancelled".to_owned();
        let counting = (server.call)("cancelled".to_owned(), Map::new(), label);
        let counted = block_on(counting).result.expect("call");
        assert_eq!(counted, json!({"text": "0"}));

        let mut arguments = Map::new();
        arguments.insert("seconds".to_owned(), json!(1));
        let label = "fake.wait".to_owned();
        let waiting = (server.call)("wait".to_owned(), arguments, label);
        // Dropping it on timeout does what an interrupt does
        let limit = Duration::from_millis(200);
        let dropped = async move { time::timeout(limit, waiting).await.is_err() };
        assert!(block_on(dropped));

        // Let the task sending the cancel run first
        let settling = async { time::sleep(Duration::from_millis(100)).await };
        block_on(settling);

        let label = "fake.cancelled".to_owned();
        let counting = (server.call)("cancelled".to_owned(), Map::new(), label);
        let counted = block_on(counting).result.expect("call");
        assert_eq!(counted, json!({"text": "1"}));
    }

    #[test]
    fn stderr_shown() {
        let script = "import sys\nsys.stderr.buffer.write('missing environment variable API_TOKEN\\n'.encode())\nsys.exit(1)";
        let args = vec!["-c".to_owned(), script.to_owned()];
        let exited = stdio("broken", args);
        let expected =
            "connect: connection closed during the handshake\nmissing environment variable API_TOKEN";
        assert_eq!(connect_error(&exited), expected);

        // A server that never replies still gets its stderr shown
        let script = "import sys, time\nsys.stderr.buffer.write('waiting for authorization in the browser…\\n'.encode())\nsys.stderr.flush()\ntime.sleep(60)";
        let args = vec!["-c".to_owned(), script.to_owned()];
        let silent = stdio("broken", args);
        let expected = "connect: timed out after 5s\nwaiting for authorization in the browser…";
        assert_eq!(connect_error(&silent), expected);

        // Progress redrawn on one line for longer than the kept tail
        let script = "import sys\n\
            progress = '\\r'.join(f'{i}% café' for i in range(50000))\n\
            sys.stderr.buffer.write(progress.encode())\n\
            sys.stderr.buffer.write('\\n\\x1b[31mmissing API_TOKEN\\x1b[0m\\n'.encode())\n\
            sys.exit(1)";
        let args = vec!["-c".to_owned(), script.to_owned()];
        let drawn = stdio("broken", args);
        let expected =
            "connect: connection closed during the handshake\n49999% café\nmissing API_TOKEN";
        assert_eq!(connect_error(&drawn), expected);

        // A line the kept tail starts inside, here in the middle of a character
        let script = "import sys\n\
            sys.stderr.buffer.write(('é' * 10000 + '\\nmissing API_TOKEN\\n').encode())\n\
            sys.exit(1)";
        let args = vec!["-c".to_owned(), script.to_owned()];
        let long = stdio("broken", args);
        let expected = "connect: connection closed during the handshake\nmissing API_TOKEN";
        assert_eq!(connect_error(&long), expected);
    }

    #[test]
    fn stderr_cleaned() {
        let long = "é".repeat(400);
        let said = format!(
            "zeroth\nfirst\n\x1b[33mwarning\x1b[0m: café\r\n10%\r55%\r100%\n\n{long}\nlast naïve line"
        );
        let cut = "é".repeat(300);
        let expected = format!("first\nwarning: café\n100%\n{cut}…\nlast naïve line");
        assert_eq!(said_lines(said.as_bytes()), expected);
    }

    #[test]
    fn image_kept() {
        let copies = temp_dir("mcp-image");
        let (server, _link) = started(copies.clone());
        let label = "fake.mixed".to_owned();
        let calling = (server.call)("mixed".to_owned(), Map::new(), label);
        let called = block_on(calling);

        let [(label, _)] = &called.media[..] else {
            panic!("{:?}", called.media);
        };
        assert_eq!(label, "fake.mixed");

        let expected = "Intro\nembedded content\n[resource link: file:///big.bin]";
        let result = called.result.expect("call");
        assert_eq!(result, json!({"text": expected}));

        let entries = fs::read_dir(&copies).expect("copies");
        let names: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        let [copy] = &names[..] else {
            panic!("{names:?}");
        };
        assert_eq!(copy.extension(), Some("png".as_ref()));
        assert_eq!(fs::read(copy).expect("copy"), b"\x89PNG\r\n\x1a\nhello");
        fs::remove_dir_all(&copies).ok();
    }

    /// A server that exits after connecting still yields its last stderr lines
    #[test]
    fn exit_noticed() {
        let code = "import atexit, runpy, sys\n\
            def said():\n    sys.stderr.buffer.write('written at exit\\n'.encode())\n    sys.stderr.buffer.flush()\n\
            atexit.register(said)\n\
            runpy.run_path(sys.argv[1], run_name='__main__')";
        let args = vec!["-c".to_owned(), code.to_owned(), SCRIPT.to_owned()];
        let server = stdio("fake", args);
        let cwd = Path::new(CWD);
        let copies = temp_dir("copies");

        let running = async {
            let (server, link) = connect(&server, cwd, copies).await.expect("connect");
            let label = "fake.exit".to_owned();
            let calling = (server.call)("exit".to_owned(), Map::new(), label);
            let failed = calling.await.result.expect_err("exited");
            let ending = link.ended();
            let ended = time::timeout(Duration::from_secs(5), ending).await;
            let said = ended.map(|(_, said)| said);
            (failed, said)
        };
        let (failed, said) = block_on(running);

        assert_eq!(failed, "server disconnected");
        assert_eq!(said.ok().as_deref(), Some("written at exit"));
    }

    #[test]
    fn connect_failure() {
        let kind = McpKind::Http {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            headers: BTreeMap::new(),
        };
        let server = McpConfig {
            name: "down".to_owned(),
            description: None,
            enabled: true,
            kind,
        };

        // Windows words the reason in the system language
        let failed = connect_error(&server);
        let expected = "connect: cannot connect to 127.0.0.1:1: ";
        assert!(failed.starts_with(expected), "{failed}");
        #[cfg(unix)]
        assert!(failed.ends_with(": connection refused"), "{failed}");

        let kind = McpKind::Stdio {
            command: PathBuf::from("/no/such/program"),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        let missing = McpConfig { kind, ..server };
        let failed = connect_error(&missing);
        assert!(failed.starts_with("start: /no/such/program: "), "{failed}");
    }
}
