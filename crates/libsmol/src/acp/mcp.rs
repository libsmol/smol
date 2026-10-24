use crate::acp::rpc::Peer;
use crate::acp::MCP;
use crate::code::{Added, Worker};
use crate::config::McpConfig;
use crate::history::Settings;
use crate::prompt;
use crate::tools::mcp::{connect, Server};
use serde_json::json;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task;
use tracing::{info, warn};

/// A server that drops this soon after connecting stays down instead of restarting in a loop
const MIN_UPTIME: Duration = Duration::from_secs(60);

/// The only record of servers installed in the worker, listed in the prompt in config order
/// because connect order would break the prompt cache
pub(crate) struct McpPool {
    servers: Vec<Mcp>,
    /// Bumped on every connect, and only the latest attempt's result counts
    attempts: u64,
    events: UnboundedReceiver<McpEvent>,
    outbox: UnboundedSender<McpEvent>,
    cwd: PathBuf,
    copies: PathBuf,
    peer: Arc<Peer>,
    session: String,
}

struct Mcp {
    config: McpConfig,
    state: McpState,
}

enum McpState {
    Off,
    Connecting {
        attempt: u64,
        /// The disconnect error when reconnecting after a running server dropped
        dropped: Option<String>,
    },
    Connected {
        attempt: u64,
        since: Instant,
        added: Added,
    },
    Failed {
        error: String,
    },
}

pub(crate) enum McpEvent {
    Attempted {
        attempt: u64,
        result: Result<Server, String>,
    },
    /// `said` is the last thing the server wrote to stderr
    Ended {
        attempt: u64,
        at: Instant,
        said: String,
    },
}

impl McpPool {
    pub(crate) fn new(
        configs: Vec<McpConfig>,
        settings: &Settings,
        copies: &Path,
        peer: Arc<Peer>,
        session: String,
    ) -> Self {
        let (outbox, events) = mpsc::unbounded_channel();
        let mut pool = Self {
            servers: Vec::new(),
            attempts: 0,
            events,
            outbox,
            cwd: settings.cwd.clone(),
            copies: copies.to_owned(),
            peer,
            session,
        };

        for config in configs {
            let on = settings.mcp_on(&config);
            let state = McpState::Off;
            pool.servers.push(Mcp { config, state });
            if on {
                let index = pool.servers.len() - 1;
                pool.connect(index, None);
            }
        }
        pool
    }

    pub(crate) async fn recv(&mut self) -> Option<McpEvent> {
        self.events.recv().await
    }

    pub(crate) fn connecting(&self) -> bool {
        let connecting = |server: &Mcp| matches!(server.state, McpState::Connecting { .. });
        self.servers.iter().any(connecting)
    }

    pub(crate) fn installed(&self) -> Vec<&Added> {
        let mut installed = Vec::new();
        for server in &self.servers {
            if let McpState::Connected { added, .. } = &server.state {
                installed.push(added);
            }
        }
        installed
    }

    pub(crate) fn find(&self, name: &str) -> Option<usize> {
        self.servers
            .iter()
            .position(|server| server.config.name == name)
    }

    pub(crate) fn configs(&self) -> Vec<McpConfig> {
        self.servers
            .iter()
            .map(|server| server.config.clone())
            .collect()
    }

    /// Turning on a server that is already on reconnects it
    pub(crate) fn set(&mut self, index: usize, on: bool, worker: &Worker) -> &McpConfig {
        let off = McpState::Off;
        let state = mem::replace(&mut self.servers[index].state, off);
        if let McpState::Connected { added, .. } = state {
            worker.remove_server(&added.name);
        }

        let name = &self.servers[index].config.name;
        match on {
            true => self.connect(index, None),
            false => info!("mcp[{name}]: turned off"),
        }
        self.report();
        &self.servers[index].config
    }

    pub(crate) async fn handle(&mut self, event: McpEvent, worker: &mut Worker) {
        match event {
            McpEvent::Attempted { attempt, result } => {
                // A server toggled in the meantime is disconnected by dropping the result
                let current = |server: &Mcp| match server.state {
                    McpState::Connecting { attempt: now, .. } => now == attempt,
                    _ => false,
                };
                let Some(index) = self.servers.iter().position(current) else {
                    return;
                };
                let McpState::Connecting { dropped, .. } = &mut self.servers[index].state else {
                    return;
                };
                let dropped = dropped.take();

                let added = match result {
                    Ok(server) => {
                        let installed = self.installed();
                        let adding = worker.add_server(server, &installed);
                        adding.await.map_err(|error| error.to_string())
                    }
                    Err(error) => Err(error),
                };
                let name = &self.servers[index].config.name;
                let state = match added {
                    Ok(added) => {
                        let tools = added.server.tools.len();
                        info!("mcp[{name}]: connected [tools={tools}]");
                        let since = Instant::now();
                        McpState::Connected {
                            attempt,
                            since,
                            added,
                        }
                    }
                    Err(error) => {
                        warn!("mcp[{name}]: connect failed, {error}");
                        let error = match dropped {
                            Some(dropped) => format!("{dropped}\n{error}"),
                            None => error,
                        };
                        McpState::Failed { error }
                    }
                };
                self.servers[index].state = state;
            }

            McpEvent::Ended { attempt, at, said } => {
                let current = |server: &Mcp| match server.state {
                    McpState::Connected { attempt: now, .. } => now == attempt,
                    _ => false,
                };
                let Some(index) = self.servers.iter().position(current) else {
                    return;
                };
                let off = McpState::Off;
                let state = mem::replace(&mut self.servers[index].state, off);
                let McpState::Connected { since, added, .. } = state else {
                    return;
                };
                worker.remove_server(&added.name);

                let disconnected = prompt::NO_SERVER;
                let error = match said.is_empty() {
                    true => disconnected.to_owned(),
                    false => format!("{disconnected}\n{said}"),
                };
                let name = &self.servers[index].config.name;
                match at - since >= MIN_UPTIME {
                    true => {
                        warn!("mcp[{name}]: disconnected (reconnecting), {error}");
                        self.connect(index, Some(error));
                    }
                    false => {
                        warn!("mcp[{name}]: disconnected (not reconnecting), {error}");
                        self.servers[index].state = McpState::Failed { error };
                    }
                }
            }
        }

        self.report();
    }

    pub(crate) fn report(&self) {
        let mut listed = Vec::new();
        for server in &self.servers {
            let name = &server.config.name;
            let entry = match &server.state {
                McpState::Off => json!({ "name": name, "state": "off" }),
                McpState::Connecting { .. } => json!({ "name": name, "state": "connecting" }),
                McpState::Connected { added, .. } => {
                    let tools = added.server.tools.len();
                    json!({ "name": name, "state": "connected", "tools": tools })
                }
                McpState::Failed { error } => {
                    json!({ "name": name, "state": "failed", "error": error })
                }
            };
            listed.push(entry);
        }

        let status = json!({ "sessionId": self.session, "servers": listed });
        self.peer.notify(MCP, status);
    }

    fn connect(&mut self, index: usize, dropped: Option<String>) {
        self.attempts += 1;
        let attempt = self.attempts;
        let config = self.servers[index].config.clone();
        let cwd = self.cwd.clone();
        let copies = self.copies.clone();
        let outbox = self.outbox.clone();

        task::spawn(async move {
            let connected = connect(&config, &cwd, copies).await;
            let (result, link) = match connected {
                Ok((server, link)) => (Ok(server), Some(link)),
                Err(error) => (Err(error), None),
            };
            let event = McpEvent::Attempted { attempt, result };
            outbox.send(event).ok();

            let Some(link) = link else {
                return;
            };
            // Record the drop time here because events wait for a running turn to end
            let (at, said) = link.ended().await;
            let event = McpEvent::Ended { attempt, at, said };
            outbox.send(event).ok();
        });
        self.servers[index].state = McpState::Connecting { attempt, dropped };
    }
}
