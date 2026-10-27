use crate::config::Skill;
use crate::history::MAX_TITLE_CHARS;
use crate::memory::{
    bytes, estimate, AgentMemory, AgentStep, Call, MemoryStep, StepError, SummaryStep, TaskStep,
    Usage,
};
use crate::model::{
    ChatMessage, Finish, Finished, MediaKind, Model, ModelError, Purpose, StreamDelta,
};
use crate::prompt;
use crate::tools::attach::{attach, Mention};
use crate::utils::Interrupt;
use crate::worker::host::{Added, Worker};
use crate::{Error, Result};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::Notify;
use tokio::{task, time};

mod compact;
mod step;

const VIEW: &str = "view";
const MAX_NAMING_CHARS: usize = 2000;
const MAX_RETRIES: u32 = 3;
const RETRY_WAIT: Duration = match cfg!(test) {
    true => Duration::from_millis(10),
    false => Duration::from_secs(2),
};

/// Only what happens live in a turn, as the display replays saved steps itself
#[derive(Debug, Clone, Copy)]
pub(crate) enum Event<'a> {
    Thought(&'a str),
    Text(&'a str),
    /// Code streamed for the call at this index
    Code(usize, &'a str),
    /// A call with no readable code, shown as soon as it arrives
    Unread(usize, &'a Call),
    Execute(usize),
    /// Clipped output of the running code so far
    Output(&'a str),
    Ran(usize, &'a Call),
    Restarted,
    Retry {
        reason: &'a str,
        wait: Duration,
    },
    StepEnd(&'a AgentStep),
    Compact,
    CompactEnd(Result<&'a SummaryStep, &'a StepError>),
}

pub(crate) type Asking<'a> = Pin<Box<dyn Future<Output = Vec<Task>> + Send + 'a>>;

pub(crate) trait Sink: Send {
    fn event(&mut self, event: Event);

    /// Must be called for every added step or an exit midway loses steps
    fn saved(&mut self, memory: &AgentMemory);

    /// The returned messages are already taken off the client's queue
    fn queued(&mut self) -> Asking<'_>;
}

pub(crate) struct Task {
    pub(crate) text: String,
    pub(crate) mentions: Vec<Mention>,
}

pub(crate) struct Agent {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) instructions: Vec<String>,
    pub(crate) skills: Vec<Skill>,
    pub(crate) cwd: PathBuf,
    pub(crate) copies: PathBuf,
    pub(crate) interrupt: Interrupt,
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
    /// `servers` are the MCP servers loaded into the worker and stay fixed for the turn
    pub(crate) async fn run(
        &self,
        worker: &mut Worker,
        servers: &[&Added],
        memory: &mut AgentMemory,
        task: Task,
        sink: &mut dyn Sink,
    ) -> Result<Option<String>> {
        let seeing = self.model.takes(MediaKind::Image) || self.model.takes(MediaKind::Video);
        let mut functions = Vec::new();
        for function in worker.functions() {
            if seeing || function.name != VIEW {
                functions.push(function);
            }
        }
        let system = prompt::system(
            &functions,
            servers,
            &self.skills,
            &self.cwd,
            worker.sandboxed(),
            &self.instructions,
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

    /// Reading files and copying images and videos stays off the async runtime
    async fn attached(&self, task: Task) -> TaskStep {
        let cwd = self.cwd.clone();
        let copies = self.copies.clone();
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
}

impl Turn<'_> {
    async fn run(&mut self, task: Task) -> Result<Option<String>> {
        let task = self.agent.attached(task).await;

        // The task text survives compaction verbatim, so a task over half the window can never fit
        let copies = &self.agent.copies;
        if let Some(window) = self.agent.model.context_window() {
            // Image and video estimates run high, so only text counts toward refusing
            let text_only = |_: MediaKind| false;
            let parts = prompt::task(&task);
            let asked = [ChatMessage::User(parts)];
            let sent = bytes(&asked, &text_only);
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
        let steps = &mut self.memory.steps;
        let last = steps
            .iter()
            .rposition(|step| !matches!(step, MemoryStep::Restarted | MemoryStep::Summary(_)));
        if let Some(at) = last {
            if let MemoryStep::Task(old) = &steps[at] {
                if old.text == task.text {
                    discard(old, copies);
                    steps.remove(at);
                    // Summaries after it count it among their `kept` steps
                    for step in &mut steps[at..] {
                        if let MemoryStep::Summary(summary) = step {
                            summary.kept = summary.kept.saturating_sub(1);
                        }
                    }
                }
            }
        }

        self.memory.steps.push(MemoryStep::Task(task));
        self.sink.saved(self.memory);

        loop {
            self.compact().await?;

            // Pushed after compaction or it would be summarized away
            if self.worker.restarted() {
                self.memory.steps.push(MemoryStep::Restarted);
                self.sink.event(Event::Restarted);
                self.sink.saved(self.memory);
            }

            let step = self.step().await;
            if let Some(StepError::Model(error)) = &step.error {
                self.sink.event(Event::StepEnd(&step));
                return Err(error.clone().into());
            }

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
            self.sink.event(Event::StepEnd(step));
            self.sink.saved(self.memory);

            // An interrupt that arrives after the answer still ends the turn as interrupted
            if self.agent.interrupt.triggered() {
                return Ok(None);
            }
            if answer.is_some() {
                return Ok(answer);
            }

            let updates = self.queued().await;
            let received = !updates.is_empty();
            for update in updates {
                let update = self.agent.attached(update).await;
                self.memory.steps.push(MemoryStep::Update(update));
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

fn discard(task: &TaskStep, copies: &Path) {
    for file in &task.files {
        if let Some(media) = &file.media {
            let copy = copies.join(&media.file);
            fs::remove_file(copy).ok();
        }
    }
}

/// Reports a retryable error to `sink` and waits, returning true early on interrupt
async fn retry(
    tries: &mut u32,
    error: &StepError,
    interrupt: &Interrupt,
    sink: &mut dyn Sink,
) -> bool {
    let wait = match error {
        StepError::Model(model) if model.transient && *tries < MAX_RETRIES => {
            let backoff = RETRY_WAIT * 2u32.pow(*tries);
            model.retry_after.unwrap_or(backoff)
        }
        _ => return false,
    };

    let reason = error.to_string();
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

/// Once `enough` fires the stream is not read to the end, which leaves usage unknown
async fn generate(
    model: &dyn Model,
    interrupt: &Interrupt,
    enough: &Notify,
    conversation: &[ChatMessage],
    purpose: Purpose,
    usage: &mut Usage,
    on_delta: &mut (dyn FnMut(StreamDelta) + Send),
) -> Result<Finish, StepError> {
    let mut first = None;
    let mut blank = true;
    let mut reasoned = false;
    let mut seen = |delta: StreamDelta| {
        first.get_or_insert_with(Instant::now);
        match delta {
            StreamDelta::Content(text) => blank &= text.trim().is_empty(),
            StreamDelta::CallCode(_) | StreamDelta::CallEnd(_) => blank = false,
            StreamDelta::Reasoning(text) => reasoned |= !text.is_empty(),
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
        return Err(StepError::Interrupted);
    };
    if let Ok(finished) = &generated {
        usage.tokens = finished.usage;
    }

    if interrupt.triggered() {
        return Err(StepError::Interrupted);
    }

    let finished = generated.map_err(StepError::Model)?;
    let error = match finished.finish {
        Finish::Stop if blank && reasoned => {
            ModelError::transient("stopped after reasoning without replying")
        }
        Finish::Stop if blank => ModelError::transient("empty reply"),
        Finish::Length if blank => ModelError::new("hit the output limit before replying"),
        Finish::Other(reason) => {
            let message = format!("stopped by provider ({reason})");
            ModelError::new(message)
        }
        finish => return Ok(finish),
    };
    Err(StepError::Model(error))
}

async fn text_reply(
    model: &dyn Model,
    interrupt: &Interrupt,
    request: &[ChatMessage],
    purpose: Purpose,
) -> (Result<String, StepError>, Usage) {
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

pub(crate) async fn title_of(model: Arc<dyn Model>, task: String) -> Option<String> {
    let task: String = task.chars().take(MAX_NAMING_CHARS).collect();
    let request = [
        ChatMessage::System(prompt::NAMER.to_owned()),
        ChatMessage::user(task),
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
    use crate::agent::{title_of, Agent, Asking, Event, Sink, Task, Turn, RETRY_WAIT};
    use crate::history::MAX_TITLE_CHARS;
    use crate::memory::{AgentMemory, MemoryStep};
    use crate::model::{
        ChatMessage, Finish, Finished, Generating, MediaKind, Model, ModelError, Purpose,
        StreamDelta, TokenUsage, Unread,
    };
    use crate::prompt::RESTARTED;
    use crate::tests::{content, temp_dir, write_file, Temp};
    use crate::tools::attach::tests::mentioned;
    use crate::utils::{block_on, Interrupt};
    use crate::worker::host::Worker;
    use crate::worker::sandbox::tests::opened;
    use crate::Result;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::task;

    /// Tool name and raw arguments of a call with no readable code
    pub(crate) type Unreadable = (&'static str, &'static str);

    #[derive(Debug, Clone)]
    pub(crate) struct Script {
        pub(crate) reasoning: Option<String>,
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

    /// Records every request and cancels `interrupt` on one whose system prompt is `trigger`
    struct Recording {
        replies: ScriptedModel,
        window: Option<u64>,
        trigger: &'static str,
        requests: Requests,
        interrupt: Arc<Mutex<Option<Interrupt>>>,
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

            let interrupt = self.interrupt.lock().expect("interrupt").clone();
            let triggered = content(&conversation[0]) == self.trigger;
            if let Some(interrupt) = interrupt.filter(|_| triggered) {
                interrupt.cancel(u64::MAX);
            }

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
        pub(crate) updates: Box<dyn FnMut() -> Vec<Task> + Send>,
        /// Step count at each save
        pub(crate) saves: Vec<usize>,
    }

    impl Sink for Probe {
        fn event(&mut self, event: Event) {
            (self.events)(event);
        }

        fn saved(&mut self, memory: &AgentMemory) {
            self.saves.push(memory.steps.len());
        }

        fn queued(&mut self) -> Asking<'_> {
            let updates = (self.updates)();
            Box::pin(async move { updates })
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
            let probe = &mut self.probe;
            let running = self.agent.run(&mut self.worker, &[], memory, task, probe);
            block_on(running)
        }

        /// Compacts only, with an empty system prompt
        pub(crate) fn compact(&mut self, memory: &mut AgentMemory) -> Result<()> {
            let mut turn = Turn {
                agent: &self.agent,
                worker: &mut self.worker,
                servers: &[],
                memory,
                system: String::new(),
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
        let copies = dir.join("copies");
        let sandbox = opened(&cwd, &copies);
        let starting = Worker::new(&interrupt, sandbox, false);
        let worker = block_on(starting).expect("worker");

        let agent = Agent {
            model: Arc::new(model),
            instructions: Vec::new(),
            skills: Vec::new(),
            cwd,
            copies,
            interrupt,
        };
        let probe = Probe {
            events,
            updates: Box::new(Vec::new),
            saves: Vec::new(),
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
        pub(crate) interrupt: Arc<Mutex<Option<Interrupt>>>,
    }

    pub(crate) fn recorded(
        replies: Vec<Result<Script, ModelError>>,
        window: Option<u64>,
        trigger: &'static str,
        events: Box<dyn FnMut(Event) + Send>,
    ) -> Run {
        let requests = Requests::default();
        let interrupt = Arc::<Mutex<Option<Interrupt>>>::default();
        let model = Recording {
            replies: ScriptedModel::new(replies),
            window,
            trigger,
            requests: Arc::clone(&requests),
            interrupt: Arc::clone(&interrupt),
        };
        let agent = scripted(model, events);

        Run {
            agent,
            requests,
            interrupt,
        }
    }

    #[test]
    fn title_cleaned() {
        let titled = |answer: &str| {
            let replies = vec![Ok(reply(answer))];
            let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(replies));
            let task = "Write me a snake game".to_owned();
            let naming = title_of(model, task);
            block_on(naming)
        };
        let title = titled("  **“Snake game”**。\nAn extra line");
        assert_eq!(title.as_deref(), Some("Snake game"));
        let heading = titled("\n# .env file setup\n");
        assert_eq!(heading.as_deref(), Some(".env file setup"));
        assert_eq!(titled("「」。"), None);

        let long = "café".repeat(20);
        let cut = titled(&long).expect("title");
        assert_eq!(cut.chars().count(), MAX_TITLE_CHARS);

        let refused = vec![Err(ModelError::new("refused"))];
        let failed: Arc<dyn Model> = Arc::new(ScriptedModel::new(refused));
        let task = "task".to_owned();
        let naming = title_of(failed, task);
        assert_eq!(block_on(naming), None);
    }

    #[test]
    fn huge_task_refused() {
        let replies = vec![Ok(reply("Watched it"))];
        let mut run = recorded(replies, Some(40_000), "", Box::new(|_| {}));
        let video = run.agent.dir.join("clip.mp4");
        write_file(&video, b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2mp41");
        let files = [video];

        // Over half the window but still inside it
        let mut memory = AgentMemory::default();
        let huge = "crème brûlée ".repeat(5625);
        let error = run.agent.turn(&mut memory, &huge).expect_err("refused");
        let error = error.to_string();
        let refused = "task: too large: about 30003 tokens, over half of the 40000-token context";
        assert_eq!(error, refused);
        assert!(memory.steps.is_empty());

        run.agent
            .turn_attached(&mut memory, &huge, &files)
            .expect_err("refused");
        let copies = run.agent.agent.copies.clone();
        let left = fs::read_dir(&copies).expect("copies").count();
        assert_eq!(left, 0);

        let answer = run.agent.turn_attached(&mut memory, "Watch it", &files);
        let answer = answer.expect("run");
        assert_eq!(answer, Some("Watched it".to_owned()));
        let MemoryStep::Task(task) = &memory.steps[0] else {
            panic!("{:?}", memory.steps);
        };
        let media = task.files[0].media.as_ref().expect("media");
        assert!(copies.join(&media.file).exists());
    }

    #[test]
    fn resent_task_replaced() {
        let failed = Err(ModelError::new("http 401: wrong key"));
        let replies = vec![failed, Ok(reply("Counted them."))];
        let mut run = recorded(replies, None, "", Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let failed = run.agent.turn(&mut memory, "Count lines");
        failed.expect_err("401");
        let answer = run.agent.turn(&mut memory, "Count lines").expect("run");
        assert_eq!(answer.as_deref(), Some("Counted them."));

        let requests = run.requests.lock().expect("requests");
        let second: Vec<String> = requests[1][1..].iter().map(content).collect();
        assert_eq!(second, ["New task:\nCount lines"]);
        assert_eq!(memory.steps.len(), 2, "{:?}", memory.steps);
    }

    #[test]
    fn retry_after_waited() {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&waits);
        let on_event = Box::new(move |event: Event| {
            if let Event::Retry { wait, .. } = event {
                seen.lock().expect("waits").push(wait);
            }
        });
        let asked = Duration::from_millis(50);
        let mut limited = ModelError::transient("http 429: too many requests");
        limited.retry_after = Some(asked);
        let busy = ModelError::transient("http 503: service busy");
        let replies = vec![Err(limited), Err(busy), Ok(reply("Done"))];
        let model = ScriptedModel::new(replies);
        let mut agent = scripted(model, on_event);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Add one to x").expect("run");
        assert_eq!(answer.as_deref(), Some("Done"));
        let expected = [asked, RETRY_WAIT * 2];
        assert_eq!(*waits.lock().expect("waits"), expected);
    }

    #[test]
    fn context_counts_observation() {
        let printed = Script {
            usage: Some(TokenUsage::new(100, 20)),
            ..called("print('observed' * 300)")
        };
        let mut run = recorded(vec![Ok(printed)], None, "", Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        run.agent.turn(&mut memory, "task").expect("run");

        let MemoryStep::Action(step) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        let used = step.used.expect("used");
        let seen = AgentMemory {
            steps: memory.steps[..2].to_vec(),
        };
        let takes = |_: MediaKind| false;
        let requests = run.requests.lock().expect("requests");
        let prompt = content(&requests[0][0]);
        let expected = seen.tokens(&prompt, &takes, None);
        assert_eq!(used, expected);
        assert!(used > 120 + 300, "{used}");
    }

    #[test]
    fn restart_told_once() {
        let replies = vec![Ok(called("print('reading the orders again')"))];
        let mut run = recorded(replies, None, "", Box::new(|_| {}));
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
        assert_eq!(first, ["New task:\nRead the orders", RESTARTED]);
        let second: Vec<String> = requests[1][1..].iter().map(content).collect();
        let expected = [
            "New task:\nRead the orders",
            RESTARTED,
            "eval:\nprint('reading the orders again')",
            "reading the orders again",
        ];
        assert_eq!(second, expected);
    }
}
