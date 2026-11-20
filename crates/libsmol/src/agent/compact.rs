use crate::agent::{retry, text_reply, Event, Failure, Turn};
use crate::model::{ChatMessage, Purpose};
use crate::prompt::{self, SUMMARIZER};
use crate::steps::{
    bytes, estimate, text_bytes, MemoryStep, SummaryStep, Usage, MAX_OBSERVATION_CHARS,
};
use crate::utils::truncated;
use crate::worker::Globals;
use crate::Result;
use std::time::Instant;

/// Reserved for the next reply only, as the summary request has its own budget
const RESERVE_SHARE: f64 = 0.15;
const MAX_RESERVE_TOKENS: u64 = 16_384;
const MAX_RECENT_SHARE: f64 = 0.15;
/// The rest is left for the summary
const MAX_TRANSCRIPT_SHARE: f64 = 0.7;
const MIN_TRANSCRIPT_SHARE: f64 = 0.25;
const MIN_OBSERVATION_CHARS: usize = 1024;

impl Turn<'_> {
    /// An interrupt yields no summary and is recorded by the next step
    pub(crate) async fn compact(&mut self) -> Result<()> {
        let model = &*self.agent.model;
        let Some(window) = model.context_window() else {
            return Ok(());
        };

        let takes = |kind| model.takes(kind);
        let rank = self.agent.place.rank();
        let used = self.memory.tokens(&self.system, &takes, rank, Some(window));
        let reserve = (window as f64 * RESERVE_SHARE) as u64;
        let reserve = reserve.min(MAX_RESERVE_TOKENS);
        // Servers may refuse a request whose input and output limit together pass the window,
        // and past half of what the system prompt leaves, every step would compact again
        let output = model.max_tokens();
        let size = text_bytes(&self.system);
        let system = estimate(size);
        let left = window.saturating_sub(system);
        let limit = output.unwrap_or(0).min(left / 2);
        let reserve = reserve.max(limit);
        let interrupted = self.agent.interrupt.triggered();
        if interrupted || used < window - reserve {
            return Ok(());
        }

        let visible = self.memory.visible();

        let max_recent = (window as f64 * MAX_RECENT_SHARE) as u64;
        let mut recent = 0;
        let mut kept = 0;
        for step in visible.steps.iter().rev() {
            let messages = step.messages(&mut 0, &takes, rank);
            let sent = bytes(&messages);
            recent += estimate(sent);
            if recent > max_recent {
                break;
            }
            kept += 1;
        }

        // Steps after the model's last reply are unseen and stay unsummarized
        // unless together they pass half the window
        let mut unseen = 0;
        let mut pending = 0;
        for step in visible.steps.iter().rev() {
            let unread = matches!(
                step,
                MemoryStep::Task(_)
                    | MemoryStep::Update(_)
                    | MemoryStep::Shell(_)
                    | MemoryStep::Restarted
                    | MemoryStep::Done(_)
            );
            // The model wrote the reply but hasn't seen its call replies or errors
            let reported = match step {
                MemoryStep::Action(action) => !action.calls.is_empty() || action.error.is_some(),
                _ => false,
            };
            if !unread && !reported {
                break;
            }

            let messages = step.messages(&mut 0, &takes, rank);
            let sent = bytes(&messages);
            unseen += estimate(sent);
            if unseen > window / 2 {
                break;
            }
            pending += 1;
            // The model has seen everything before this step
            if reported {
                break;
            }
        }
        let kept = kept.max(pending);

        let older = &visible.steps[..visible.steps.len() - kept];
        if older.is_empty() {
            return Ok(());
        }
        // A restart among the kept steps would read as undoing the names listed before them
        let restarted = visible.steps[older.len()..]
            .iter()
            .any(|step| matches!(step, MemoryStep::Restarted));

        self.sink.event(Event::Compact);
        let interrupt = &self.agent.interrupt;
        let started = Instant::now();
        let mut previous = visible.summary.map(|summary| summary.summary.clone());
        let mut usage = Usage::default();
        // If it does not fit, chunk it oldest first with the summary so far
        // and cut only a step too big alone
        let mut start = 0;
        while start < older.len() {
            // The request also holds the output limit, the prompt and the summary so far,
            // and a smaller budget would make every step a chunk of its own
            let mut budget = (window as f64 * MAX_TRANSCRIPT_SHARE) as u64;
            if let Some(output) = output {
                let asked = prompt::summary("", previous.as_deref(), true);
                let fixed = text_bytes(SUMMARIZER) + text_bytes(&asked);
                let fixed = estimate(fixed);
                let room = window.saturating_sub(output + fixed);
                let least = (window as f64 * MIN_TRANSCRIPT_SHARE) as u64;
                budget = budget.min(room).max(least);
            }

            // Steps up to `end` fit and steps up to `over` do not
            let mut end = start + 1;
            let mut transcript = squeezed(&older[start..end], budget);
            let mut over = older.len() + 1;
            while over - end > 1 {
                let middle = (end + over) / 2;
                let longer = prompt::transcript(&older[start..middle], usize::MAX);
                let size = text_bytes(&longer);
                if estimate(size) <= budget {
                    end = middle;
                    transcript = longer;
                } else {
                    over = middle;
                }
            }
            start = end;

            let trailing = kept > 0 && end == older.len();
            let asked = prompt::summary(&transcript, previous.as_deref(), trailing);
            // A fresh request, as continuing the conversation makes the model keep doing the task
            let request = [
                ChatMessage::System(SUMMARIZER.to_owned()),
                ChatMessage::user(asked),
            ];

            let mut tries = 0;
            let text = loop {
                let replying = text_reply(model, interrupt, &request, Purpose::Summary);
                let (replied, measured) = replying.await;
                // A cut-off summary is kept too
                let failure = match replied {
                    Ok(text) => {
                        usage.add(&measured);
                        break text.trim().to_owned();
                    }
                    Err(failure) => failure,
                };
                if retry(&mut tries, &failure, interrupt, &mut *self.sink).await {
                    continue;
                }

                self.sink.event(Event::CompactEnd(Err(&failure)));
                let Failure::Model(error) = failure else {
                    return Ok(());
                };
                return Err(error.into());
            };
            previous = Some(text);
        }

        // Summarized steps never go back, so they keep no items of their provider
        let mut recent = kept;
        for step in self.memory.steps.iter_mut().rev() {
            match step {
                MemoryStep::Summary(_) => {}
                _ if recent > 0 => recent -= 1,
                MemoryStep::Action(action) => action.sealed = None,
                _ => {}
            }
        }

        let summary = previous.expect("summary");
        // Covers every chunk and the time spent waiting to retry
        usage.duration = started.elapsed().as_secs_f64();
        let globals = match restarted {
            true => Globals::default(),
            false => self.worker.globals().await,
        };
        let summary = SummaryStep {
            summary,
            globals,
            kept,
            usage,
            used: None,
        };

        self.memory.steps.push(MemoryStep::Summary(summary));
        let used = self.memory.tokens(&self.system, &takes, rank, Some(window));
        let Some(MemoryStep::Summary(summary)) = self.memory.steps.last_mut() else {
            panic!("summary");
        };
        summary.used = Some(used);
        self.sink.event(Event::CompactEnd(Ok(summary)));
        self.sink.saved(self.memory);

        Ok(())
    }
}

/// Always returns a transcript however long, cutting observations first and then the middle
fn squeezed(steps: &[&MemoryStep], budget: u64) -> String {
    // Stored observations are already cut and a second cut would lose their count,
    // so try them as they are
    let mut max = usize::MAX;
    let text = loop {
        let text = prompt::transcript(steps, max);
        let size = text_bytes(&text);
        if estimate(size) <= budget {
            return text;
        }
        if max <= MIN_OBSERVATION_CHARS {
            break text;
        }
        max = max.min(MAX_OBSERVATION_CHARS) / 2;
    };

    // Tokens become characters at the text's average rate,
    // so a denser head or tail may pass the budget a little
    let size = text_bytes(&text);
    let tokens = estimate(size).max(1);
    let chars = text.chars().count() as u64;
    let kept = budget.saturating_mul(chars) / tokens;
    truncated(&text, kept as usize)
}

#[cfg(test)]
mod tests {
    use crate::agent::compact::*;
    use crate::agent::tests::{called, recorded, reply, Run, Script};
    use crate::model::{Generating, MediaKind, Model, ModelError, StreamDelta, TokenUsage};
    use crate::steps::tests::{asked, coded, metered, replied};
    use crate::steps::{AgentMemory, AgentStep};
    use crate::tests::content;
    use crate::worker::host::Rank;
    use crate::worker::Variable;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn agent(window: Option<u64>, replies: Vec<Result<Script, ModelError>>) -> Run {
        recorded(replies, window, Box::new(|_| {}))
    }

    /// Answers as `inner` does, with an output limit
    struct Limited {
        inner: Arc<dyn Model>,
        max_tokens: u64,
    }

    impl Model for Limited {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            purpose: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            self.inner.generate(conversation, purpose, on_delta)
        }

        fn context_window(&self) -> Option<u64> {
            self.inner.context_window()
        }

        fn max_tokens(&self) -> Option<u64> {
            Some(self.max_tokens)
        }

        fn takes(&self, kind: MediaKind) -> bool {
            self.inner.takes(kind)
        }
    }

    fn limited(run: &mut Run, max_tokens: u64) {
        let inner = Arc::clone(&run.agent.agent.model);
        run.agent.agent.model = Arc::new(Limited { inner, max_tokens });
    }

    fn watched(
        window: Option<u64>,
        replies: Vec<Result<Script, ModelError>>,
    ) -> (Run, Arc<Mutex<Vec<String>>>) {
        let events = Arc::<Mutex<Vec<String>>>::default();
        let seen = Arc::clone(&events);
        let on_event = Box::new(move |event: Event| {
            let kind = match event {
                Event::Compact => "compact".to_owned(),
                Event::CompactEnd(Ok(summary)) => format!("summary: {}", summary.summary),
                Event::CompactEnd(Err(error)) => format!("failed: {error}"),
                _ => return,
            };
            seen.lock().expect("events").push(kind);
        });

        let run = recorded(replies, window, on_event);
        (run, events)
    }

    fn measured(script: Script, input_tokens: u64) -> Result<Script, ModelError> {
        Ok(Script {
            usage: Some(TokenUsage::new(input_tokens, 10)),
            ..script
        })
    }

    /// With a 1000-token window this compacts before the third step and keeps only the second
    fn filling() -> Vec<Result<Script, ModelError>> {
        vec![
            measured(called("x = 41\nprint('a' * 600)"), 500),
            measured(called("print('data')"), 840),
            measured(reply("x holds 41."), 900),
            measured(reply("42"), 100),
        ]
    }

    fn contents(request: &[ChatMessage]) -> Vec<String> {
        request.iter().map(content).collect()
    }

    #[test]
    fn older_steps_summarized() {
        let mut replies = filling();
        for script in replies.iter_mut().flatten() {
            let item =
                json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-café" });
            script.sealed = vec![item];
        }
        let (mut run, events) = watched(Some(1000), replies);
        let mut memory = AgentMemory::default();
        let answer = run.agent.turn(&mut memory, "Add 1 to x").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let requests = run.requests.lock().expect("requests");
        let [system, asked] = &requests[2][..] else {
            panic!("{:?}", requests[2]);
        };
        assert_eq!(content(system), SUMMARIZER);

        // The summarizer reads the older steps, and the latest stays word for word after the summary
        let asked = content(asked);
        assert!(
            asked.contains("Add 1 to x") && asked.contains("x = 41"),
            "{asked}"
        );
        assert!(!asked.contains("print('data')"), "{asked}");

        // The names the code left in the worker follow the summary
        let MemoryStep::Summary(summary) = &memory.steps[3] else {
            panic!("{:?}", memory.steps);
        };
        let x = Variable {
            name: "x".to_owned(),
            kind: "int".to_owned(),
            length: None,
        };
        let held = Globals {
            values: vec![x],
            ..Globals::default()
        };
        assert_eq!(summary.globals, held);
        let after = contents(&requests[3]);
        let summarized = prompt::summarized(summary, &["Add 1 to x"]);
        assert_eq!(
            after[1..],
            [summarized.as_str(), "eval:\nprint('data')", "data"]
        );

        let mut kinds = Vec::new();
        let mut sealed = Vec::new();
        for step in &memory.steps {
            let kind = match step {
                MemoryStep::Task(_) => "task",
                MemoryStep::Update(_) => "update",
                MemoryStep::Action(action) => {
                    sealed.push(action.sealed.is_some());
                    "action"
                }
                MemoryStep::Summary(_) => "summary",
                MemoryStep::Shell(_) => "shell",
                MemoryStep::Restarted => "restarted",
                MemoryStep::Done(_) => "done",
            };
            kinds.push(kind);
        }
        assert_eq!(kinds, ["task", "action", "action", "summary", "action"]);
        assert_eq!(sealed, [false, true, true]);

        let events = events.lock().expect("events");
        assert_eq!(*events, ["compact", "summary: x holds 41."]);
        assert!(summary.used.is_some());

        let saved = serde_json::to_string(&memory).expect("serialize");
        let loaded: AgentMemory = serde_json::from_str(&saved).expect("deserialize");
        let all = |_| true;
        assert_eq!(
            loaded.conversation("", &all, Rank::Main, None),
            memory.conversation("", &all, Rank::Main, None)
        );
    }

    /// Every summary request leaves the output limit free in the window,
    /// also a later chunk that carries the long summary of those before it
    #[test]
    fn summary_fits_output() {
        let window = 16_000;
        let max_tokens = 4000;
        let long = reply(&"brief ".repeat(750));
        let mut run = agent(
            Some(window),
            vec![Ok(long.clone()), Ok(long.clone()), Ok(long)],
        );
        limited(&mut run, max_tokens);
        let earlier = SummaryStep {
            summary: "brief ".repeat(10),
            kept: 1,
            ..SummaryStep::default()
        };
        let mut steps = vec![
            MemoryStep::Task(asked("Print the parts")),
            MemoryStep::Summary(earlier),
        ];
        // Each part is about a thousand tokens and fits a chunk whole
        for _ in 0..30 {
            let printed = "naïve café ".repeat(250);
            let ran = coded("print(part)", Some(printed));
            steps.push(MemoryStep::Action(Box::new(ran)));
        }
        let answered = AgentStep {
            usage: metered(13_000, 10),
            ..replied("Printed them")
        };
        steps.push(MemoryStep::Action(Box::new(answered)));
        let mut memory = AgentMemory { steps };
        run.agent.compact(&mut memory).expect("compact");

        assert!(matches!(memory.steps.last(), Some(MemoryStep::Summary(_))));
        let requests = run.requests.lock().expect("requests");
        assert!(requests.len() > 1, "{}", requests.len());
        for request in requests.iter() {
            let sent: usize = request
                .iter()
                .map(|message| text_bytes(&content(message)))
                .sum();
            assert!(estimate(sent) + max_tokens <= window, "{sent}");
        }
    }

    #[test]
    fn new_task_kept() {
        let replies = vec![
            measured(called("x = 41\nprint('a' * 600)"), 500),
            measured(reply("x is 41."), 840),
            measured(reply("x holds 41."), 900),
            measured(reply("42"), 100),
        ];
        let mut run = agent(Some(1000), replies);
        let mut memory = AgentMemory::default();
        run.agent.turn(&mut memory, "Check x").expect("run");
        let answer = run.agent.turn(&mut memory, "Add 1 to x").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let requests = run.requests.lock().expect("requests");
        let asked = content(&requests[2][1]);
        assert!(asked.contains("Check x"), "{asked}");
        assert!(!asked.contains("Add 1 to x"), "{asked}");

        let MemoryStep::Summary(summary) = &memory.steps[4] else {
            panic!("{:?}", memory.steps);
        };
        let summarized = prompt::summarized(summary, &[]);
        let after = contents(&requests[3]);
        assert_eq!(after[1..], [summarized.as_str(), "x is 41.", "Add 1 to x"]);
    }
}
