use crate::client::{
    model_of, models, Asked, AuthRequired, Client, Goal, GoalStatus, Incoming, McpState, McpStatus,
    Notice, Pick,
};
use agent_client_protocol_schema::v1::{
    ContentBlock, ResourceLink, SessionConfigOption, SessionUpdate, StopReason,
};
use agent_client_protocol_schema::MaybeUndefined;
use anyhow::Result;
use chrono::{DateTime, Local};
use std::env;
use std::path::{is_separator, Path, PathBuf};
use std::sync::mpsc::Receiver;
use url::Url;

pub(crate) const NO_GOAL: &str = "no goal";
/// No file name or path is longer than these
const MAX_NAME_BYTES: usize = 255;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PROBES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum List {
    Sessions,
    Models,
    Servers,
}

impl List {
    pub(crate) fn empty_text(self) -> &'static str {
        match self {
            List::Sessions => "no other sessions",
            List::Models => "no models",
            List::Servers => "no MCP servers",
        }
    }
}

pub(crate) enum Output {
    Update(Box<SessionUpdate>),
    Notice(Notice),
    Info(String),
    Failure(String),
    Status,
    Queued(Asked),
    Done(Result<StopReason>),
}

pub(crate) struct Session {
    pub(crate) client: Client,
    inbox: Receiver<Incoming>,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) options: Vec<SessionConfigOption>,
    pub(crate) mcp: Vec<McpStatus>,
    /// Servers turned on with `:mcp on`, reported once when they connect or fail
    asked: Vec<String>,
    /// Servers that dropped while running and are reconnecting, reported if they fail
    dropped: Vec<String>,
    pub(crate) goal: Option<Goal>,
    /// Why the goal stopped, reported only after this turn's result is out
    settled: Option<String>,
    /// Set only once the agent reports a save time,
    /// since the first message may be refused or cancelled before it goes out
    saved: bool,
    unsaved: bool,
}

impl Session {
    pub(crate) fn start(
        command: &[String],
        model: Option<String>,
        resume: Option<&str>,
    ) -> Result<Self> {
        let (client, inbox) = Client::start(command)?;
        let opened = client.open(resume, None);
        let opened = match opened {
            Err(e) if e.is::<AuthRequired>() => {
                if !client.authenticate()? {
                    return Err(e);
                }
                client.open(resume, None)?
            }
            opened => opened?,
        };

        let options = match model {
            Some(model) => client.set_model(&opened.id, &opened.options, &model)?,
            None => opened.options,
        };

        Ok(Self {
            client,
            inbox,
            id: opened.id,
            title: String::new(),
            options,
            mcp: Vec::new(),
            asked: Vec::new(),
            dropped: Vec::new(),
            goal: None,
            settled: None,
            saved: resume.is_some(),
            unsaved: false,
        })
    }

    /// Takes one message at a time because later ones must be read after earlier ones are handled
    pub(crate) fn poll(&mut self) -> Option<Vec<Output>> {
        let message = self.inbox.try_recv().ok()?;
        let outputs = self.receive(message);
        Some(outputs)
    }

    pub(crate) fn open(&mut self, resume: Option<&str>) -> Result<()> {
        let opened = self.client.open(resume, Some(&self.id))?;
        if self.id != opened.id {
            self.client.close(&self.id);
        }

        // The chosen model may be gone from the config,
        // so keep the new session's default if switching fails
        let current = models(&self.options).into_iter().find(|pick| pick.current);
        let mut options = opened.options;
        if let (Some(model), None) = (current, resume) {
            let switched = self.client.set_model(&opened.id, &options, &model.value);
            if let Ok(switched) = switched {
                options = switched;
            }
        }

        self.id = opened.id;
        self.options = options;
        self.title.clear();
        self.mcp.clear();
        self.asked.clear();
        self.dropped.clear();
        self.goal = None;
        self.settled = None;
        self.saved = resume.is_some();
        self.unsaved = false;

        Ok(())
    }

    pub(crate) fn rows(&self, list: List) -> Result<Vec<Pick>> {
        let rows = match list {
            List::Sessions => {
                let mut rows = Vec::new();
                for info in self.client.sessions()? {
                    let id = info.session_id.0.to_string();
                    // Reopening the current session would restart Python
                    if id == self.id {
                        continue;
                    }

                    // Each session takes exactly one line
                    let title = info.title.unwrap_or_default();
                    let words: Vec<&str> = title.split_whitespace().collect();
                    let label = match words.is_empty() {
                        true => id.clone(),
                        false => words.join(" "),
                    };
                    let updated = info.updated_at.as_deref().map(stamp);
                    let updated = updated.unwrap_or_default();
                    rows.push(Pick {
                        value: id,
                        label,
                        description: updated,
                        current: false,
                    });
                }
                rows
            }

            List::Models => models(&self.options),

            List::Servers => {
                let mut rows = Vec::new();
                for server in &self.mcp {
                    let state = server.state.to_string();
                    rows.push(Pick {
                        value: server.name.clone(),
                        label: server.name.clone(),
                        description: state,
                        current: false,
                    });
                }
                rows
            }
        };
        Ok(rows)
    }

    pub(crate) fn list(&self, list: List) -> Result<String> {
        let rows = self.rows(list)?;
        if rows.is_empty() {
            return Ok(list.empty_text().to_owned());
        }

        let mut lines = Vec::new();
        for row in rows {
            let mut line = row.value;
            if row.label != line {
                line += &format!("  {}", row.label);
            }
            if !row.description.is_empty() {
                line += &format!("  {}", row.description);
            }
            if row.current {
                line += "  (current)";
            }
            lines.push(line);
        }
        Ok(lines.join("\n"))
    }

    pub(crate) fn model(&self) -> String {
        let named = || self.client.agent.name.clone();
        model_of(&self.options).unwrap_or_else(named)
    }

    pub(crate) fn set_model(&mut self, name: &str) -> Result<String> {
        self.options = self.client.set_model(&self.id, &self.options, name)?;
        let model = self.model();
        Ok(format!("model: {model}"))
    }

    pub(crate) fn set_mcp(&mut self, on: bool, name: &str) -> Result<String> {
        let option = format!("mcp.{name}");
        self.options = self.client.set_option(&self.id, &option, on)?;

        let state = match on {
            true => {
                self.asked.push(name.to_owned());
                McpState::Connecting
            }
            false => {
                self.asked.retain(|asked| asked != name);
                McpState::Off
            }
        };

        // The agent's own state arrives later, so show the switched state until then
        for server in self.mcp.iter_mut().filter(|server| server.name == name) {
            server.state = state.clone();
        }
        Ok(format!("mcp: {name}: {state}"))
    }

    pub(crate) fn clear_goal(&self) -> Result<String> {
        if self.goal.is_none() {
            return Ok(NO_GOAL.to_owned());
        }
        self.client.clear_goal(&self.id)?;
        Ok("goal: cleared".to_owned())
    }

    pub(crate) fn parting(&self) -> Option<String> {
        if self.client.exited() {
            return None;
        }

        let id = &self.id;
        let line = match (self.saved, self.unsaved) {
            (true, false) => format!("session {id}: saved; resume with `smol --resume {id}`"),
            (true, true) => format!("session {id}: the last save failed; `smol --resume {id}` restores the last saved state"),
            (false, true) => format!("session {id}: not saved"),
            (false, false) => return None,
        };
        Some(line)
    }

    pub(crate) fn prompt(&self, text: &str, goal: bool) {
        let blocks = self.blocks(text);
        self.client.prompt(&self.id, blocks, goal);
    }

    pub(crate) fn blocks(&self, text: &str) -> Vec<ContentBlock> {
        let cwd = &self.client.cwd;
        let exists = |path: &str| {
            let misread = cfg!(windows) && dotted(path);
            !misread && resolved(path, cwd).exists()
        };

        let mut blocks = vec![ContentBlock::from(text)];
        for path in mentions(text, exists) {
            let full = resolved(&path, cwd);
            let Ok(uri) = Url::from_file_path(&full) else {
                continue;
            };
            let link = ResourceLink::new(path, uri);
            blocks.push(ContentBlock::ResourceLink(link));
        }

        blocks
    }

    fn receive(&mut self, message: Incoming) -> Vec<Output> {
        match message {
            Incoming::Update(session, update) if session == self.id => {
                let SessionUpdate::SessionInfoUpdate(info) = &*update else {
                    return vec![Output::Update(update)];
                };
                if let MaybeUndefined::Value(title) = &info.title {
                    self.title = title.clone();
                }
                if let MaybeUndefined::Value(_) = &info.updated_at {
                    self.saved = true;
                    self.unsaved = false;
                }
                vec![Output::Status]
            }

            Incoming::Notice(session, notice) if session == self.id => {
                self.unsaved |= notice.unsaved;
                vec![Output::Notice(notice)]
            }

            Incoming::Queued(session, asked) if session == self.id => vec![Output::Queued(asked)],

            Incoming::Goal(session, goal) if session == self.id => {
                // A refused goal reverts to the old one, so a change of objective is never reported
                if let (Some(old), Some(new)) = (&self.goal, &goal) {
                    let same = old.objective == new.objective;
                    if same && old.status != new.status && new.status != GoalStatus::Active {
                        let text = match new.status {
                            GoalStatus::Paused => {
                                "goal: paused; resume with `:goal resume`".to_owned()
                            }
                            GoalStatus::Blocked => {
                                "goal: blocked; reply with what it needs, then `:goal resume`"
                                    .to_owned()
                            }
                            status => format!("goal: {status}"),
                        };
                        self.settled = Some(text);
                    }
                }

                self.goal = goal;
                vec![Output::Status]
            }

            Incoming::Mcp(session, servers) if session == self.id => {
                let mut outputs = Vec::new();
                for server in &servers {
                    let old = self.mcp.iter().find(|old| old.name == server.name);
                    let old = old.map(|old| &old.state);
                    if old == Some(&server.state) {
                        continue;
                    }

                    let name = &server.name;
                    let asked = self.asked.contains(name);
                    let connected = matches!(old, Some(McpState::Connected { .. }));
                    let dropped = connected || self.dropped.contains(name);
                    self.dropped.retain(|dropped| dropped != name);
                    // The agent first tries to reconnect a server that drops after connecting
                    if connected && !asked && server.state == McpState::Connecting {
                        self.dropped.push(name.clone());
                    }

                    let text = format!("mcp: {name}: {}", server.state);
                    let output = match &server.state {
                        McpState::Connected { .. } if asked => Output::Info(text),
                        McpState::Failed { .. } if asked || dropped => Output::Failure(text),
                        // States sent before the switch still show it off,
                        // so wait for this connection's result
                        _ => continue,
                    };

                    self.asked.retain(|asked| asked != name);
                    outputs.push(output);
                }

                self.mcp = servers;
                outputs.push(Output::Status);
                outputs
            }

            Incoming::Exited(text) => vec![Output::Failure(text)],

            Incoming::Done(stop) => {
                let mut outputs = vec![Output::Done(stop)];
                let settled = self.settled.take().map(Output::Info);
                outputs.extend(settled);
                outputs
            }

            _ => Vec::new(),
        }
    }
}

fn stamp(rfc3339: &str) -> String {
    let Ok(time) = DateTime::parse_from_rfc3339(rfc3339) else {
        let spaced = rfc3339.replacen('T', " ", 1);
        return spaced.chars().take(16).collect();
    };
    let local = time.with_timezone(&Local);
    local.format("%Y-%m-%d %H:%M").to_string()
}

pub(crate) fn opens_mention(before: &str) -> bool {
    let previous = before.chars().next_back();
    let named =
        |c: char| c.is_ascii_alphanumeric() || is_separator(c) || matches!(c, '.' | '-' | '_');
    !previous.is_some_and(named)
}

pub(crate) fn resolved(path: &str, cwd: &Path) -> PathBuf {
    let home = env::home_dir();
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home,
        // Joining an absolute path would replace the home directory entirely
        (Some(rest), Some(home)) if rest.starts_with(is_separator) => {
            let rest = rest.trim_start_matches(is_separator);
            home.join(rest)
        }
        _ => cwd.join(path),
    }
}

/// Windows strips trailing dots from a name when opening it
fn dotted(path: &str) -> bool {
    let name = path.rsplit(is_separator).next().unwrap_or(path);
    name.ends_with('.') && !name.trim_matches('.').is_empty()
}

/// Chinese text often follows a path with no space, so this takes the longest prefix that exists
fn mentions(text: &str, exists: impl Fn(&str) -> bool) -> Vec<String> {
    // Pasted JSON can hold thousands of @ signs and each one costs a disk lookup
    let mut probes = 0;
    let mut exists = |path: &str| {
        probes += 1;
        probes <= MAX_PROBES && exists(path)
    };

    // A prefix still followed by path characters is a piece cut out of a mistyped path
    let continues = |rest: &str| {
        let mut chars = rest.chars();
        match chars.next() {
            Some('.') => chars.next().is_some_and(|c| c.is_ascii_alphanumeric()),
            Some(c) => c.is_ascii_alphanumeric() || is_separator(c) || matches!(c, '_' | '-' | '~'),
            None => false,
        }
    };

    let mut found = Vec::new();
    for (at, _) in text.match_indices('@') {
        if !opens_mention(&text[..at]) {
            continue;
        }

        let after = &text[at + 1..];
        let path = match after.strip_prefix('"') {
            Some(quoted) => {
                let path = quoted.split_once('"').map(|(path, _)| path);
                path.filter(|path| !path.is_empty() && exists(path))
            }
            None => {
                // Probing the disk for every prefix of long text without whitespace would be slow
                let mut ends = Vec::new();
                let mut part = 0;
                for (start, c) in after.char_indices() {
                    part = if is_separator(c) {
                        0
                    } else {
                        part + c.len_utf8()
                    };
                    let end = start + c.len_utf8();
                    if c.is_whitespace() || part > MAX_NAME_BYTES || end > MAX_PATH_BYTES {
                        break;
                    }
                    ends.push(end);
                }
                let ended = |end: &usize| !continues(&after[*end..]);
                let ends = ends.into_iter().rev().filter(ended);
                ends.map(|end| &after[..end]).find(|path| exists(path))
            }
        };

        if let Some(path) = path.filter(|path| !found.contains(path)) {
            found.push(path);
        }
    }

    found.into_iter().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use crate::chat::*;
    use crate::tests::{fake, shell};
    use agent_client_protocol_schema::v1::SessionInfoUpdate;
    use std::cell::Cell;
    use std::thread;
    use std::time::Duration;

    fn status(session: &str, state: McpState) -> Incoming {
        let server = McpStatus {
            name: "web".to_owned(),
            state,
        };
        Incoming::Mcp(session.to_owned(), vec![server])
    }

    fn info(session: &str, info: SessionInfoUpdate) -> Incoming {
        let update = SessionUpdate::SessionInfoUpdate(info);
        let update = Box::new(update);
        Incoming::Update(session.to_owned(), update)
    }

    fn shown(outputs: Vec<Output>) -> Vec<String> {
        let mut shown = Vec::new();
        for output in outputs {
            match output {
                Output::Info(text) => shown.push(text),
                Output::Failure(text) => shown.push(format!("✗ {text}")),
                _ => {}
            }
        }
        shown
    }

    #[test]
    fn asked_until_settled() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");

        let switched = session.set_mcp(true, "web").expect("switch");
        assert_eq!(switched, "mcp: web: connecting");

        let stale = status("s1", McpState::Off);
        let stale = session.receive(stale);
        assert!(shown(stale).is_empty());
        let other = status("s9", McpState::Connected { tools: 1 });
        let other = session.receive(other);
        assert!(other.is_empty());

        let connected = status("s1", McpState::Connected { tools: 2 });
        let connected = session.receive(connected);
        assert_eq!(shown(connected), ["mcp: web: connected, 2 tools"]);
        let off = status("s1", McpState::Off);
        let off = session.receive(off);
        let again = status("s1", McpState::Connected { tools: 2 });
        let again = session.receive(again);
        assert!(shown(off).is_empty() && shown(again).is_empty());

        let error = "disconnected".to_owned();
        let dropped = status("s1", McpState::Failed { error });
        let dropped = session.receive(dropped);
        assert_eq!(shown(dropped), ["✗ mcp: web: failed: disconnected"]);
        let connecting = status("s1", McpState::Connecting);
        session.receive(connecting);
        let error = "connection refused".to_owned();
        let failed = status("s1", McpState::Failed { error });
        let failed = session.receive(failed);
        assert!(shown(failed).is_empty());

        session.set_mcp(true, "web").expect("switch");
        let error = "refused again".to_owned();
        let failed = status("s1", McpState::Failed { error });
        let failed = session.receive(failed);
        assert_eq!(shown(failed), ["✗ mcp: web: failed: refused again"]);

        let switched = session.set_mcp(false, "web").expect("switch");
        assert_eq!(switched, "mcp: web: off");
        assert_eq!(session.mcp[0].state, McpState::Off);
    }

    #[test]
    fn reconnect_failure_shown() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");
        let mut seen = |state: McpState| {
            let message = status("s1", state);
            let outputs = session.receive(message);
            shown(outputs)
        };

        assert!(seen(McpState::Connected { tools: 2 }).is_empty());
        assert!(seen(McpState::Connecting).is_empty());
        assert!(seen(McpState::Connected { tools: 2 }).is_empty());

        assert!(seen(McpState::Connecting).is_empty());
        let error =
            "server disconnected\nwritten before the crash\nconnect: connection refused".to_owned();
        let failed = seen(McpState::Failed { error });
        let reported = "✗ mcp: web: failed: server disconnected\nwritten before the crash\nconnect: connection refused";
        assert_eq!(failed, [reported]);
        assert!(seen(McpState::Connecting).is_empty());
        let error = "connection refused".to_owned();
        assert!(seen(McpState::Failed { error }).is_empty());
    }

    #[test]
    fn open_resets_state() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");
        let off = status("s1", McpState::Off);
        session.receive(off);
        session.set_mcp(true, "web").expect("switch");
        let titled = SessionInfoUpdate::new().title("Snake game".to_owned());
        let titled = info("s1", titled);
        session.receive(titled);
        assert_eq!(session.title, "Snake game");
        assert!(!session.saved);
        let stamped = SessionInfoUpdate::new().updated_at("2026-10-12T00:00:00Z".to_owned());
        let stamped = info("s1", stamped);
        session.receive(stamped);
        assert!(session.saved);
        let goal = Goal {
            objective: "fix the café parser".to_owned(),
            status: GoalStatus::Active,
            updates: Vec::new(),
        };
        let goal = Incoming::Goal("s1".to_owned(), Some(goal));
        session.receive(goal);
        assert!(session.goal.is_some());

        session.open(None).expect("open");
        assert_eq!((session.id.as_str(), session.mcp.len()), ("s2", 0));
        assert_eq!((session.title.as_str(), &session.goal), ("", &None));
        assert!(!session.saved);
        session.open(Some("s1")).expect("resume");
        assert!(session.saved);

        let connected = status("s1", McpState::Connected { tools: 1 });
        let connected = session.receive(connected);
        assert!(shown(connected).is_empty());
        let missing = session.open(Some("nope"));
        assert!(missing.is_err());
        assert_eq!(session.id, "s1");
    }

    #[test]
    fn goal_reported() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");
        let mut report = |objective: &str, status: GoalStatus| {
            let objective = objective.to_owned();
            let goal = Goal {
                objective,
                status,
                updates: Vec::new(),
            };
            let message = Incoming::Goal("s1".to_owned(), Some(goal));
            let held = session.receive(message);
            assert!(shown(held).is_empty());

            let done = Incoming::Done(Ok(StopReason::EndTurn));
            let done = session.receive(done);
            assert!(matches!(done[0], Output::Done(..)));
            shown(done)
        };

        assert!(report("fix the parser", GoalStatus::Paused).is_empty());
        assert!(report("migrate the database", GoalStatus::Active).is_empty());
        assert!(report("fix the parser", GoalStatus::Paused).is_empty());
        assert!(report("fix the parser", GoalStatus::Active).is_empty());
        let blocked = "goal: blocked; reply with what it needs, then `:goal resume`";
        assert_eq!(report("fix the parser", GoalStatus::Blocked), [blocked]);
        assert!(report("fix the parser", GoalStatus::Blocked).is_empty());
        let paused = "goal: paused; resume with `:goal resume`";
        assert_eq!(report("fix the parser", GoalStatus::Paused), [paused]);
        assert_eq!(
            report("fix the parser", GoalStatus::Complete),
            ["goal: complete"]
        );
    }

    #[test]
    fn last_save_failed() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");
        let failed = || Notice {
            text: "history: disk full".to_owned(),
            retry_in: None,
            step: false,
            unsaved: true,
        };
        let notice = Incoming::Notice("s1".to_owned(), failed());
        session.receive(notice);
        assert_eq!(session.parting().as_deref(), Some("session s1: not saved"));

        let stamped = SessionInfoUpdate::new().updated_at("2026-10-18T00:00:00Z".to_owned());
        let stamped = info("s1", stamped);
        session.receive(stamped);
        let saved = "session s1: saved; resume with `smol --resume s1`";
        assert_eq!(session.parting().as_deref(), Some(saved));

        let notice = Incoming::Notice("s1".to_owned(), failed());
        session.receive(notice);
        let unsaved =
            "session s1: the last save failed; `smol --resume s1` restores the last saved state";
        assert_eq!(session.parting().as_deref(), Some(unsaved));
    }

    #[test]
    fn lists_formatted() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");
        let list = |session: &Session, list: List| session.list(list).expect("list");

        assert_eq!(list(&session, List::Models), "m1  (current)\nm2-café");
        assert_eq!(list(&session, List::Sessions), "no other sessions");
        assert_eq!(list(&session, List::Servers), "no MCP servers");

        session.open(None).expect("open");
        assert_eq!(list(&session, List::Sessions), "s1  old session");
    }

    #[test]
    fn dropped_ask_answered() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None).expect("start");
        session.prompt("queue", false);

        let mut stops = Vec::new();
        for _ in 0..500 {
            while let Some(outputs) = session.poll() {
                for output in outputs {
                    if let Output::Done(stop) = output {
                        stops.push(stop.ok());
                    }
                }
            }
            if !stops.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(stops, [Some(StopReason::Cancelled)]);
    }

    #[test]
    fn model_carried() {
        let agent = fake();
        let model = Some("m2-café".to_owned());
        let resumed = Session::start(&agent, model, Some("s1")).expect("start");
        assert_eq!(resumed.model(), "m2-café");

        let mut session = Session::start(&agent, None, None).expect("start");
        session.set_model("m2-café").expect("switch");
        session.open(None).expect("open");
        assert_eq!(
            (session.id.as_str(), session.model()),
            ("s2", "m2-café".to_owned())
        );
        session.open(Some("s1")).expect("resume");
        assert_eq!(session.model(), "m1");
    }

    #[test]
    fn auth_before_open() {
        let script = r#"
if [ "$0" = setup ]; then
    [ "$NOTE" = café ] || exit 2
    exit EXIT
fi
while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case $line in
    *'"method":"initialize"'*'"terminal":true'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"authMethods":[{"type":"terminal","id":"setup","name":"Set up","args":["setup"],"env":{"NOTE":"café"}}]}}\n' "$id" ;;
    *'"method":"initialize"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id" ;;
    *'"method":"session/new"'*)
        if [ "$opened" ]; then
            printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id"
        else
            opened=1
            printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"config: model.toml: not found"}}\n' "$id"
        fi ;;
    esac
done
"#;
        let agent = |exit: &str| {
            let script = script.replace("EXIT", exit);
            shell(&script)
        };

        let session = Session::start(&agent("0"), None, None).expect("start");
        assert_eq!(session.id, "s1");

        let failed = Session::start(&agent("1"), None, None);
        let error = failed.err().expect("error");
        assert_eq!(error.to_string(), "config: model.toml: not found");
    }

    #[test]
    fn latest_in_cwd() {
        let agent = fake();
        let started = Session::start(&agent, None, Some("latest"));
        let mut session = started.expect("start");
        assert_eq!(session.id, "s1");

        let reopened = session.open(Some("latest"));
        let error = reopened.expect_err("latest");
        assert!(error.to_string().ends_with("nothing to resume"), "{error}");
    }

    #[test]
    fn short_stamp() {
        let utc = "2026-10-04T08:09:10+00:00";
        let time = DateTime::parse_from_rfc3339(utc).expect("time");
        let local = time.with_timezone(&Local);
        let expected = local.format("%Y-%m-%d %H:%M").to_string();
        assert_eq!(stamp(utc), expected);
        assert_eq!(stamp("2026-10-04T08:09:10"), "2026-10-04 08:09");
    }

    #[test]
    fn paths_resolved() {
        let cwd = Path::new("/café");
        assert_eq!(
            resolved("src/naïve.rs", cwd),
            Path::new("/café/src/naïve.rs")
        );
        assert_eq!(resolved("/etc/über", cwd), Path::new("/etc/über"));
        assert_eq!(resolved("~josé", cwd), Path::new("/café/~josé"));

        let Some(home) = env::home_dir() else {
            return;
        };
        assert_eq!(resolved("~", cwd), home);
        assert_eq!(resolved("~/résumé.md", cwd), home.join("résumé.md"));
        assert_eq!(resolved("~//etc", cwd), home.join("etc"));
        #[cfg(windows)]
        assert_eq!(resolved("~\\résumé.md", cwd), home.join("résumé.md"));
    }

    #[test]
    fn mentions_found() {
        let files = [
            "src/main.rs",
            "src",
            "my file.txt",
            "jalapeño.py",
            "a.md",
            "café/@types/x.ts",
            "types/x.ts",
        ];
        let found = |text| mentions(text, |path| files.contains(&path));

        assert_eq!(found("see @src/main.rs and @src"), ["src/main.rs", "src"]);
        assert_eq!(found("@café/@types/x.ts too"), ["café/@types/x.ts"]);
        assert_eq!(
            found("the parser—@src/main.rs—still panics"),
            ["src/main.rs"]
        );
        assert_eq!(found("@jalapeño.py, run it"), ["jalapeño.py"]);
        assert_eq!(
            found("read @\"my file.txt\" twice @a.md @a.md"),
            ["my file.txt", "a.md"]
        );

        let none = [
            "mail me@a.md",
            "name_@a.md",
            "@nothing",
            "@ src",
            "@\"\"",
            "@\"my file.txt",
            "@",
        ];
        for text in none {
            assert!(found(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn partial_paths_skipped() {
        let files = ["/", ".", "~", "src", "src/main.rs", "README.md", "résumé"];
        let found = |text| mentions(text, |path| files.contains(&path));

        let none = [
            "import Foo from '@/components/Foo.vue'",
            "check @.env",
            "@src/mian.rs is broken",
            "@README.md.bak",
        ];
        for text in none {
            assert!(found(text).is_empty(), "{text}: {:?}", found(text));
        }

        assert_eq!(found("see @README.md."), ["README.md"]);
        assert_eq!(found("@src/main.rs:12 this line"), ["src/main.rs"]);
        assert_eq!(found("@résumé’s typos"), ["résumé"]);
    }

    #[test]
    fn trailing_dots() {
        assert!(dotted("café.png."));
        assert!(dotted("docs/notes."));
        for path in ["café.png", ".", "..", "src/.", "src/..", "café.png…"] {
            assert!(!dotted(path), "{path}");
        }
    }

    #[test]
    fn long_words_bounded() {
        let calls = Cell::new(0);
        let longest = Cell::new(0);
        let counted = |path: &str| {
            calls.set(calls.get() + 1);
            longest.set(longest.get().max(path.len()));
            false
        };

        let minified = "{\"@type\":\"Café\"}".repeat(10_000);
        assert!(mentions(&minified, counted).is_empty());
        assert!(calls.get() <= MAX_PROBES, "{}", calls.get());

        calls.set(0);
        let nested = format!("@{}", "café/".repeat(100_000));
        assert!(mentions(&nested, counted).is_empty());
        assert!(calls.get() <= MAX_PATH_BYTES, "{}", calls.get());
        assert!(longest.get() <= MAX_PATH_BYTES, "{}", longest.get());
    }
}
