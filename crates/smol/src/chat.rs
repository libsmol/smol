use crate::client::{
    chosen, offerings, Asked, Client, Incoming, McpState, McpStatus, Memory, Offering, Refusal,
    Selector,
};
use crate::conversation::{Conversation, Entry};
use agent_client_protocol_schema::v1::{
    ContentBlock, ResourceLink, SessionConfigOption, SessionInfo, SessionUpdate,
};
use agent_client_protocol_schema::MaybeUndefined;
use anyhow::Result;
use reaper::wire::MCP_PREFIX;
use std::env;
use std::path::{is_separator, Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;
use url::Url;

/// No file name or path is longer than these
const MAX_NAME_BYTES: usize = 255;
const MAX_PATH_BYTES: usize = 4096;

const MAX_PROBES: usize = 4096;

pub(crate) enum Output {
    /// Nothing beyond the conversation or the session changed, which only needs drawing
    Shown,
    /// The states of the MCP servers changed
    Servers,
    Queued(Asked),
    Done,
}

pub(crate) struct Run {
    pub(crate) step_started: Instant,
    pub(crate) interrupting: bool,
}

impl Run {
    fn new() -> Self {
        Self {
            step_started: Instant::now(),
            interrupting: false,
        }
    }
}

pub(crate) struct Session {
    pub(crate) client: Client,
    inbox: Receiver<Incoming>,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) options: Vec<SessionConfigOption>,
    pub(crate) mcp: Vec<McpStatus>,
    pub(crate) conversation: Conversation,
    /// The turn in progress
    pub(crate) run: Option<Run>,
    /// Servers turned on with `:mcp on`, reported once when they connect or fail
    switched_on: Vec<String>,
    /// Servers that dropped while running and are reconnecting, reported if they fail
    dropped: Vec<String>,
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
        images_shown: bool,
    ) -> Result<Self> {
        let (client, inbox) = Client::start(command, images_shown)?;
        let opened = client.open(resume, None);
        let opened = match opened {
            Err(e) if matches!(e.downcast_ref::<Refusal>(), Some(Refusal::AuthRequired(_))) => {
                if !client.authenticate()? {
                    return Err(e);
                }
                client.open(resume, None)?
            }
            opened => opened?,
        };

        let options = match model {
            Some(model) => client.choose(&opened.id, &opened.options, Selector::Model, &model)?,
            None => opened.options,
        };

        Ok(Self {
            client,
            inbox,
            id: opened.id,
            title: String::new(),
            options,
            mcp: Vec::new(),
            conversation: Conversation::default(),
            run: None,
            switched_on: Vec::new(),
            dropped: Vec::new(),
            saved: resume.is_some(),
            unsaved: false,
        })
    }

    /// Takes one message at a time because later ones must be read after earlier ones are handled,
    /// and is None when none has arrived
    pub(crate) fn poll(&mut self) -> Option<Output> {
        let message = self.inbox.try_recv().ok()?;
        let output = self.receive(message);
        Some(output)
    }

    /// Only between turns, since a prompt's reply names no session
    pub(crate) fn open(&mut self, resume: Option<&str>) -> Result<()> {
        let opened = self.client.open(resume, Some(&self.id))?;
        if self.id != opened.id {
            self.client.close(&self.id);
        }

        // A model or effort gone from the config fails to switch and leaves the default
        let mut options = opened.options;
        if resume.is_none() {
            // The effort goes second, as switching models resets it
            for selector in [Selector::Model, Selector::Effort] {
                let offered = self.offerings(selector);
                let current = offered.into_iter().find(|offering| offering.current);
                let Some(current) = current else {
                    continue;
                };

                let switched = self
                    .client
                    .choose(&opened.id, &options, selector, &current.value);
                if let Ok(switched) = switched {
                    options = switched;
                }
            }
        }

        self.id = opened.id;
        self.options = options;
        self.title.clear();
        self.mcp.clear();
        self.conversation = Conversation::default();
        self.switched_on.clear();
        self.dropped.clear();
        self.saved = resume.is_some();
        self.unsaved = false;

        Ok(())
    }

    /// Leaves this session out, since reopening it would restart Python
    pub(crate) fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut sessions = self.client.sessions()?;
        sessions.retain(|info| *info.session_id.0 != self.id);
        Ok(sessions)
    }

    pub(crate) fn offerings(&self, selector: Selector) -> Vec<Offering> {
        offerings(&self.options, selector)
    }

    pub(crate) fn memories(&self) -> Result<Vec<Memory>> {
        self.client.memories(&self.id)
    }

    pub(crate) fn model(&self) -> String {
        let named = || self.client.agent.name.clone();
        chosen(&self.options, Selector::Model).unwrap_or_else(named)
    }

    pub(crate) fn effort(&self) -> Option<String> {
        chosen(&self.options, Selector::Effort)
    }

    pub(crate) fn choose(&mut self, selector: Selector, value: &str) -> Result<()> {
        self.options = self
            .client
            .choose(&self.id, &self.options, selector, value)?;
        Ok(())
    }

    /// Returns the server in the state it was switched to
    pub(crate) fn set_mcp(&mut self, on: bool, name: &str) -> Result<McpStatus> {
        let option = format!("{MCP_PREFIX}{name}");
        self.options = self.client.set_option(&self.id, &option, on)?;

        let state = match on {
            true => {
                self.switched_on.push(name.to_owned());
                McpState::Connecting
            }
            false => {
                self.switched_on.retain(|switched| switched != name);
                McpState::Off
            }
        };

        // The agent's own state arrives later, so show the switched state until then
        for server in self.mcp.iter_mut().filter(|server| server.name == name) {
            server.state = state.clone();
        }

        let name = name.to_owned();
        Ok(McpStatus { name, state })
    }

    pub(crate) fn forget(&self, id: i64) -> Result<()> {
        self.client.forget(&self.id, id)
    }

    /// Called before the client drops, so an exited agent ended on its own
    pub(crate) fn parting(&self) -> Option<String> {
        let id = &self.id;
        let line = match (self.saved, self.unsaved, self.client.exited()) {
            (true, _, true) => format!("session {id}: the agent exited; `smol --resume {id}` restores the last saved state"),
            (true, false, false) => format!("session {id}: saved; resume with `smol --resume {id}`"),
            (true, true, false) => format!("session {id}: the last save failed; `smol --resume {id}` restores the last saved state"),
            (false, true, _) => format!("session {id}: not saved"),
            (false, false, _) => return None,
        };
        Some(line)
    }

    pub(crate) fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| !run.interrupting)
    }

    pub(crate) fn prompt(&mut self, text: &str) {
        self.conversation.task(text);
        self.run = Some(Run::new());
        let blocks = self.blocks(text);
        self.client.prompt(&self.id, blocks);
    }

    pub(crate) fn notify_queued(&self) {
        self.client.notify_queued(&self.id);
    }

    /// Gives the running turn the messages queued for it, which may be none
    pub(crate) fn answer(&mut self, asked: Asked, texts: impl IntoIterator<Item = String>) {
        let mut prompts = Vec::new();
        for text in texts {
            self.conversation.amend(&text);
            let blocks = self.blocks(&text);
            prompts.push(blocks);
        }
        asked.answer(prompts);
    }

    pub(crate) fn shell(&mut self, command: &str) {
        self.run = Some(Run::new());
        self.client.shell(&self.id, command);
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(run) = &mut self.run {
            run.interrupting = true;
            self.client.cancel(&self.id);
        }
    }

    fn blocks(&self, text: &str) -> Vec<ContentBlock> {
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

    fn receive(&mut self, message: Incoming) -> Output {
        match message {
            Incoming::Update(session, update) if session == self.id => {
                if let SessionUpdate::SessionInfoUpdate(info) = &*update {
                    if let MaybeUndefined::Value(title) = &info.title {
                        self.title = title.clone();
                    }
                    if let MaybeUndefined::Value(_) = &info.updated_at {
                        self.saved = true;
                        self.unsaved = false;
                    }
                    return Output::Shown;
                }

                let ended = self.conversation.update(&update);
                if let Some(run) = self.run.as_mut().filter(|_| ended) {
                    run.step_started = Instant::now();
                }
                Output::Shown
            }

            Incoming::Notice(session, notice) if session == self.id => {
                self.unsaved |= notice.unsaved;
                self.conversation.notice(&notice);
                Output::Shown
            }

            Incoming::Queued(session, asked) if session == self.id => Output::Queued(asked),

            Incoming::Mcp(session, servers) if session == self.id => {
                for server in &servers {
                    let old = self.mcp.iter().find(|old| old.name == server.name);
                    let old = old.map(|old| &old.state);
                    if old == Some(&server.state) {
                        continue;
                    }

                    let name = &server.name;
                    let switched_on = self.switched_on.contains(name);
                    let connected = matches!(old, Some(McpState::Connected { .. }));
                    let dropped = connected || self.dropped.contains(name);
                    self.dropped.retain(|dropped| dropped != name);
                    // The agent first tries to reconnect a server that drops after connecting
                    if connected && !switched_on && server.state == McpState::Connecting {
                        self.dropped.push(name.clone());
                    }

                    let text = server.to_string();
                    let entry = match &server.state {
                        McpState::Connected { .. } if switched_on => Entry::Info(text),
                        McpState::Failed { .. } if switched_on || dropped => Entry::Failure(text),
                        McpState::Unauthorized { .. } if switched_on || dropped => {
                            Entry::Warning(text)
                        }
                        // States sent before the switch still show it off,
                        // so wait for this connection's result
                        _ => continue,
                    };

                    self.switched_on.retain(|switched| switched != name);
                    self.conversation.push(entry);
                }

                self.mcp = servers;
                Output::Servers
            }

            Incoming::Exited(text) => {
                self.conversation.push(Entry::Failure(text));
                Output::Shown
            }

            Incoming::Done(stop) => {
                self.run = None;
                self.conversation.done(stop);
                Output::Done
            }

            _ => Output::Shown,
        }
    }
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
    use crate::client::Notice;
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

    /// The infos, warnings and failures that receiving `message` added
    fn shown(session: &mut Session, message: Incoming) -> Vec<String> {
        let before = session.conversation.entries().len();
        session.receive(message);

        let mut shown = Vec::new();
        for entry in &session.conversation.entries()[before..] {
            match entry {
                Entry::Info(text) => shown.push(text.clone()),
                Entry::Warning(text) => shown.push(format!("! {text}")),
                Entry::Failure(text) => shown.push(format!("✗ {text}")),
                _ => {}
            }
        }
        shown
    }

    #[test]
    fn switched_until_settled() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");

        let switched = session.set_mcp(true, "web").expect("switch");
        assert_eq!(switched.to_string(), "mcp: web: connecting");

        let stale = status("s1", McpState::Off);
        assert!(shown(&mut session, stale).is_empty());
        let other = status("s9", McpState::Connected { tools: 1 });
        assert!(matches!(session.receive(other), Output::Shown));

        let connected = status("s1", McpState::Connected { tools: 2 });
        let connected = shown(&mut session, connected);
        assert_eq!(connected, ["mcp: web: connected, 2 tools"]);
        let off = status("s1", McpState::Off);
        let off = shown(&mut session, off);
        let again = status("s1", McpState::Connected { tools: 2 });
        let again = shown(&mut session, again);
        assert!(off.is_empty() && again.is_empty());

        let error = "disconnected".to_owned();
        let dropped = status("s1", McpState::Failed { error });
        let dropped = shown(&mut session, dropped);
        assert_eq!(dropped, ["✗ mcp: web: failed: disconnected"]);
        let connecting = status("s1", McpState::Connecting);
        session.receive(connecting);
        let error = "connection refused".to_owned();
        let failed = status("s1", McpState::Failed { error });
        assert!(shown(&mut session, failed).is_empty());

        session.set_mcp(true, "web").expect("switch");
        let error = "refused again".to_owned();
        let failed = status("s1", McpState::Failed { error });
        let failed = shown(&mut session, failed);
        assert_eq!(failed, ["✗ mcp: web: failed: refused again"]);

        let switched = session.set_mcp(false, "web").expect("switch");
        assert_eq!(switched.to_string(), "mcp: web: off");
        assert_eq!(session.mcp[0].state, McpState::Off);
    }

    #[test]
    fn reconnect_failure_shown() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");
        let mut seen = |state: McpState| {
            let message = status("s1", state);
            shown(&mut session, message)
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

        assert!(seen(McpState::Connected { tools: 2 }).is_empty());
        let error = "authorization required".to_owned();
        let refused = seen(McpState::Unauthorized { error });
        assert_eq!(
            refused,
            ["! mcp: web: unauthorized: authorization required"]
        );
    }

    #[test]
    fn open_resets_state() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");
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

        session.open(None).expect("open");
        assert_eq!((session.id.as_str(), session.mcp.len()), ("s2", 0));
        assert_eq!(session.title, "");
        assert!(!session.saved);
        session.open(Some("s1")).expect("resume");
        assert!(session.saved);

        let connected = status("s1", McpState::Connected { tools: 1 });
        assert!(shown(&mut session, connected).is_empty());
        let missing = session.open(Some("nope"));
        assert!(missing.is_err());
        assert_eq!(session.id, "s1");
    }

    #[test]
    fn last_save_failed() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");
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
    fn parting_after_exit() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");
        let stamped = SessionInfoUpdate::new().updated_at("2026-10-18T00:00:00Z".to_owned());
        let stamped = info("s1", stamped);
        session.receive(stamped);

        session.prompt("exit");
        for _ in 0..500 {
            while session.poll().is_some() {}
            if session.run.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let exited =
            "session s1: the agent exited; `smol --resume s1` restores the last saved state";
        assert_eq!(session.parting().as_deref(), Some(exited));
    }

    #[test]
    fn dropped_ask_answered() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");
        session.prompt("queue");

        let mut done = false;
        for _ in 0..500 {
            while let Some(output) = session.poll() {
                done |= matches!(output, Output::Done);
            }
            if done {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(done && session.run.is_none());
        let failed = |entry: &Entry| matches!(entry, Entry::Failure(_));
        assert!(!session.conversation.entries().iter().any(failed));
    }

    #[test]
    fn model_carried() {
        let agent = fake();
        let model = Some("m2-café".to_owned());
        let resumed = Session::start(&agent, model, Some("s1"), false).expect("start");
        assert_eq!(resumed.model(), "m2-café");

        let mut session = Session::start(&agent, None, None, false).expect("start");
        session.choose(Selector::Model, "m2-café").expect("switch");
        session.open(None).expect("open");
        assert_eq!(
            (session.id.as_str(), session.model()),
            ("s2", "m2-café".to_owned())
        );
        session.open(Some("s1")).expect("resume");
        assert_eq!(session.model(), "m1");

        // A new session keeps the chosen effort after the model, which would reset it
        assert_eq!(session.effort().as_deref(), Some("high"));
        session.choose(Selector::Effort, "low").expect("switch");
        session.open(None).expect("open");
        assert_eq!(session.effort().as_deref(), Some("low"));
        session.open(Some("s1")).expect("resume");
        assert_eq!(session.effort().as_deref(), Some("high"));
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

        let session = Session::start(&agent("0"), None, None, false).expect("start");
        assert_eq!(session.id, "s1");

        let failed = Session::start(&agent("1"), None, None, false);
        let error = failed.err().expect("error");
        assert_eq!(error.to_string(), "config: model.toml: not found");
    }

    #[test]
    fn latest_in_cwd() {
        let agent = fake();
        let started = Session::start(&agent, None, Some("latest"), false);
        let mut session = started.expect("start");
        assert_eq!(session.id, "s1");

        let reopened = session.open(Some("latest"));
        let error = reopened.expect_err("latest");
        assert!(error.to_string().ends_with("nothing to resume"), "{error}");
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
