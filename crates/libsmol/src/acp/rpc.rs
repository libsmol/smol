use crate::utils::Waiting;
use crate::{Error, Result};
use agent_client_protocol_schema::v1::{
    Error as RpcError, ErrorCode, SessionNotification, SessionUpdate, CLIENT_METHOD_NAMES,
};
use reaper::wire::{
    self, Addressed, McpStatus, Program, BACKGROUND, IMAGES, MCP, NOTICE, PROGRAMS, QUEUED,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
#[cfg(test)]
use tokio::sync::mpsc::UnboundedSender;

pub(crate) type Lines = UnboundedReceiver<io::Result<String>>;

pub(crate) enum Out {
    Stdout,
    #[cfg(test)]
    Lines(UnboundedSender<io::Result<String>>),
}

#[derive(Clone, Copy)]
pub(crate) enum Notice {
    /// Part of the running step even before it shows any text, with the wait before a retry if any
    Step(Option<Duration>),
    Session,
    Unsaved,
}

/// The `_smol/` capabilities the client declared
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Declared {
    pub(crate) queued: bool,
    pub(crate) images: bool,
    pub(crate) background: bool,
}

/// Writes only whole lines
pub(crate) struct Peer {
    out: Out,
    pending: Waiting,
    declared: Mutex<Declared>,
}

impl Peer {
    pub(crate) fn new(out: Out) -> Arc<Self> {
        Arc::new(Self {
            out,
            pending: Waiting::default(),
            declared: Mutex::default(),
        })
    }

    fn write(&self, line: String) {
        match &self.out {
            Out::Stdout => {
                let mut out = io::stdout().lock();
                if writeln!(out, "{line}").is_ok() {
                    out.flush().ok();
                }
            }
            #[cfg(test)]
            Out::Lines(lines) => {
                lines.send(Ok(line)).ok();
            }
        }
    }

    fn notify(&self, method: &str, params: impl Serialize) {
        let line = encoded(None, method, params);
        self.write(line);
    }

    pub(crate) fn respond(&self, id: &Value, result: Result<Value>) {
        let message = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => {
                let error = RpcError::from(error);
                json!({ "jsonrpc": "2.0", "id": id, "error": error })
            }
        };
        let line = serde_json::to_string(&message).expect("serialize");
        self.write(line);
    }

    /// None when the client replies with an error
    async fn call(&self, method: &str, params: impl Serialize) -> Option<Value> {
        let (id, reply) = self.pending.ask();
        let line = encoded(Some(id), method, params);
        self.write(line);
        reply.await.ok()?.ok()
    }

    pub(crate) fn answered(&self, message: &Value) {
        let Some(id) = message["id"].as_u64() else {
            return;
        };
        let reply = match message.get("error") {
            Some(error) => Err(error.to_string()),
            None => Ok(message["result"].clone()),
        };
        self.pending.answer(id, reply);
    }

    /// Takes what the `_meta` of the client's capabilities declares
    pub(crate) fn declare(&self, meta: &Value) {
        let on = |name: &str| meta[name].as_bool() == Some(true);
        let declared = Declared {
            queued: on(QUEUED),
            images: on(IMAGES),
            background: on(BACKGROUND),
        };
        *self.declared.lock().expect("declared") = declared;
    }

    pub(crate) fn declared(&self) -> Declared {
        *self.declared.lock().expect("declared")
    }
}

/// Sends what concerns one session, with its id
#[derive(Clone)]
pub(crate) struct SessionPeer {
    peer: Arc<Peer>,
    id: Arc<str>,
}

impl SessionPeer {
    pub(crate) fn new(peer: Arc<Peer>, id: &str) -> Self {
        let id = Arc::from(id);
        Self { peer, id }
    }

    pub(crate) fn update(&self, update: SessionUpdate) {
        let id = Arc::clone(&self.id);
        let notification = SessionNotification::new(id, update);
        let method = CLIENT_METHOD_NAMES.session_update;
        self.peer.notify(method, notification);
    }

    pub(crate) fn notice(&self, text: &str, kind: Notice) {
        let (retry_in, step, unsaved) = match kind {
            Notice::Step(retry_in) => (retry_in, true, false),
            Notice::Session => (None, false, false),
            Notice::Unsaved => (None, false, true),
        };
        let notice = wire::Notice {
            text: text.to_owned(),
            retry_in: retry_in.map(|wait| wait.as_secs_f64()),
            step,
            unsaved,
        };
        self.send(NOTICE, notice);
    }

    pub(crate) fn servers(&self, servers: Vec<McpStatus>) {
        self.send(MCP, wire::Servers { servers });
    }

    pub(crate) fn programs(&self, running: &[Program]) {
        let programs = running.to_vec();
        self.send(PROGRAMS, wire::Programs { programs });
    }

    fn send(&self, method: &str, body: impl Serialize) {
        let session_id = self.id.to_string();
        let params = Addressed { session_id, body };
        self.peer.notify(method, params);
    }

    pub(crate) fn declared(&self) -> Declared {
        self.peer.declared()
    }

    /// Tells a client that declared `BACKGROUND` that programs the code left running ended
    /// while no turn ran, for it to hand them the next turn
    pub(crate) fn background_ended(&self) {
        if self.declared().background {
            self.send(BACKGROUND, ());
        }
    }

    /// None unless the client declared `QUEUED` and answered
    pub(crate) async fn queued(&self) -> Option<Value> {
        if !self.declared().queued {
            return None;
        }

        let session_id = self.id.to_string();
        let params = Addressed {
            session_id,
            body: (),
        };
        self.peer.call(QUEUED, params).await
    }
}

#[derive(Serialize)]
struct Message<'a, T> {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: &'a str,
    params: T,
}

/// A request or notification serialized without going through `Value`
fn encoded(id: Option<u64>, method: &str, params: impl Serialize) -> String {
    let message = Message {
        jsonrpc: "2.0",
        id,
        method,
        params,
    };
    serde_json::to_string(&message).expect("serialize")
}

impl From<Error> for RpcError {
    fn from(error: Error) -> Self {
        let code = match error {
            Error::Parse(_) => ErrorCode::ParseError,
            Error::InvalidRequest => ErrorCode::InvalidRequest,
            Error::MethodNotFound(_) => ErrorCode::MethodNotFound,
            Error::InvalidParams(_) | Error::Refused(_) => ErrorCode::InvalidParams,
            Error::SessionNotFound(_) | Error::MemoryNotFound(_) => ErrorCode::ResourceNotFound,
            Error::Unconfigured(_) => ErrorCode::AuthRequired,
            _ => ErrorCode::InternalError,
        };
        let code = i32::from(code);
        let message = error.to_string();

        RpcError::new(code, message)
    }
}

#[cfg(test)]
mod tests {
    use crate::acp::to_value;
    use crate::Error;
    use agent_client_protocol_schema::v1::Error as RpcError;
    use serde_json::json;

    #[test]
    fn error_codes() {
        let cases = [
            (Error::Parse("eof".to_owned()), -32700),
            (Error::InvalidRequest, -32600),
            (Error::MethodNotFound("x/y".to_owned()), -32601),
            (Error::InvalidParams("bad".to_owned()), -32602),
            (Error::Refused("too long".to_owned()), -32602),
            (Error::SessionNotFound("s1".to_owned()), -32002),
            (Error::MemoryNotFound(12), -32002),
            (Error::Unconfigured("no model.toml".to_owned()), -32000),
            (Error::History("disk full".to_owned()), -32603),
        ];

        for (error, code) in cases {
            let message = error.to_string();
            let error = RpcError::from(error);
            let error = to_value(error);
            assert_eq!(error, json!({ "code": code, "message": message }));
        }
    }
}
