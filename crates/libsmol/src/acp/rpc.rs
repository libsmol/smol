use crate::acp::NOTICE;
use crate::utils::Waiting;
use crate::{Error, Result};
use agent_client_protocol_schema::v1::{
    Error as RpcError, ErrorCode, SessionNotification, SessionUpdate, CLIENT_METHOD_NAMES,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// Writes only whole lines
pub(crate) struct Peer {
    out: Out,
    pending: Waiting,
    /// The client declared `_smol/queued`
    queued: AtomicBool,
}

impl Peer {
    pub(crate) fn new(out: Out) -> Arc<Self> {
        Arc::new(Self {
            out,
            pending: Waiting::default(),
            queued: AtomicBool::new(false),
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

    pub(crate) fn notify(&self, method: &str, params: impl Serialize) {
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
    pub(crate) async fn call(&self, method: &str, params: impl Serialize) -> Option<Value> {
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

    pub(crate) fn set_queued(&self, queued: bool) {
        self.queued.store(queued, Ordering::Relaxed);
    }

    pub(crate) fn queued(&self) -> bool {
        self.queued.load(Ordering::Relaxed)
    }

    pub(crate) fn update(&self, session: &str, update: SessionUpdate) {
        let notification = SessionNotification::new(session.to_owned(), update);
        self.notify(CLIENT_METHOD_NAMES.session_update, notification);
    }

    pub(crate) fn notice(&self, session: &str, text: &str, kind: Notice) {
        let (retry_in, step, unsaved) = match kind {
            Notice::Step(retry_in) => (retry_in, true, false),
            Notice::Session => (None, false, false),
            Notice::Unsaved => (None, false, true),
        };
        let retry_in = retry_in.map(|wait| wait.as_secs_f64());
        let params = json!({
            "sessionId": session,
            "text": text,
            "retryIn": retry_in,
            "step": step,
            "unsaved": unsaved,
        });
        self.notify(NOTICE, params);
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
            Error::SessionNotFound(_) => ErrorCode::ResourceNotFound,
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
