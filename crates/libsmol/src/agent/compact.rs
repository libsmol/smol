use crate::agent::{retry, text_reply, Event, Turn};
use crate::memory::{
    bytes, estimate, text_bytes, MemoryStep, StepError, SummaryStep, Usage, Verdict,
    MAX_OBSERVATION_CHARS,
};
use crate::model::{ChatMessage, Purpose};
use crate::prompt::{self, SUMMARIZER};
use crate::utils::truncated;
use crate::Result;
use std::time::Instant;

/// Reserved for the next reply only, as the summary request has its own budget
const RESERVE_SHARE: f64 = 0.15;
const MAX_RESERVE_TOKENS: u64 = 16_384;
const MAX_RECENT_SHARE: f64 = 0.15;
/// The rest is left for the summary
pub(crate) const MAX_TRANSCRIPT_SHARE: f64 = 0.7;
const MIN_OBSERVATION_CHARS: usize = 1024;

impl Turn<'_> {
    /// An interrupt yields no summary and is recorded by the next step
    pub(crate) async fn compact(&mut self) -> Result<()> {
        let model = &*self.agent.model;
        let Some(window) = model.context_window() else {
            return Ok(());
        };

        // Only the last step counts, as a summary followed by an unrecorded failure
        // must not stop later compaction
        let added = !matches!(self.memory.steps.last(), Some(MemoryStep::Summary(_)));
        let takes = |kind| model.takes(kind);
        let used = self.memory.tokens(&self.system, &takes, Some(window));
        let reserve = (window as f64 * RESERVE_SHARE) as u64;
        let reserve = reserve.min(MAX_RESERVE_TOKENS);
        let interrupted = self.agent.interrupt.triggered();
        if !added || interrupted || used < window - reserve {
            return Ok(());
        }

        let visible = self.memory.visible();

        let max_recent = (window as f64 * MAX_RECENT_SHARE) as u64;
        let mut recent = 0;
        let mut kept = 0;
        for step in visible.steps.iter().rev() {
            let messages = step.messages(&mut 0);
            let sent = bytes(&messages, &takes);
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
            );
            // The model wrote the reply but hasn't seen its call replies, errors or verdict
            let reported = match step {
                MemoryStep::Action(action) => {
                    let verdict = action.check.as_ref().map(|check| check.verdict);
                    let told = matches!(verdict, Some(Verdict::Unmet | Verdict::Blocked));
                    !action.calls.is_empty() || action.error.is_some() || told
                }
                _ => false,
            };
            if !unread && !reported {
                break;
            }

            let messages = step.messages(&mut 0);
            let sent = bytes(&messages, &takes);
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

        // If it does not fit, chunk it oldest first with the previous summary
        // and cut only a step too big alone
        let budget = (window as f64 * MAX_TRANSCRIPT_SHARE) as u64;
        let mut transcripts = Vec::new();
        let mut start = 0;
        while start < older.len() {
            // Steps up to `end` fit and steps up to `over` do not
            let mut end = start + 1;
            let mut text = squeezed(&older[start..end], window);
            let mut over = older.len() + 1;
            while over - end > 1 {
                let middle = (end + over) / 2;
                let longer = prompt::transcript(&older[start..middle], usize::MAX);
                let size = text_bytes(&longer);
                if estimate(size) <= budget {
                    end = middle;
                    text = longer;
                } else {
                    over = middle;
                }
            }
            transcripts.push(text);
            start = end;
        }

        self.sink.event(Event::Compact);
        let interrupt = &self.agent.interrupt;
        let started = Instant::now();
        let mut previous = visible.summary.map(|summary| summary.summary.clone());
        let mut usage = Usage::default();
        let last = transcripts.len() - 1;
        for (index, transcript) in transcripts.iter().enumerate() {
            let trailing = kept > 0 && index == last;
            let asked = prompt::summary(transcript, previous.as_deref(), trailing);
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
                let error = match replied {
                    Ok(text) => {
                        usage.add(&measured);
                        break text.trim().to_owned();
                    }
                    Err(error) => error,
                };
                if retry(&mut tries, &error, interrupt, &mut *self.sink).await {
                    continue;
                }

                self.sink.event(Event::CompactEnd(Err(&error)));
                let StepError::Model(error) = error else {
                    return Ok(());
                };
                return Err(error.into());
            };
            previous = Some(text);
        }

        let summary = previous.expect("summary");
        // Covers every chunk and the time spent waiting to retry
        usage.duration = started.elapsed().as_secs_f64();
        let summary = SummaryStep {
            summary,
            kept,
            usage,
            used: None,
        };

        self.memory.steps.push(MemoryStep::Summary(summary));
        let used = self.memory.tokens(&self.system, &takes, Some(window));
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
pub(crate) fn squeezed(steps: &[&MemoryStep], window: u64) -> String {
    let budget = (window as f64 * MAX_TRANSCRIPT_SHARE) as u64;
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
    // and the window's spare 30% covers a denser head or tail
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
    use crate::memory::tests::{asked, coded, metered, replied};
    use crate::memory::{AgentMemory, AgentStep, Attachment, Check, Exit, ShellStep, TaskStep};
    use crate::model::{Finish, ModelError, TokenUsage};
    use crate::tests::content;
    use std::sync::{Arc, Mutex};

    fn agent(window: Option<u64>, replies: Vec<Result<Script, ModelError>>) -> Run {
        recorded(replies, window, SUMMARIZER, Box::new(|_| {}))
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

        let run = recorded(replies, window, SUMMARIZER, on_event);
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

    fn summarized(memory: &AgentMemory) -> bool {
        let summary = |step: &MemoryStep| matches!(step, MemoryStep::Summary(_));
        memory.steps.iter().any(summary)
    }

    fn contents(request: &[ChatMessage]) -> Vec<String> {
        request.iter().map(content).collect()
    }

    #[test]
    fn older_steps_summarized() {
        let (mut run, events) = watched(Some(1000), filling());
        let mut memory = AgentMemory::default();
        let answer = run.agent.turn(&mut memory, "Add 1 to x").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let requests = run.requests.lock().expect("requests");
        let [system, asked] = &requests[2][..] else {
            panic!("{:?}", requests[2]);
        };
        assert_eq!(content(system), SUMMARIZER);

        let asked = content(asked);
        let start = "<conversation>\n[Task]: Add 1 to x\n\n[Code]: x = 41\n";
        assert!(asked.starts_with(start), "{asked}");
        assert!(!asked.contains("print('data')"), "{asked}");
        assert!(
            asked.contains("The conversation above leaves out its latest messages"),
            "{asked}"
        );

        let after = contents(&requests[3]);
        assert_eq!(
            after[1..],
            [
                "Earlier messages were replaced by this summary:\n\n<summary>\nx holds 41.\n</summary>\n\nCurrent task:\nAdd 1 to x",
                "eval:\nprint('data')",
                "data",
            ]
        );

        let mut kinds = Vec::new();
        for step in &memory.steps {
            let kind = match step {
                MemoryStep::Task(_) => "task",
                MemoryStep::Update(_) => "update",
                MemoryStep::Action(_) => "action",
                MemoryStep::Summary(_) => "summary",
                MemoryStep::Shell(_) => "shell",
                MemoryStep::Cleared => "cleared",
                MemoryStep::Restarted => "restarted",
            };
            kinds.push(kind);
        }
        assert_eq!(kinds, ["task", "action", "action", "summary", "action"]);

        let events = events.lock().expect("events");
        assert_eq!(*events, ["compact", "summary: x holds 41."]);

        let MemoryStep::Summary(summary) = &memory.steps[3] else {
            panic!("{:?}", memory.steps);
        };
        assert!(summary.used.is_some());

        let saved = serde_json::to_string(&memory).expect("serialize");
        let loaded: AgentMemory = serde_json::from_str(&saved).expect("deserialize");
        assert_eq!(loaded.conversation("", None), memory.conversation("", None));
    }

    #[test]
    fn reserve_capped() {
        let window = 1 << 20;
        let filled = |tokens: u64| {
            let task = asked("Tidy the logs", false);
            let printed = "log ".repeat(150_000);
            let read = coded("print(log)", Some(printed));
            let counted = AgentStep {
                usage: metered(tokens, 0),
                ..replied("The log is 600,000 characters long.")
            };
            let steps = vec![
                MemoryStep::Task(task),
                MemoryStep::Action(Box::new(read)),
                MemoryStep::Action(Box::new(counted)),
            ];
            AgentMemory { steps }
        };
        let mut run = agent(Some(window), vec![Ok(reply("Summary"))]);
        let limit = window - MAX_RESERVE_TOKENS;

        let mut below = filled(limit - 1);
        run.agent.compact(&mut below).expect("compact");
        assert!(!summarized(&below));

        let mut full = filled(limit);
        run.agent.compact(&mut full).expect("compact");
        assert!(summarized(&full));
    }

    #[test]
    fn transcript_cut() {
        let replies = vec![Ok(reply("Read the file")), Ok(reply("Printed it"))];
        let mut run = agent(Some(4000), replies);
        let data = Attachment {
            path: "data.txt".to_owned(),
            content: "café ".repeat(1800),
            media: None,
        };
        let task = TaskStep {
            files: vec![data],
            ..asked("Print data", false)
        };
        let printed = "naïve ".repeat(1500);
        let ran = coded("print(data)", Some(printed));
        let mut memory = AgentMemory {
            steps: vec![MemoryStep::Task(task), MemoryStep::Action(Box::new(ran))],
        };
        run.agent.compact(&mut memory).expect("compact");

        let requests = run.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        let budget = (4000.0 * MAX_TRANSCRIPT_SHARE) as u64;
        let mut transcripts = Vec::new();
        for request in requests.iter() {
            let asked = content(&request[1]);
            let (_, transcript) = asked.split_once("<conversation>").unwrap_or_default();
            let (transcript, _) = transcript.split_once("</conversation>").unwrap_or_default();
            let size = text_bytes(transcript);
            assert!(estimate(size) <= budget, "{size}");
            let cuts = transcript.matches("characters cut").count();
            assert_eq!(cuts, 1, "{transcript}");
            transcripts.push(transcript.to_owned());
        }
        assert!(
            transcripts[0].contains("[Attached data.txt]: café café café"),
            "{transcripts:?}"
        );
        assert!(transcripts[1].contains("naïve naïve"), "{transcripts:?}");
    }

    #[test]
    fn stored_cut_kept() {
        let printed = "über ".repeat(8000);
        let stored = truncated(&printed, MAX_OBSERVATION_CHARS);
        let ran = coded("print(log)", Some(stored));
        let step = MemoryStep::Action(Box::new(ran));

        let text = squeezed(&[&step], 1 << 20);
        let marker = prompt::cut(40_000 - MAX_OBSERVATION_CHARS);
        assert!(text.contains(&marker), "{text}");
    }

    #[test]
    fn interrupted_summary_dropped() {
        let (mut run, events) = watched(Some(1000), filling());
        let interrupt = run.agent.agent.interrupt.clone();
        *run.interrupt.lock().expect("interrupt") = Some(interrupt);

        let mut memory = AgentMemory::default();
        let answer = run.agent.turn(&mut memory, "Add 1 to x").expect("run");
        assert_eq!(answer, None);
        assert!(!summarized(&memory));

        let Some(MemoryStep::Action(step)) = memory.steps.last() else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(step.error, Some(StepError::Interrupted));

        let events = events.lock().expect("events");
        assert_eq!(*events, ["compact", "failed: interrupted"]);
    }

    #[test]
    fn cut_summary_fails() {
        let mut replies = filling();
        replies[2] = Ok(Script {
            finish: Finish::Length,
            ..reply("")
        });
        let mut run = agent(Some(1000), replies);
        let mut memory = AgentMemory::default();
        let error = run.agent.turn(&mut memory, "Add 1 to x").expect_err("cut");
        assert!(error.to_string().contains("output limit"), "{error}");
        assert_eq!(run.requests.lock().expect("requests").len(), 3);
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
        assert!(
            asked.starts_with("<conversation>\n[Task]: Check x\n"),
            "{asked}"
        );
        assert!(!asked.contains("Add 1 to x"), "{asked}");

        let after = contents(&requests[3]);
        assert_eq!(
            after[1..],
            [
                "Earlier messages were replaced by this summary:\n\n<summary>\nx holds 41.\n</summary>",
                "x is 41.",
                "New task:\nAdd 1 to x",
            ]
        );
    }

    /// A model error right after compaction leaves the resent task with the same conversation
    #[test]
    fn resent_after_summary() {
        let replies = vec![
            measured(called("x = 41\nprint('a' * 600)"), 500),
            measured(reply("x is 41."), 840),
            measured(reply("x holds 41."), 900),
            Err(ModelError::new("http 400: bad request")),
            measured(reply("42"), 100),
        ];
        let mut run = agent(Some(1000), replies);
        let mut memory = AgentMemory::default();
        run.agent.turn(&mut memory, "Check x").expect("run");
        run.agent.turn(&mut memory, "Add 1 to x").expect_err("400");
        assert!(summarized(&memory));
        let answer = run.agent.turn(&mut memory, "Add 1 to x").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let requests = run.requests.lock().expect("requests");
        let resent = contents(&requests[4]);
        assert_eq!(resent, contents(&requests[3]));
        let asked = "New task:\nAdd 1 to x";
        let count = resent.iter().filter(|text| *text == asked).count();
        assert_eq!(count, 1, "{resent:?}");
    }

    /// The command output is over the share for recent steps, so only the unseen rule keeps it
    #[test]
    fn shell_kept() {
        let replies = vec![
            measured(called("x = 41\nprint('a' * 600)"), 500),
            measured(reply("x is 41."), 840),
            measured(reply("x holds 41."), 900),
            measured(reply("42"), 100),
        ];
        let mut run = agent(Some(1000), replies);
        let mut memory = AgentMemory::default();
        run.agent.turn(&mut memory, "Check x").expect("run");
        let ran = ShellStep {
            command: "cat notes.txt".to_owned(),
            shell: "fish".to_owned(),
            output: "log line\n".repeat(100),
            end: Exit::Exited(0),
        };
        let shown = prompt::shell(&ran);
        memory.steps.push(MemoryStep::Shell(ran));
        run.agent.turn(&mut memory, "Add 1 to x").expect("run");

        let requests = run.requests.lock().expect("requests");
        let asked = content(&requests[2][1]);
        assert!(!asked.contains("notes.txt"), "{asked}");
        let after = contents(&requests[3]);
        assert_eq!(
            after[after.len() - 2..],
            [shown, "New task:\nAdd 1 to x".to_owned()]
        );
    }

    /// The latest output is over the recent share but stays because the model has not seen it
    #[test]
    fn latest_output_kept() {
        let mut run = agent(Some(1000), vec![Ok(reply("Read the first one"))]);
        let first = AgentStep {
            usage: metered(500, 10),
            ..coded("print('first')", Some("first".to_owned()))
        };
        let printed = "log ".repeat(150);
        let latest = AgentStep {
            usage: metered(800, 10),
            ..coded("print(log)", Some(printed.clone()))
        };
        let mut memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(asked("Read the log", false)),
                MemoryStep::Action(Box::new(first)),
                MemoryStep::Action(Box::new(latest)),
            ],
        };
        run.agent.compact(&mut memory).expect("compact");

        assert!(summarized(&memory));
        let requests = run.requests.lock().expect("requests");
        let asked = content(&requests[0][1]);
        assert!(!asked.contains("log log"), "{asked}");
        let conversation = memory.conversation("", None);
        let texts = contents(&conversation);
        assert_eq!(texts.last(), Some(&printed), "{texts:?}");
    }

    #[test]
    fn verdict_kept() {
        let reason = "the report has no conclusion";
        let cases = [
            (Verdict::Unmet, prompt::unmet(reason)),
            (Verdict::Blocked, prompt::blocked(reason)),
        ];
        for (verdict, told) in cases {
            let mut run = agent(Some(1000), vec![Ok(reply("Did the first one"))]);
            let ran = AgentStep {
                usage: metered(500, 10),
                ..coded("print('first')", Some("first".to_owned()))
            };
            let check = Check {
                verdict,
                reason: reason.to_owned(),
                usage: Usage::default(),
            };
            let answered = AgentStep {
                usage: metered(950, 10),
                check: Some(check),
                ..replied(&"report".repeat(200))
            };
            let mut memory = AgentMemory {
                steps: vec![
                    MemoryStep::Task(asked("First one", false)),
                    MemoryStep::Action(Box::new(ran)),
                    MemoryStep::Task(asked("Write the report", true)),
                    MemoryStep::Action(Box::new(answered)),
                ],
            };
            run.agent.compact(&mut memory).expect("compact");

            assert!(summarized(&memory));
            let conversation = memory.conversation("", None);
            let texts = contents(&conversation);
            assert_eq!(texts.last(), Some(&told), "{texts:?}");
        }
    }

    #[test]
    fn early_interrupt_uncompacted() {
        let (mut run, events) = watched(Some(1000), vec![Ok(reply("Summary"))]);
        let ran = AgentStep {
            usage: metered(950, 10),
            ..coded("print('first')", Some("first".to_owned()))
        };
        let mut memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(asked("First one", false)),
                MemoryStep::Action(Box::new(ran)),
            ],
        };
        run.agent.agent.interrupt.cancel(0);
        let answer = run.agent.turn(&mut memory, "Second one").expect("run");
        assert_eq!(answer, None);

        assert!(!summarized(&memory));
        let events = events.lock().expect("events");
        assert!(events.is_empty(), "{events:?}");
        let Some(MemoryStep::Action(step)) = memory.steps.last() else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(step.error, Some(StepError::Interrupted));
    }

    #[test]
    fn long_history_chunked() {
        let replies = vec![Ok(reply("First part")), Ok(reply("Second part"))];
        let mut run = agent(Some(4000), replies);
        // Each output fits alone and both fit by characters but not by bytes
        let ran = |printed: &str| {
            let printed = printed.repeat(500);
            let step = coded("print(read())", Some(printed));
            MemoryStep::Action(Box::new(step))
        };
        let answered = AgentStep {
            usage: metered(3900, 10),
            ..replied("Read them all")
        };
        let task = asked("Tidy up", false);
        let mut memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(task),
                ran("café 🐍 "),
                ran("naïve 🐍 "),
                MemoryStep::Action(Box::new(answered)),
            ],
        };
        run.agent.compact(&mut memory).expect("compact");

        let requests = run.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        let budget = (4000.0 * MAX_TRANSCRIPT_SHARE) as u64;
        for request in requests.iter() {
            let asked = content(&request[1]);
            let (_, transcript) = asked.split_once("<conversation>").unwrap_or_default();
            let (transcript, _) = transcript.split_once("</conversation>").unwrap_or_default();
            let size = text_bytes(transcript);
            assert!(estimate(size) <= budget, "{size}");
            // Cut observations would fit in one chunk, but chunking leaves them whole
            assert!(!transcript.contains("characters cut"), "{transcript}");
        }
        let second = content(&requests[1][1]);
        let start = "<previous-summary>\nFirst part\n</previous-summary>\n\n<conversation>\n";
        assert!(second.starts_with(start), "{second}");

        let Some(MemoryStep::Summary(summary)) = memory.steps.last() else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(summary.summary, "Second part");
    }

    #[test]
    fn huge_update_summarized() {
        let replies = vec![Ok(reply("Recap")), Ok(reply("Recap")), Ok(reply("Recap"))];
        let mut run = agent(Some(10_000), replies);
        let printed = "first".to_owned();
        let ran = AgentStep {
            usage: metered(9000, 10),
            ..coded("print('first')", Some(printed))
        };
        let data = Attachment {
            path: "big.txt".to_owned(),
            content: "crème brûlée ".repeat(3375),
            media: None,
        };
        let update = TaskStep {
            files: vec![data],
            ..asked("See the attached file", false)
        };
        let first = asked("First one", false);
        let mut memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(first),
                MemoryStep::Action(Box::new(ran)),
                MemoryStep::Update(update),
            ],
        };
        run.agent.compact(&mut memory).expect("compact");

        assert!(summarized(&memory));
        let conversation = memory.conversation("", None);
        let texts = contents(&conversation);
        assert!(
            texts.iter().all(|text| !text.contains("brûlée crème")),
            "{texts:?}"
        );
        assert!(texts[1].contains("See the attached file"), "{texts:?}");
    }

    #[test]
    fn task_after_summary() {
        let mut run = agent(Some(1000), vec![Ok(reply("Second summary"))]);
        let printed = "data ".repeat(150);
        let ran = coded("print(data)", Some(printed));
        let answered = replied(&"done ".repeat(150));
        let task = |text: &str| MemoryStep::Task(asked(text, false));
        let earlier = SummaryStep {
            summary: "brief ".repeat(165),
            kept: 2,
            ..SummaryStep::default()
        };
        let mut memory = AgentMemory {
            steps: vec![
                task("First one"),
                MemoryStep::Action(Box::new(ran)),
                MemoryStep::Action(Box::new(answered)),
                MemoryStep::Summary(earlier),
                task("Second one"),
            ],
        };

        run.agent.compact(&mut memory).expect("compact");
        assert_eq!(run.agent.probe.saves, [6]);

        let Some(MemoryStep::Summary(summary)) = memory.steps.last() else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(summary.summary, "Second summary");
        assert_eq!(summary.kept, 1);
    }
}
