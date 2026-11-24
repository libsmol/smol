use crate::config::Skill;
use crate::history::MAX_TITLE_CHARS;
use crate::model::{
    ChatMessage, Finish, Finished, MediaKind, Model, ModelError, Purpose, StreamDelta,
};
use crate::prompt;
use crate::steps::{
    bytes, estimate, AgentMemory, AgentStep, Call, Content, MemoryStep, StepError, SummaryStep,
    TaskStep, Usage,
};
use crate::tools::attach::{attach, Mention};
use crate::tools::memories::Digest;
use crate::utils::Interrupt;
use crate::worker::host::{Added, Worker};
use crate::worker::workplace::Workplace;
use crate::{Error, Result};
use reaper::wire::Program;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::Notify;
use tokio::{task, time};
use tracing::{info, warn};

mod compact;
mod step;

const MAX_TITLING_CHARS: usize = 2000;
const MAX_RETRIES: u32 = 3;
const RETRY_WAIT: Duration = match cfg!(test) {
    true => Duration::from_millis(10),
    false => Duration::from_secs(2),
};

/// Only what happens live in a turn, as the display replays saved steps itself
#[derive(Debug, Clone, Copy)]
pub(crate) enum Event<'a> {
    /// What arrived of the reasoning
    Thought(&'a str),
    /// What arrived of the text
    Text(&'a str),
    /// Code streamed for the call at this index, once it isn't blank
    Code(usize, Streamed<'a>),
    /// A call with no readable code, shown as soon as it arrives
    Unread(usize, &'a Call),
    Run(usize),
    /// Clipped output so far of the call at this index
    Output(usize, &'a Call, &'a str),
    /// A call ended, also one with no readable code that never ran
    RunEnd(usize, &'a Call),
    Restarted,
    Retry {
        reason: &'a str,
        wait: Duration,
    },
    StepEnd(&'a AgentStep),
    /// A step whose request the model failed, which memory never holds
    StepFailed(&'a AgentStep),
    Compact,
    CompactEnd(Result<&'a SummaryStep, &'a Failure>),
    /// The programs the code left running that still run
    Programs(&'a [Program]),
}

/// What arrived since the last event for it, and all of it so far in this call
#[derive(Debug, Clone, Copy)]
pub(crate) struct Streamed<'a> {
    pub(crate) added: &'a str,
    pub(crate) whole: &'a str,
}

pub(crate) trait Sink: Send {
    fn event(&mut self, event: Event);

    /// Must be called for every added step or an exit midway loses steps
    fn saved(&mut self, memory: &AgentMemory);

    /// What the user queued while the turn ran, which is already taken off the client's queue
    fn queued(&mut self) -> Asking<'_>;
}

pub(crate) type Asking<'a> = Pin<Box<dyn Future<Output = Vec<Task>> + Send + 'a>>;

/// What a step's log line ends with, with - for tokens the provider didn't give
fn step_counts(step: &AgentStep) -> String {
    let calls = step.calls.len();
    let seconds = step.usage.duration;
    let tokens = match step.usage.tokens {
        Some(tokens) => format!("{}/{}", tokens.input_tokens, tokens.output_tokens),
        None => "-".to_owned(),
    };
    format!("[calls={calls}, tokens={tokens}, elapsed={seconds:.1}s]")
}

pub(crate) struct Task {
    pub(crate) text: String,
    pub(crate) mentions: Vec<Mention>,
}

pub(crate) struct Agent {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) brief: Brief,
    pub(crate) interrupt: Interrupt,
}

/// What the session learned when it opened, which the system prompt shows
#[derive(Default)]
pub(crate) struct Brief {
    pub(crate) instructions: Vec<String>,
    pub(crate) skills: Vec<Skill>,
    pub(crate) digest: Digest,
    pub(crate) images_shown: bool,
    /// The client declared `_smol/background`, so programs that end after an answer get a turn
    pub(crate) background: bool,
}

struct Turn<'a> {
    agent: &'a Agent,
    worker: &'a mut Worker,
    servers: &'a [&'a Added],
    memory: &'a mut AgentMemory,
    system: String,
    sink: &'a mut dyn Sink,
}

impl Agent {
    /// `servers` are the MCP servers loaded into the worker and stay fixed for the turn,
    /// and without a task the turn takes up the programs that ended, which the last step names
    pub(crate) async fn run(
        &self,
        worker: &mut Worker,
        servers: &[&Added],
        memory: &mut AgentMemory,
        task: Option<Task>,
        sink: &mut dyn Sink,
    ) -> Result<Option<String>> {
        let takes = |kind| self.model.takes(kind);
        let workplace = worker.workplace();
        let system = prompt::system(
            worker.functions(),
            servers,
            &self.brief,
            &workplace.cwd,
            workplace.writable(),
            &takes,
        );

        let mut turn = Turn {
            agent: self,
            worker,
            servers,
            memory,
            system,
            sink,
        };
        turn.run(task).await
    }
}

impl Turn<'_> {
    async fn run(&mut self, task: Option<Task>) -> Result<Option<String>> {
        if let Some(task) = task {
            let task = attached(task, self.worker.workplace()).await;
            if task.text.trim().is_empty() && task.files.is_empty() {
                return Err(Error::Refused("empty".to_owned()));
            }

            // The task text survives compaction verbatim, so one over half the window can never fit
            let copies = &self.worker.workplace().copies;
            if let Some(window) = self.agent.model.context_window() {
                // Image and video estimates run high, so only text counts toward refusing
                let text_only = |_: MediaKind| false;
                let parts = prompt::task(&task, &text_only);
                let asked = [ChatMessage::User(parts)];
                let sent = bytes(&asked);
                let size = estimate(sent);
                if size > window / 2 {
                    discard(&task, copies);
                    let message = format!(
                        "too large: about {size} tokens, over half of the {window}-token context"
                    );
                    return Err(Error::Refused(message));
                }
            }

            // A task whose turn hit a model error stays in the record, so resending it replaces it
            if let Some(old) = self.memory.withdraw(&task.text) {
                discard(&old, copies);
            }

            self.memory.steps.push(MemoryStep::Task(task));
            self.sink.saved(self.memory);
        }

        loop {
            self.compact().await?;

            // Pushed after compaction or it would be summarized away
            if self.worker.take_restarted() {
                self.memory.steps.push(MemoryStep::Restarted);
                warn!("python: restarted");
                self.sink.event(Event::Restarted);
                self.sink.saved(self.memory);
            }

            let step = self.step().await?;
            let answer = step.answer().map(str::to_owned);
            self.memory.steps.push(MemoryStep::Action(step));

            let model = &self.agent.model;
            let takes = |kind| model.takes(kind);
            let window = model.context_window();
            let used = self.memory.tokens(&self.system, &takes, window);
            let Some(MemoryStep::Action(step)) = self.memory.steps.last_mut() else {
                panic!("action");
            };
            step.used = Some(used);
            // Code errors contain task content and are not logged
            let counted = step_counts(step);
            match &step.error {
                None => info!("step: done {counted}"),
                Some(StepError::Interrupted) => info!("step: interrupted {counted}"),
                Some(error) => warn!("step: failed, {error} {counted}"),
            }
            self.sink.event(Event::StepEnd(step));
            self.sink.saved(self.memory);

            // An interrupt that arrives after the answer still ends the turn as interrupted
            if self.agent.interrupt.triggered() {
                return Ok(None);
            }
            // Programs that end after an answer go to the session, which takes them up between turns
            if answer.is_some() {
                return Ok(answer);
            }

            let done = self.worker.ended().await;
            let programs = self.worker.running();
            self.sink.event(Event::Programs(&programs));
            let updates = self.queued().await;
            let received = !updates.is_empty() || done.is_some();
            for update in updates {
                let update = attached(update, self.worker.workplace()).await;
                self.memory.steps.push(MemoryStep::Update(update));
            }
            if let Some(done) = done {
                self.memory.steps.push(MemoryStep::Done(done));
            }
            if received {
                self.sink.saved(self.memory);
            }
        }
    }

    /// Stays interruptible when the client never replies
    async fn queued(&mut self) -> Vec<Task> {
        let asking = self.sink.queued();
        select! {
            // The client already dequeued these, so they win over a cancel arriving with them
            biased;
            updates = asking => updates,
            () = self.agent.interrupt.wait() => Vec::new(),
        }
    }
}

/// Reading files and copying images and videos stays off the async runtime
async fn attached(task: Task, workplace: &Workplace) -> TaskStep {
    let cwd = workplace.cwd.clone();
    let copies = workplace.copies.clone();
    let mentions = task.mentions;
    let attaching = move || {
        let attached = mentions
            .iter()
            .map(|mention| attach(mention, &cwd, &copies));
        attached.collect()
    };
    let files = task::spawn_blocking(attaching).await.expect("attach");

    TaskStep {
        text: task.text,
        files,
    }
}

fn discard(task: &TaskStep, copies: &Path) {
    for file in &task.files {
        if let Content::Media { media } = &file.content {
            let copy = copies.join(&media.file);
            fs::remove_file(copy).ok();
        }
    }
}

/// Reports a retryable failure to `sink` and waits, returning true early on interrupt
async fn retry(
    tries: &mut u32,
    failure: &Failure,
    interrupt: &Interrupt,
    sink: &mut dyn Sink,
) -> bool {
    let (error, wait) = match failure {
        Failure::Model(error) if error.transient && *tries < MAX_RETRIES => {
            let backoff = RETRY_WAIT * 2u32.pow(*tries);
            let wait = error.retry_after.unwrap_or(backoff);
            (error, wait)
        }
        _ => return false,
    };

    let seconds = wait.as_secs_f64();
    warn!("model: retrying in {seconds:.1}s, {error}");
    let reason = failure.to_string();
    sink.event(Event::Retry {
        reason: &reason,
        wait,
    });
    select! {
        () = time::sleep(wait) => {}
        () = interrupt.wait() => {}
    }
    *tries += 1;

    true
}

/// Why a request to the model brought no reply to use
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Failure {
    #[error("interrupted")]
    Interrupted,
    #[error("model: {0}")]
    Model(ModelError),
}

/// Once `enough` fires the stream is not read to the end, which leaves usage unknown
async fn generate(
    model: &dyn Model,
    interrupt: &Interrupt,
    enough: &Notify,
    conversation: &[ChatMessage],
    purpose: Purpose,
    usage: &mut Usage,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) -> Result<Finish, Failure> {
    let mut first = None;
    let mut blank = true;
    let mut reasoned = false;
    let mut seen = |delta: StreamDelta| {
        first.get_or_insert_with(Instant::now);
        match delta {
            StreamDelta::Content(text) => blank &= text.trim().is_empty(),
            StreamDelta::CallCode(_) | StreamDelta::CallEnd(_) => blank = false,
            StreamDelta::Reasoning(text) => reasoned |= !text.is_empty(),
            StreamDelta::Sealed(sealed) => {
                let last = sealed.items.last();
                reasoned |= last.is_some_and(|item| item["type"] == "reasoning");
            }
        }
        on_delta(delta);
    };
    let generating = model.generate(conversation, purpose, &mut seen);
    // Cancel wins and drops any output the model still has
    let generated = select! {
        biased;
        () = interrupt.wait() => None,
        () = enough.notified() => {
            let cut = Finished {
                finish: Finish::Stop,
                usage: None,
            };
            Some(Ok(cut))
        }
        generated = generating => Some(generated),
    };

    usage.decode_duration = first.map(|at: Instant| at.elapsed().as_secs_f64());
    let Some(generated) = generated else {
        return Err(Failure::Interrupted);
    };
    if let Ok(finished) = &generated {
        usage.tokens = finished.usage;
    }

    if interrupt.triggered() {
        return Err(Failure::Interrupted);
    }

    let finished = generated.map_err(Failure::Model)?;
    let error = match finished.finish {
        Finish::Stop if blank && reasoned => {
            ModelError::transient("stopped after reasoning without replying")
        }
        Finish::Stop if blank => ModelError::transient("empty reply"),
        Finish::Length if blank => ModelError::new("hit the output limit before replying"),
        Finish::Filtered => ModelError::new("stopped by provider (content filter)"),
        // Such as an overloaded server
        Finish::Other(reason) => {
            let message = format!("stopped by provider ({reason})");
            ModelError::transient(message)
        }
        finish => return Ok(finish),
    };
    Err(Failure::Model(error))
}

async fn text_reply(
    model: &dyn Model,
    interrupt: &Interrupt,
    request: &[ChatMessage],
    purpose: Purpose,
) -> (Result<String, Failure>, Usage) {
    let started = Instant::now();
    let mut usage = Usage::default();
    let mut text = String::new();

    let mut on_delta = |delta: StreamDelta| {
        if let StreamDelta::Content(part) = delta {
            text.push_str(part);
        }
    };
    let never = Notify::new();
    let generating = generate(
        model,
        interrupt,
        &never,
        request,
        purpose,
        &mut usage,
        &mut on_delta,
    );
    let generated = generating.await;
    usage.duration = started.elapsed().as_secs_f64();

    let replied = generated.map(|_| text);
    (replied, usage)
}

pub(crate) async fn title_of(
    model: Arc<dyn Model>,
    task: String,
    attached: Vec<String>,
) -> Option<String> {
    let message = prompt::first_message(&task, &attached, MAX_TITLING_CHARS);
    let request = [
        ChatMessage::System(prompt::titler()),
        ChatMessage::user(message),
    ];

    let interrupt = Interrupt::default();
    let replying = text_reply(&*model, &interrupt, &request, Purpose::Title);
    let (replied, _) = replying.await;
    let text = replied.ok()?;

    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let marks = ['"', '\'', '“', '”', '‘', '’', '「', '」', '*', '#', '`'];
    let endings = [
        '。', '.', '!', '！', '?', '？', ',', '，', ':', '：', ';', '；',
    ];
    let lead = |c: char| c.is_whitespace() || marks.contains(&c);
    let tail = |c: char| lead(c) || endings.contains(&c);
    let title = line.trim_start_matches(lead).trim_end_matches(tail);
    if title.is_empty() {
        return None;
    }
    Some(title.chars().take(MAX_TITLE_CHARS).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::agent::{Agent, Asking, Brief, Event, Sink, Task, Turn};
    use crate::config::{Access, Kind};
    use crate::model::{
        ChatMessage, Finish, Finished, Generating, MediaKind, Model, ModelError, Purpose, Sealed,
        StreamDelta, TokenUsage, Unread,
    };
    use crate::prompt::RESTARTED;
    use crate::steps::{AgentMemory, MemoryStep};
    use crate::tests::{content, temp_dir, Temp};
    use crate::tools::attach::tests::mentioned;
    use crate::tools::memories::Memories;
    use crate::utils::{block_on, Interrupt};
    use crate::worker::host::Worker;
    use crate::worker::workplace::tests::opened;
    use crate::Result;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::future;
    use std::mem;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::task;

    /// Tool name and raw arguments of a call with no readable code
    pub(crate) type Unreadable = (&'static str, &'static str);

    #[derive(Debug, Clone)]
    pub(crate) struct Script {
        pub(crate) reasoning: Option<String>,
        /// Output items of provider `mock` through Responses, which come before the content
        pub(crate) sealed: Vec<Value>,
        pub(crate) content: String,
        pub(crate) calls: Vec<Result<String, Unreadable>>,
        pub(crate) usage: Option<TokenUsage>,
        pub(crate) finish: Finish,
    }

    /// Answers 42 forever once the replies run out
    pub(crate) struct ScriptedModel(Mutex<VecDeque<Result<Script, ModelError>>>);

    impl ScriptedModel {
        pub(crate) fn new(replies: Vec<Result<Script, ModelError>>) -> Self {
            Self(Mutex::new(replies.into()))
        }
    }

    impl Model for ScriptedModel {
        fn generate<'a>(
            &'a self,
            _: &'a [ChatMessage],
            _: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            let next = self.0.lock().expect("replies lock").pop_front();
            Box::pin(async move {
                let answer = || Ok(reply("42"));
                let script = next.unwrap_or_else(answer)?;

                if let Some(reasoning) = &script.reasoning {
                    on_delta(StreamDelta::Reasoning(reasoning));
                }
                let mut sealed = Sealed {
                    provider: "mock".to_owned(),
                    kind: Kind::Responses,
                    items: Vec::new(),
                    ends: Vec::new(),
                };
                for item in &script.sealed {
                    sealed.items.push(item.clone());
                    sealed.ends.push(0);
                    on_delta(StreamDelta::Sealed(&sealed));
                }
                // Yields after every line so cancels and early cutoffs can land
                for line in script.content.split_inclusive('\n') {
                    let delta = StreamDelta::Content(line);
                    on_delta(delta);
                    task::yield_now().await;
                }
                let last = script.calls.len().saturating_sub(1);
                for (index, call) in script.calls.iter().enumerate() {
                    let code = call.as_deref().unwrap_or_default();
                    for line in code.split_inclusive('\n') {
                        let delta = StreamDelta::CallCode(line);
                        on_delta(delta);
                        task::yield_now().await;
                    }
                    // Like a real stream, each call ends as the next starts
                    // and a cut-off last one never does
                    if script.finish == Finish::Length && index == last {
                        continue;
                    }
                    let ended = match call {
                        Ok(_) => Ok(()),
                        Err((name, arguments)) => Err(Unread { name, arguments }),
                    };
                    let delta = StreamDelta::CallEnd(ended);
                    on_delta(delta);
                }

                Ok(Finished {
                    finish: script.finish,
                    usage: script.usage,
                })
            })
        }
    }

    type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

    /// Records every request
    struct Recording {
        replies: ScriptedModel,
        window: Option<u64>,
        requests: Requests,
    }

    impl Model for Recording {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            purpose: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            let request = conversation.to_vec();
            self.requests.lock().expect("requests").push(request);
            self.replies.generate(conversation, purpose, on_delta)
        }

        fn context_window(&self) -> Option<u64> {
            self.window
        }

        fn takes(&self, _kind: MediaKind) -> bool {
            true
        }
    }

    pub(crate) fn reply(content: &str) -> Script {
        Script {
            reasoning: None,
            sealed: Vec::new(),
            content: content.to_owned(),
            calls: Vec::new(),
            usage: None,
            finish: Finish::Stop,
        }
    }

    pub(crate) fn called(code: &str) -> Script {
        Script {
            calls: vec![Ok(code.to_owned())],
            ..reply("")
        }
    }

    pub(crate) struct Probe {
        events: Box<dyn FnMut(Event) + Send>,
        /// Step count at each save
        pub(crate) saves: Vec<usize>,
        /// What the user queues, all taken at the first ask
        pub(crate) queued: Vec<Task>,
    }

    impl Sink for Probe {
        fn event(&mut self, event: Event) {
            (self.events)(event);
        }

        fn saved(&mut self, memory: &AgentMemory) {
            self.saves.push(memory.steps.len());
        }

        fn queued(&mut self) -> Asking<'_> {
            let taken = mem::take(&mut self.queued);
            Box::pin(future::ready(taken))
        }
    }

    pub(crate) struct Tested {
        pub(crate) agent: Agent,
        pub(crate) worker: Worker,
        pub(crate) probe: Probe,
    }

    impl Tested {
        pub(crate) fn turn(
            &mut self,
            memory: &mut AgentMemory,
            task: &str,
        ) -> Result<Option<String>> {
            self.turn_attached(memory, task, &[])
        }

        pub(crate) fn turn_attached(
            &mut self,
            memory: &mut AgentMemory,
            task: &str,
            files: &[PathBuf],
        ) -> Result<Option<String>> {
            let mentions = files.iter().map(|path| mentioned(path)).collect();
            let task = Task {
                text: task.to_owned(),
                mentions,
            };
            let task = Some(task);
            let probe = &mut self.probe;
            let running = self.agent.run(&mut self.worker, &[], memory, task, probe);
            block_on(running)
        }

        /// Compacts only, with an empty system prompt
        pub(crate) fn compact(&mut self, memory: &mut AgentMemory) -> Result<()> {
            self.compact_under(memory, "")
        }

        pub(crate) fn compact_under(
            &mut self,
            memory: &mut AgentMemory,
            system: &str,
        ) -> Result<()> {
            let mut turn = Turn {
                agent: &self.agent,
                worker: &mut self.worker,
                servers: &[],
                memory,
                system: system.to_owned(),
                sink: &mut self.probe,
            };
            let compacting = turn.compact();
            block_on(compacting)
        }
    }

    pub(crate) fn scripted(
        model: impl Model + 'static,
        events: Box<dyn FnMut(Event) + Send>,
    ) -> Temp<Tested> {
        let interrupt = Interrupt::default();
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dir = temp_dir("session");
        let workplace = opened(&cwd, &dir, &Access::default());
        let memories = Memories::new(&dir, &cwd);
        let starting = Worker::new(&interrupt, workplace, memories, false);
        let worker = block_on(starting).expect("worker");

        let agent = Agent {
            model: Arc::new(model),
            brief: Brief::default(),
            interrupt,
        };
        let probe = Probe {
            events,
            saves: Vec::new(),
            queued: Vec::new(),
        };
        let value = Tested {
            agent,
            worker,
            probe,
        };
        Temp { value, dir }
    }

    pub(crate) struct Run {
        pub(crate) agent: Temp<Tested>,
        pub(crate) requests: Requests,
    }

    pub(crate) fn recorded(
        replies: Vec<Result<Script, ModelError>>,
        window: Option<u64>,
        events: Box<dyn FnMut(Event) + Send>,
    ) -> Run {
        let requests = Requests::default();
        let model = Recording {
            replies: ScriptedModel::new(replies),
            window,
            requests: Arc::clone(&requests),
        };
        let agent = scripted(model, events);
        Run { agent, requests }
    }

    #[test]
    fn restart_told_once() {
        let replies = vec![Ok(called("print('reading the orders again')"))];
        let mut run = recorded(replies, None, Box::new(|_| {}));
        let mut ignore = |_: &str| {};
        let code = "import os\nos._exit(1)";
        let exiting = run.agent.worker.evaluate(code, &[], &mut ignore);
        block_on(exiting).expect("evaluate");

        let mut memory = AgentMemory::default();
        run.agent.turn(&mut memory, "Read the orders").expect("run");
        // Saved as soon as the restart notice is added
        assert_eq!(run.agent.probe.saves, [1, 2, 3, 4]);

        let requests = run.requests.lock().expect("requests");
        let first: Vec<String> = requests[0][1..].iter().map(content).collect();
        assert_eq!(first, ["Read the orders", RESTARTED]);
        let second: Vec<String> = requests[1][1..].iter().map(content).collect();
        let expected = [
            "Read the orders",
            RESTARTED,
            "eval:\nprint('reading the orders again')",
            "reading the orders again",
        ];
        assert_eq!(second, expected);
    }

    /// What the user queued while code ran follows that step and reaches the next request
    #[test]
    fn updates_follow_step() {
        let replies = vec![Ok(called("print('2 cafés')")), Ok(reply("Done"))];
        let mut run = recorded(replies, None, Box::new(|_| {}));
        let update = Task {
            text: "Also count the naïve ones".to_owned(),
            mentions: Vec::new(),
        };
        run.agent.probe.queued.push(update);

        let mut memory = AgentMemory::default();
        let answer = run.agent.turn(&mut memory, "Count the cafés").expect("run");
        assert_eq!(answer.as_deref(), Some("Done"));
        let MemoryStep::Update(taken) = &memory.steps[2] else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(taken.text, "Also count the naïve ones");

        let requests = run.requests.lock().expect("requests");
        let second: Vec<String> = requests[1][1..].iter().map(content).collect();
        let expected = [
            "Count the cafés",
            "eval:\nprint('2 cafés')",
            "2 cafés",
            "Also count the naïve ones",
        ];
        assert_eq!(second, expected);
    }
}
