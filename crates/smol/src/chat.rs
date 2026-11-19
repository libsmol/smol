use crate::client::{
    chosen, current, mcp_line, offerings, Asked, Client, Event, Incoming, Offering, Program,
    Refusal, Selector,
};
use crate::conversation::{Conversation, Entry, Outcome};
use agent_client_protocol_schema::v1::{
    ContentBlock, ImageContent, ResourceLink, SessionConfigOption, SessionInfo, SessionUpdate,
};
use agent_client_protocol_schema::MaybeUndefined;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageFormat;
use reaper::wire::{McpState, McpStatus, Memory, MCP_PREFIX};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::path::{is_separator, Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;
use url::Url;

/// No file name or path is longer than these
const MAX_NAME_BYTES: usize = 255;
const MAX_PATH_BYTES: usize = 4096;

const MAX_PROBES: usize = 4096;

const IMAGE_PREFIX: &str = "[image #";
/// The formats smol draws and pastes, all of which libsmol takes
pub(crate) const IMAGE_FORMATS: [ImageFormat; 4] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Gif,
    ImageFormat::WebP,
];

pub(crate) enum Output {
    /// Nothing beyond the conversation or the session changed, which only needs drawing
    Shown,
    /// The states of the MCP servers changed
    Servers,
    Queued(Asked),
    /// Programs the code left running ended while no turn ran
    Background,
    Done(Outcome),
}

pub(crate) struct Run {
    pub(crate) step_started: Instant,
    pub(crate) interrupting: bool,
}

/// An image pasted into the input, which the text names with `image_placeholder`
pub(crate) struct Pasted {
    mime: &'static str,
    data: Vec<u8>,
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
    /// The programs the code left running that still run
    pub(crate) programs: Vec<Program>,
    pub(crate) conversation: Conversation,
    /// The turn in progress
    pub(crate) run: Option<Run>,
    /// By number, and kept across sessions, as a queued or recalled message still names them
    pub(crate) images: BTreeMap<usize, Pasted>,
    /// Servers turned on with `:mcp on`, reported once when they connect or fail
    switched_on: Vec<String>,
    /// Servers that dropped while running and are reconnecting, reported if they fail
    dropped: Vec<String>,
    /// Set only once the agent reports a save time,
    /// since the first message may be refused or cancelled before it goes out
    stored: bool,
    /// The last save failed
    save_failed: bool,
}

impl Session {
    pub(crate) fn start(
        command: &[OsString],
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
            programs: Vec::new(),
            conversation: Conversation::default(),
            run: None,
            images: BTreeMap::new(),
            switched_on: Vec::new(),
            dropped: Vec::new(),
            stored: resume.is_some(),
            save_failed: false,
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
                let Some(current) = current(&self.options, selector) else {
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

        // Every field is named, so one added later can't be left over from the session before
        let Self {
            client: _,
            inbox: _,
            id,
            title,
            options: offered,
            mcp,
            programs,
            conversation,
            run: _,
            images: _,
            switched_on,
            dropped,
            stored,
            save_failed,
        } = self;
        *id = opened.id;
        *offered = options;
        title.clear();
        mcp.clear();
        programs.clear();
        *conversation = Conversation::default();
        switched_on.clear();
        dropped.clear();
        *stored = resume.is_some();
        *save_failed = false;

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
        let line = match (self.stored, self.save_failed, self.client.exited()) {
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

    /// The image's number, or why it isn't one smol pastes
    pub(crate) fn paste_image(&mut self, data: Vec<u8>) -> Result<usize, String> {
        let format = image::guess_format(&data).ok();
        let Some(format) = format.filter(|format| IMAGE_FORMATS.contains(format)) else {
            return Err("image: not PNG, JPEG, GIF or WebP".to_owned());
        };
        let mime = format.to_mime_type();

        // A resumed conversation names images of an earlier run, which a new number must not take
        let sent = self.conversation.sent();
        let named = sent.iter().flat_map(|text| named_images(text)).max();
        let last = self.images.keys().next_back().copied();
        let number = named.max(last).unwrap_or(0) + 1;
        self.images.insert(number, Pasted { mime, data });
        Ok(number)
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

    /// The agent already showed the programs that ended, which open the turn
    pub(crate) fn background(&mut self) {
        self.run = Some(Run::new());
        self.client.background(&self.id);
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
        for number in named_images(text) {
            let Some(pasted) = self.images.get(&number) else {
                continue;
            };
            let data = STANDARD.encode(&pasted.data);
            let image = ImageContent::new(data, pasted.mime);
            blocks.push(ContentBlock::Image(image));
        }

        blocks
    }

    fn receive(&mut self, message: Incoming) -> Output {
        let event = match message {
            Incoming::Session(session, event) if session == self.id => event,
            // A session left behind may still send a few
            Incoming::Session(..) => return Output::Shown,

            Incoming::Exited(text) => {
                self.conversation.push(Entry::Failure(text));
                return Output::Shown;
            }

            Incoming::Done(stop) => {
                self.run = None;
                let outcome = self.conversation.done(stop);
                return Output::Done(outcome);
            }
        };

        match event {
            Event::Update(update) => {
                if let SessionUpdate::SessionInfoUpdate(info) = &*update {
                    if let MaybeUndefined::Value(title) = &info.title {
                        self.title = title.clone();
                    }
                    if let MaybeUndefined::Value(_) = &info.updated_at {
                        self.stored = true;
                        self.save_failed = false;
                    }
                    return Output::Shown;
                }

                let ended = self.conversation.update(&update);
                if let Some(run) = self.run.as_mut().filter(|_| ended) {
                    run.step_started = Instant::now();
                }
                Output::Shown
            }

            Event::Notice(notice) => {
                self.save_failed |= notice.unsaved;
                self.conversation.notice(&notice);
                Output::Shown
            }

            Event::Queued(asked) => Output::Queued(asked),

            Event::Background => Output::Background,

            Event::Programs(programs) => {
                let mut listed = Vec::new();
                for mut program in programs {
                    let known = self.programs.iter().find(|known| known.pid == program.pid);
                    if let Some(known) = known {
                        program.since = known.since;
                    }
                    listed.push(program);
                }
                self.programs = listed;
                Output::Shown
            }

            Event::Mcp(servers) => {
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

                    let text = mcp_line(server);
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
        }
    }
}

/// How a picked path goes in the input, open after a folder for the path to go on
pub(crate) fn mention(path: &str) -> String {
    let quoted = path.contains(char::is_whitespace);
    match (quoted, path.ends_with('/')) {
        (false, false) => format!("@{path} "),
        (false, true) => format!("@{path}"),
        (true, false) => format!("@\"{path}\" "),
        (true, true) => format!("@\"{path}"),
    }
}

/// Returns the character count including the @, and the path typed so far
pub(crate) fn typing_mention(before: &str) -> Option<(usize, &str)> {
    let mut ats = before.rmatch_indices('@').map(|(at, _)| at);
    let at = ats.find(|&at| opens_mention(&before[..at]))?;

    let rest = &before[at + 1..];
    let path = match rest.strip_prefix('"') {
        Some(quoted) if quoted.contains('"') => return None,
        Some(quoted) => quoted,
        None if rest.contains(char::is_whitespace) => return None,
        None => rest,
    };

    let count = before[at..].chars().count();
    Some((count, path))
}

fn opens_mention(before: &str) -> bool {
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

pub(crate) fn image_placeholder(number: usize) -> String {
    format!("{IMAGE_PREFIX}{number}]")
}

/// The numbers `image_placeholder` wrote in `text`, each once and in order
fn named_images(text: &str) -> Vec<usize> {
    let mut named = Vec::new();
    for (at, _) in text.match_indices(IMAGE_PREFIX) {
        let rest = &text[at + IMAGE_PREFIX.len()..];
        let Some((digits, _)) = rest.split_once(']') else {
            continue;
        };
        let Ok(number) = digits.parse::<usize>() else {
            continue;
        };
        // `[image #+1]` and `[image #01]` parse too
        if number.to_string() == digits && !named.contains(&number) {
            named.push(number);
        }
    }
    named
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
    use image::RgbaImage;
    use std::io::Cursor;

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

    /// A new image takes a number past those the conversation names, as a resumed one names
    /// images of an earlier run
    #[test]
    fn images_numbered() {
        let agent = fake();
        let mut session = Session::start(&agent, None, None, false).expect("start");
        let mut png = Cursor::new(Vec::new());
        let pixels = RgbaImage::new(2, 2);
        pixels.write_to(&mut png, ImageFormat::Png).expect("png");
        let png = png.into_inner();

        session.conversation.task("Explain [image #4] to the café");
        assert_eq!(session.paste_image(png.clone()), Ok(5));
        assert_eq!(session.paste_image(png), Ok(6));
        let refused = session.paste_image("naïve".as_bytes().to_vec());
        assert_eq!(refused, Err("image: not PNG, JPEG, GIF or WebP".to_owned()));
    }
}
