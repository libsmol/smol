use crate::agent::delegate::Records;
use crate::agent::{generate, retry, step_counts, Event, Failure, Streamed, Turn, MAX_RETRIES};
use crate::model::{Finish, ModelError, Purpose, Sealed, StreamDelta, Unread, MAX_NAME_CHARS};
use crate::prompt::TOOL;
use crate::spawn::SHOWN_EVERY;
use crate::steps::{
    estimate, text_bytes, AgentStep, Call, CallError, CodeResult, StepError, MAX_OBSERVATION_CHARS,
};
use crate::utils::truncated;
use std::mem;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::warn;

/// Normal code never ends in several lines of closing tags
const MAX_LOOP_TOKENS: u64 = 256;

impl Turn<'_> {
    pub(crate) async fn step(&mut self) -> Result<Box<AgentStep>, ModelError> {
        let model = &*self.agent.model;
        let window = model.context_window();
        let takes = |kind| model.takes(kind);
        let rank = self.agent.place.rank();
        let conversation = self.memory.conversation(&self.system, &takes, rank, window);

        let interrupt = &self.agent.interrupt;
        let mut tries = 0;
        let mut glitches = 0;
        // Time spent waiting to retry counts toward this step too
        let started = Instant::now();

        let mut step = loop {
            let mut step = Box::<AgentStep>::default();
            let mut reply = Reply::default();
            let enough = Notify::new();

            let mut shown = |event: Event| self.sink.event(event);
            let mut on_delta = |delta: StreamDelta| {
                match delta {
                    // Empty reasoning is kept too and sent back as it is
                    StreamDelta::Reasoning(text) => {
                        let thought = reply.reasoning.get_or_insert_with(String::new);
                        thought.push_str(text);
                        if !text.is_empty() {
                            let streamed = Streamed {
                                added: text,
                                whole: thought,
                            };
                            shown(Event::Thought(streamed));
                        }
                    }
                    StreamDelta::Content(text) => {
                        reply.text.push_str(text);
                        let streamed = Streamed {
                            added: text,
                            whole: &reply.text,
                        };
                        shown(Event::Text(streamed));
                    }
                    StreamDelta::CallCode(code) => reply.push_code(code, &mut shown),
                    StreamDelta::CallEnd(ended) => reply.end_call(ended, &mut shown),
                    StreamDelta::Sealed(sealed) => reply.sealed = Some(sealed.clone()),
                }

                // Some models repeat closing tags after the code for minutes until the output limit
                if estimate(reply.looped) > MAX_LOOP_TOKENS {
                    enough.notify_one();
                }
            };
            let generating = generate(
                model,
                interrupt,
                &enough,
                &conversation,
                Purpose::Step,
                &mut step.usage,
                &mut on_delta,
            );
            let generated = generating.await;
            // However the stream ended, the code it left unshown still shows
            reply.show_code(&mut shown);
            step.reasoning = reply.reasoning;
            step.sealed = reply.sealed;
            step.usage.duration = started.elapsed().as_secs_f64();

            step.text = reply.text.trim_end().to_owned();
            // Tag-only code counts as unreadable because the model keeps copying it from history
            // An unfinished reply may have written only the whitespace before the code
            let complete = matches!(generated, Ok(Finish::Stop));
            let tagged =
                |line: &str| line.is_empty() || (line.starts_with('<') && line.ends_with('>'));
            for mut call in reply.calls {
                let blank = call.code.lines().map(str::trim).all(tagged);
                if complete && blank && call.error.is_none() {
                    call.error = Some(CallError::NoCode);
                }
                call.code = call.code.trim().to_owned();
                step.calls.push(call);
            }

            // Codeless eval calls are rare glitches,
            // so regenerate and reply to each only after the last try
            let codeless = |call: &Call| call.error == Some(CallError::NoCode);
            let glitch = step.calls.iter().any(codeless) && glitches < MAX_RETRIES;
            let failure = match generated {
                Ok(Finish::Length) => {
                    step.error = Some(StepError::Cut);
                    break step;
                }
                Ok(_) if glitch => {
                    let reason = CallError::NoCode.to_string();
                    warn!("step: regenerating, {reason}");
                    let retried = Event::Retry {
                        reason: &reason,
                        wait: Duration::ZERO,
                    };
                    self.sink.event(retried);
                    glitches += 1;
                    continue;
                }
                Ok(_) => break step,
                Err(failure) => failure,
            };
            if retry(&mut tries, &failure, interrupt, &mut *self.sink).await {
                continue;
            }

            match failure {
                Failure::Interrupted => {
                    step.error = Some(StepError::Interrupted);
                    break step;
                }
                Failure::Model(error) => {
                    // The error is logged only by the line that ends the turn or the agent
                    let counted = step_counts(&step);
                    warn!("step: failed {counted}");
                    self.sink.event(Event::StepFailed(&step));
                    return Err(error);
                }
            }
        };

        // An interrupted reply keeps its code too, but only a step without an error runs
        if step.error.is_some() {
            return Ok(step);
        }

        for index in 0..step.calls.len() {
            // An interrupt before this call runs skips it and every later one
            if self.agent.interrupt.triggered() {
                step.error = Some(StepError::Interrupted);
                break;
            }
            if step.calls[index].unread() {
                self.sink.event(Event::RunEnd(index, &step.calls[index]));
                break;
            }

            let started = Instant::now();
            let call = &step.calls[index];
            self.sink.event(Event::Run(index, call));

            let views = self.sink.tasks(index);
            let records = Records::default();
            let mut serving = self.agent.serving(self.servers, views, &records);
            let code = &call.code;
            let mut shown = |output: &str| self.sink.event(Event::Output(index, call, output));
            let evaluating = self
                .worker
                .evaluate(code, self.servers, &mut shown, &mut *serving);
            let evaluated = evaluating.await;
            step.usage.duration += started.elapsed().as_secs_f64();
            // Every call that could still change them has ended with the code
            let mut recorded = records.lock().expect("records");
            let agents = mem::take(&mut *recorded);
            drop(recorded);

            let call = &mut step.calls[index];
            match evaluated {
                Ok(output) => {
                    // Leaves room in the budget for the error that follows the output
                    let mut error = output.error;
                    let mut ending = 0;
                    if let Some(CallError::Execution(text) | CallError::Interrupted(Some(text))) =
                        &mut error
                    {
                        *text = truncated(text, MAX_OBSERVATION_CHARS / 2);
                        ending = text.chars().count();
                    }

                    let budget = MAX_OBSERVATION_CHARS - ending;
                    let shown = output.printed.shown(budget);
                    let printed = (!shown.is_empty()).then_some(shown);
                    call.result = Some(CodeResult {
                        printed,
                        media: output.media,
                        agents,
                    });
                    call.error = error;

                    // Wrong tags stay out of history or the model copies them
                    if let Some(repaired) = output.repaired {
                        let written = mem::replace(&mut call.code, repaired.trim().to_owned());
                        call.unrepaired = Some(written);
                    }
                }
                // Code that never reached the worker gets no `result`
                Err(error) => call.error = Some(error),
            }
            self.sink.event(Event::RunEnd(index, call));

            let Some(error) = &call.error else {
                continue;
            };
            // An interrupt while the code runs is recorded on the step too
            if let CallError::Interrupted(_) = error {
                step.error = Some(StepError::Interrupted);
            }
            break;
        }

        Ok(step)
    }
}

/// The reply received so far, with calls in the order they appeared
#[derive(Default)]
struct Reply {
    reasoning: Option<String>,
    sealed: Option<Sealed>,
    text: String,
    calls: Vec<Call>,
    /// The last call is still streaming
    open: bool,
    /// Bytes of closing-tag lines at the end of the code, reset when real code arrives
    looped: usize,
    /// Bytes already scanned in the code being received
    checked: usize,
    /// Bytes already shown of the code being received, and when they last were
    shown: usize,
    shown_at: Option<Instant>,
}

impl Reply {
    fn push_code(&mut self, code: &str, on_event: &mut dyn FnMut(Event)) {
        if !self.open {
            let at = self.text.len();
            let call = Call {
                at,
                ..Call::default()
            };
            self.calls.push(call);
            self.open = true;
            self.checked = 0;
            self.shown = 0;
            self.shown_at = None;
        }

        let index = self.calls.len() - 1;
        self.calls[index].code.push_str(code);
        // An event carries all the code and comes at most every `SHOWN_EVERY` after the first
        let due = self.shown_at.is_none_or(|at| at.elapsed() >= SHOWN_EVERY);
        if due {
            self.show_code(on_event);
        }

        let written = &self.calls[index].code;
        while let Some(end) = written[self.checked..].find('\n') {
            let line = written[self.checked..self.checked + end].trim();
            if line.starts_with("</") && line.ends_with('>') {
                self.looped += text_bytes(line);
            } else if !line.is_empty() {
                self.looped = 0;
            }
            self.checked += end + 1;
        }
    }

    /// Shows the code that arrived for the open call since it was last shown
    fn show_code(&mut self, on_event: &mut dyn FnMut(Event)) {
        if !self.open {
            return;
        }

        let index = self.calls.len() - 1;
        let written = &self.calls[index].code;
        // A blank call is hidden live as it is on replay, since saved code is trimmed
        if self.shown == written.len() || written.trim().is_empty() {
            return;
        }

        let streamed = Streamed {
            added: &written[self.shown..],
            whole: written,
        };
        on_event(Event::Code(index, streamed));
        self.shown = written.len();
        self.shown_at = Some(Instant::now());
    }

    /// A call with no readable code never runs and keeps its raw arguments for display
    fn end_call(&mut self, ended: Result<(), Unread>, on_event: &mut dyn FnMut(Event)) {
        self.show_code(on_event);
        // Close an unreadable call even after it streamed code, or the next call writes into it
        let opened = mem::replace(&mut self.open, false);
        if !opened {
            let at = self.text.len();
            let call = Call {
                at,
                ..Call::default()
            };
            self.calls.push(call);
        }
        let Err(unread) = ended else {
            return;
        };

        let index = self.calls.len() - 1;
        let call = &mut self.calls[index];
        let error = match unread.name == TOOL {
            true => CallError::NoCode,
            // Its reply names it, cut as the name sent back to the server is
            false => {
                let name = unread.name.chars().take(MAX_NAME_CHARS).collect();
                CallError::NoTool(name)
            }
        };
        call.error = Some(error);
        call.arguments = Some(unread.arguments.to_owned());
        on_event(Event::Unread(index, call));
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::step::*;
    use crate::agent::tests::{called, recorded, reply, scripted, Script, ScriptedModel, Tested};
    use crate::model::{ChatMessage, TokenUsage};
    use crate::prompt;
    use crate::steps::{AgentMemory, MemoryStep};
    use crate::tests::{content, Temp};
    use crate::worker::host::Rank;
    use std::sync::{Arc, Mutex};

    fn agent(replies: Vec<Script>) -> Temp<Tested> {
        let replies = replies.into_iter().map(Ok);
        let model = ScriptedModel::new(replies.collect());
        scripted(model, Box::new(|_| {}))
    }

    fn actions(memory: &AgentMemory) -> Vec<&AgentStep> {
        let mut actions = Vec::new();
        for step in &memory.steps {
            if let MemoryStep::Action(step) = step {
                actions.push(&**step);
            }
        }
        actions
    }

    fn printed(step: &AgentStep) -> Option<&str> {
        let call = step.calls.first()?;
        call.result.as_ref()?.printed.as_deref()
    }

    fn codes(step: &AgentStep) -> Vec<&str> {
        step.calls.iter().map(|call| call.code.as_str()).collect()
    }

    #[test]
    fn calls_run_ordered() {
        let both = Script {
            calls: vec![
                Ok("a = 'alpha'\nprint(a)".to_owned()),
                Ok("print(a + ' beta')".to_owned()),
            ],
            ..reply("Running both at once.")
        };
        let mut agent = agent(vec![both]);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        let mut printed = Vec::new();
        for call in &step.calls {
            let result = call.result.as_ref().expect("ran");
            printed.push(result.printed.as_deref());
        }
        assert_eq!(printed, [Some("alpha"), Some("alpha beta")]);

        let action = MemoryStep::Action(Box::new(step.clone()));
        let mut calls = 3;
        let messages = action.messages(&mut calls, &|_| true, Rank::Main);
        let ChatMessage::Assistant { calls: sent, .. } = &messages[0] else {
            panic!("{messages:?}");
        };
        let ids: Vec<&str> = sent.iter().map(|call| call.id.as_str()).collect();
        assert_eq!(ids, ["call_4", "call_5"]);
        let answered = |message: &ChatMessage| match message {
            ChatMessage::Tool { id, .. } => id.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(answered(&messages[1]), "call_4");
        assert_eq!(answered(&messages[2]), "call_5");
        assert_eq!(calls, 5);
    }

    #[test]
    fn failure_skips_rest() {
        let executed = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&executed);
        let on_event = Box::new(move |event: Event| {
            if let Event::Run(index, _) = event {
                seen.lock().expect("executed").push(index);
            }
        });
        let failing = Script {
            calls: vec![
                Ok("print('first part')\n1 / 0".to_owned()),
                Ok("print('second part')".to_owned()),
            ],
            ..reply("")
        };
        let model = ScriptedModel::new(vec![Ok(failing)]);
        let mut agent = scripted(model, on_event);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        assert_eq!(step.error, None);
        assert_eq!(*executed.lock().expect("executed"), [0]);
        assert_eq!(printed(step), Some("first part"));
        let skipped = &step.calls[1];
        assert!(skipped.result.is_none() && skipped.error.is_none());
        let told = prompt::call_error(step, skipped).unwrap_or_default();
        assert_eq!(told, format!("[{}]", prompt::SKIPPED));
    }

    /// Tag-only code is quietly regenerated until tries run out, then never runs or is echoed back
    #[test]
    fn bare_tags_retried() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);
        let on_event = Box::new(move |event: Event| {
            let kind = match event {
                Event::Retry { reason, .. } => reason.to_owned(),
                Event::StepEnd(_) => "end".to_owned(),
                _ => return,
            };
            seen.lock().expect("events lock").push(kind);
        });
        let replies = [
            called("<eval>\n</eval>"),
            called(""),
            called("\n"),
            called("<script>\n"),
            reply("three"),
        ];
        let model = ScriptedModel::new(replies.into_iter().map(Ok).collect());
        let mut agent = scripted(model, on_event);
        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer.as_deref(), Some("three"));
        let codeless = "eval: no code in the arguments";
        let expected = [codeless, codeless, codeless, "end", "end"];
        assert_eq!(*events.lock().expect("events lock"), expected);

        let steps = actions(&memory);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].error, None);
        assert_eq!(steps[0].calls[0].error, Some(CallError::NoCode));
        assert!(printed(steps[0]).is_none());

        let action = MemoryStep::Action(Box::new(steps[0].clone()));
        let sent: Vec<String> = action
            .messages(&mut 0, &|_| true, Rank::Main)
            .iter()
            .map(content)
            .collect();
        let told = prompt::call_error(steps[0], &steps[0].calls[0]).expect("told");
        assert_eq!(sent, ["eval:\n".to_owned(), told]);
    }

    /// Calls before one to a missing tool still run, it gets an error, and the rest are skipped
    #[test]
    fn unknown_tool_answered() {
        let mixed = Script {
            calls: vec![
                Ok("print('alpha')".to_owned()),
                Err(("bash", r#"{"command": "ls docs"}"#)),
                Ok("print('beta')".to_owned()),
            ],
            ..reply("Let me look first.")
        };
        let mut agent = agent(vec![mixed]);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        assert_eq!(step.text, "Let me look first.");
        assert_eq!(printed(step), Some("alpha"));
        let bash = &step.calls[1];
        assert_eq!(bash.error, Some(CallError::NoTool("bash".to_owned())));
        let arguments = Some(r#"{"command": "ls docs"}"#.to_owned());
        assert_eq!(bash.arguments, arguments);

        let action = MemoryStep::Action(Box::new(step.clone()));
        let sent: Vec<String> = action
            .messages(&mut 0, &|_| true, Rank::Main)
            .iter()
            .map(content)
            .collect();
        let refused = prompt::call_error(step, bash).expect("refused");
        let skipped = prompt::call_error(step, &step.calls[2]).expect("skipped");
        let expected = [
            "Let me look first.\neval:\nprint('alpha')\nbash:\n\neval:\nprint('beta')".to_owned(),
            "alpha".to_owned(),
            refused,
            skipped,
        ];
        assert_eq!(sent, expected);
    }

    #[test]
    fn runaway_cut() {
        let usage = Some(TokenUsage::new(10, 5));
        // Tag lines of 770 bytes are the most that still read to the end
        let closers = "</invoke>\n".repeat(85);
        let below = format!("{closers}</em>\n");
        let above = format!("{closers}</sub>\n");
        assert_eq!(estimate(770), MAX_LOOP_TOKENS);
        let bar = '\u{ff5c}';
        let written = [
            format!("print('one')\n{above}"),
            format!("print('two')\n</{bar}{bar}X{bar}{bar} parameter>\n{above}"),
            format!("print('three')\n{below}"),
        ];
        let mut replies = Vec::new();
        for code in &written {
            let script = Script {
                usage,
                ..called(code)
            };
            replies.push(Ok(script));
        }
        let model = ScriptedModel::new(replies);
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let steps = actions(&memory);
        let mut outputs = Vec::new();
        let mut measured = Vec::new();
        for step in &steps[..3] {
            outputs.push(printed(step));
            measured.push(step.usage.tokens.is_some());
        }
        assert_eq!(outputs, [Some("one"), Some("two"), Some("three")]);
        assert_eq!(measured, [false, false, true]);
        assert_eq!(codes(steps[1]), ["print('two')"]);
    }

    #[test]
    fn cut_reply_unused() {
        let cut = Script {
            calls: vec![Ok("print('half".to_owned())],
            finish: Finish::Length,
            ..reply("Starting.\n")
        };
        let replies = vec![Ok(cut), Ok(reply("Short."))];
        let mut run = recorded(replies, None, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = run.agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, Some("Short.".to_owned()));

        let step = actions(&memory)[0];
        assert_eq!(step.error, Some(StepError::Cut));
        assert_eq!(step.text, "Starting.");
        assert_eq!(codes(step), ["print('half"]);
        assert_eq!(step.seen(), ("", &[][..]));
        assert!(printed(step).is_none());
        let told = prompt::step_error(step).unwrap_or_default();
        assert!(told.contains("too long and was discarded"), "{told}");

        let requests = run.requests.lock().expect("requests");
        let next: Vec<String> = requests[1][1..].iter().map(content).collect();
        assert_eq!(next, ["task".to_owned(), told]);
    }
}
