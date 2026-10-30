use crate::agent::delegate::Records;
use crate::agent::{generate, retry, Event, Streamed, Turn, MAX_RETRIES};
use crate::memory::{
    estimate, text_bytes, AgentStep, Call, CallError, CodeResult, StepError, MAX_OBSERVATION_CHARS,
};
use crate::model::{ChatMessage, Finish, Part, Purpose, StreamDelta, Unread};
use crate::prompt::{self, TOOL};
use crate::utils::{truncated, truncated_with};
use std::mem;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Normal code never ends in several lines of closing tags
const MAX_LOOP_TOKENS: u64 = 256;
const CODELESS: &str = "reply: a tool call without code, nothing ran";

impl Turn<'_> {
    pub(crate) async fn step(&mut self) -> Box<AgentStep> {
        let model = &*self.agent.model;
        let window = model.context_window();
        let takes = |kind| model.takes(kind);
        let mut conversation = self.memory.conversation(&self.system, &takes, window);
        // Saved sessions store only the file names of copies
        let copies = &self.agent.copies;
        for message in &mut conversation {
            let parts = match message {
                ChatMessage::User(parts) | ChatMessage::Tool { parts, .. } => parts,
                ChatMessage::System(_) | ChatMessage::Assistant { .. } => continue,
            };
            for part in parts {
                if let Part::Media(_, media) = part {
                    media.file = copies.join(&media.file);
                }
            }
        }

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
            step.reasoning = reply.reasoning;
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
            let error = match generated {
                Ok(Finish::Length) => StepError::Cut,
                Ok(_) if glitch => {
                    let retried = Event::Retry {
                        reason: CODELESS,
                        wait: Duration::ZERO,
                    };
                    self.sink.event(retried);
                    glitches += 1;
                    continue;
                }
                Ok(_) => break step,
                Err(error) => error,
            };
            if !retry(&mut tries, &error, interrupt, &mut *self.sink).await {
                step.error = Some(error);
                break step;
            }
        };

        // An interrupted reply keeps its code too, but only a step without an error runs
        if step.error.is_some() {
            return step;
        }

        for index in 0..step.calls.len() {
            // An interrupt before this call runs skips it and every later one
            if self.agent.interrupt.triggered() {
                step.error = Some(StepError::Interrupted);
                break;
            }
            if step.calls[index].unread() {
                self.sink.event(Event::Ran(index, &step.calls[index]));
                break;
            }

            let started = Instant::now();
            let call = &step.calls[index];
            self.sink.event(Event::Execute(index, call));

            let views = self.sink.helpers(index);
            let records = Records::default();
            let mut delegate = self.agent.delegate(self.servers, views, &records);
            let code = &call.code;
            let mut shown = |output: &str| self.sink.event(Event::Output(index, call, output));
            let evaluating = self
                .worker
                .evaluate(code, self.servers, &mut shown, &mut *delegate);
            let evaluated = evaluating.await;
            step.usage.duration += started.elapsed().as_secs_f64();
            // Every call that could still change them has ended with the code
            let agents = mem::take(&mut *records.lock().expect("records"));

            let call = &mut step.calls[index];
            match evaluated {
                Ok(output) => {
                    let logs = output.logs.trim_end_matches('\n');

                    // Leaves room in the budget for the error that follows the output
                    let error = match output.error {
                        Some(CallError::Execution(text)) => {
                            let text = truncated(&text, MAX_OBSERVATION_CHARS / 2);
                            Some(CallError::Execution(text))
                        }
                        other => other,
                    };
                    let ending = match &error {
                        Some(CallError::Execution(text)) => text.chars().count(),
                        _ => 0,
                    };

                    let quiet = error.is_none() && output.media.is_empty();
                    let printed = if !logs.is_empty() {
                        let shown =
                            truncated_with(logs, MAX_OBSERVATION_CHARS - ending, output.cut);
                        Some(shown)
                    } else if quiet {
                        Some(prompt::NO_OUTPUT.to_owned())
                    } else {
                        None
                    };
                    call.result = Some(CodeResult {
                        printed,
                        media: output.media,
                        agents,
                    });
                    call.error = error;

                    // Wrong tags stay out of history or the model copies them
                    if let Some(repaired) = output.code {
                        let written = mem::replace(&mut call.code, repaired.trim().to_owned());
                        call.unrepaired = Some(written);
                    }
                }
                // Code that never reached the worker gets no `result`
                Err(error) => call.error = Some(error),
            }
            self.sink.event(Event::Ran(index, call));

            let Some(error) = &call.error else {
                continue;
            };
            // An interrupt while the code runs is recorded on the step too
            if *error == CallError::Interrupted {
                step.error = Some(StepError::Interrupted);
            }
            break;
        }

        step
    }
}

/// The reply received so far, with calls in the order they appeared
#[derive(Default)]
struct Reply {
    reasoning: Option<String>,
    text: String,
    calls: Vec<Call>,
    /// The last call is still streaming
    open: bool,
    /// Bytes of closing-tag lines at the end of the code, reset when real code arrives
    looped: usize,
    /// Bytes already scanned in the code being received
    checked: usize,
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
        }
        let index = self.calls.len() - 1;
        let written = &mut self.calls[index].code;
        let earlier = written.trim().is_empty();
        written.push_str(code);
        // A blank call is hidden live as it is on replay, since saved code is trimmed
        let shown = match (earlier, written.trim().is_empty()) {
            (_, true) => None,
            (true, false) => Some(written.as_str()),
            (false, false) => Some(code),
        };
        if let Some(added) = shown {
            let streamed = Streamed {
                added,
                whole: written,
            };
            on_event(Event::Code(index, streamed));
        }

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

    /// A call with no readable code never runs and keeps its raw arguments for display
    fn end_call(&mut self, ended: Result<(), Unread>, on_event: &mut dyn FnMut(Event)) {
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
            false => CallError::NoTool(unread.name.to_owned()),
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
    use crate::agent::Task;
    use crate::memory::{AgentMemory, MemoryStep};
    use crate::model::{ModelError, TokenUsage};
    use crate::tests::{content, Temp};
    use crate::utils::{new_id, Interrupt};
    use std::fs;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;

    fn agent(replies: Vec<Script>) -> Temp<Tested> {
        let replies = replies.into_iter().map(Ok);
        let model = ScriptedModel::new(replies.collect());
        scripted(model, Box::new(|_| {}))
    }

    fn running(codes: &[&str]) -> Temp<Tested> {
        let replies = codes.iter().map(|code| called(code));
        agent(replies.collect())
    }

    /// Records true for every retry and false for every step end
    fn recording(
        replies: Vec<Result<Script, ModelError>>,
    ) -> (Temp<Tested>, Arc<Mutex<Vec<bool>>>) {
        let ends = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&ends);
        let on_event = Box::new(move |event: Event| {
            let retried = match event {
                Event::Retry { .. } => true,
                Event::StepEnd(_) => false,
                _ => return,
            };
            seen.lock().expect("ends lock").push(retried);
        });

        let model = ScriptedModel::new(replies);
        let agent = scripted(model, on_event);
        (agent, ends)
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

    fn only(memory: &AgentMemory) -> &AgentStep {
        let [step] = actions(memory)[..] else {
            panic!("{:?}", memory.steps);
        };
        step
    }

    fn interrupted(memory: &AgentMemory) -> &AgentStep {
        let step = only(memory);
        assert_eq!(step.error, Some(StepError::Interrupted));

        step
    }

    fn printed(step: &AgentStep) -> Option<&str> {
        let call = step.calls.first()?;
        call.result.as_ref()?.printed.as_deref()
    }

    fn codes(step: &AgentStep) -> Vec<&str> {
        step.calls.iter().map(|call| call.code.as_str()).collect()
    }

    /// Interrupts only once the code is really running and has created the file `name`
    fn interrupt_started(agent: &Tested, name: &str) {
        let marker = agent.agent.cwd.join(name);
        let interrupt = agent.agent.interrupt.clone();
        thread::spawn(move || {
            while !marker.exists() {
                thread::sleep(Duration::from_millis(10));
            }
            interrupt.cancel(u64::MAX);
            fs::remove_file(marker).ok();
        });
    }

    #[test]
    fn reply_trimmed() {
        let wrapped = Script {
            calls: vec![Ok("\n\nx = 1\n\n".to_owned())],
            ..reply("Doing it.\n\n")
        };
        let mut agent = agent(vec![wrapped]);
        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let steps = actions(&memory);
        assert_eq!(steps[0].text, "Doing it.");
        assert_eq!(codes(steps[0]), ["x = 1"]);
        assert_eq!(steps[1].answer(), Some("42"));
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
        let messages = action.messages(&mut calls);
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
            if let Event::Execute(index, _) = event {
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
        assert_eq!(told, "Error:\nnot run: an earlier call failed");
    }

    #[test]
    fn interrupt_skips_rest() {
        let id = new_id();
        let name = format!("smol-sleeping-{id}");
        let code =
            format!("import time\nprint('sleeping')\nopen({name:?}, 'w').close()\ntime.sleep(30)");
        let sleeping = Script {
            calls: vec![Ok(code), Ok("print('second part')".to_owned())],
            ..reply("")
        };
        let mut agent = agent(vec![sleeping]);
        interrupt_started(&agent, &name);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, None);

        let step = interrupted(&memory);
        assert_eq!(printed(step), Some("sleeping"));
        assert_eq!(step.calls[0].error, Some(CallError::Interrupted));
        let unrun = prompt::call_error(step, &step.calls[1]).unwrap_or_default();
        assert_eq!(unrun, "Error:\ninterrupted by the user before the code ran");
    }

    /// Tag-only code is quietly regenerated until tries run out, then never runs or is echoed back
    #[test]
    fn bare_tags_retried() {
        let replies = [
            called("<eval>\n</eval>"),
            called(""),
            called("\n"),
            called("<script>\n"),
            reply("three"),
        ];
        let (mut agent, ends) = recording(replies.into_iter().map(Ok).collect());
        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer.as_deref(), Some("three"));
        let retried = [true, true, true, false, false];
        assert_eq!(*ends.lock().expect("ends lock"), retried);

        let steps = actions(&memory);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].error, None);
        assert_eq!(steps[0].calls[0].error, Some(CallError::NoCode));
        assert!(printed(steps[0]).is_none());

        let action = MemoryStep::Action(Box::new(steps[0].clone()));
        let sent: Vec<String> = action.messages(&mut 0).iter().map(content).collect();
        let told = "Error:\nnot run: no Python code in the code argument";
        assert_eq!(sent, ["eval:\n", told]);
    }

    /// Regenerations and error retries each have their own count
    #[test]
    fn glitches_counted_apart() {
        let busy = ModelError::transient("http 503: service busy");
        let replies = vec![
            Ok(called("")),
            Ok(called("<eval>")),
            Ok(called("\n")),
            Err(busy),
            Ok(reply("Done")),
        ];
        let (mut agent, ends) = recording(replies);
        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer.as_deref(), Some("Done"));
        let retried = [true, true, true, true, false];
        assert_eq!(*ends.lock().expect("ends lock"), retried);
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
        let sent: Vec<String> = action.messages(&mut 0).iter().map(content).collect();
        let expected = [
            "Let me look first.\neval:\nprint('alpha')\nbash:\n\neval:\nprint('beta')",
            "alpha",
            "Error:\nnot run: there is no tool named \"bash\"; the only tool is eval",
            "Error:\nnot run: an earlier call failed",
        ];
        assert_eq!(sent, expected);
    }

    #[test]
    fn plain_reply_answers() {
        let answer = "Like this:\n```python\nprint('hello')\n```";
        let mut agent = agent(vec![reply(answer)]);

        let mut memory = AgentMemory::default();
        let value = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(value.as_deref(), Some(answer));

        let step = only(&memory);
        assert_eq!(step.answer(), Some(answer));
        assert!(step.calls.is_empty());
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
    fn loop_counted() {
        let mut ignore = |_: Event| {};
        let mut reply = Reply::default();
        reply.push_code("html = '''\n</div>\n'''\n", &mut ignore);
        assert_eq!(reply.looped, 0);
        reply.push_code("</para", &mut ignore);
        reply.push_code("meter>\n</invoke>\n", &mut ignore);
        let closers = text_bytes("</parameter>") + text_bytes("</invoke>");
        assert_eq!(reply.looped, closers);
        reply.push_code("print('crème brûlée')\n", &mut ignore);
        assert_eq!(reply.looped, 0);

        reply.end_call(Ok(()), &mut ignore);
        let code = "html = '''\n</div>\n'''\n</parameter>\n</invoke>\nprint('crème brûlée')\n";
        assert_eq!(reply.calls[0].code, code);
    }

    /// Code after an unreadable call starts a new call, which shows nothing while it is blank
    #[test]
    fn calls_kept_apart() {
        let mut events = Vec::new();
        let mut collect = |event: Event| {
            if let Event::Code(index, code) = event {
                events.push((index, code.added.to_owned()));
            }
        };
        let mut reply = Reply {
            text: "Trying the naïve way first.".to_owned(),
            ..Reply::default()
        };
        reply.push_code("print('déjà", &mut collect);
        let unread = Unread {
            name: TOOL,
            arguments: r#"{"code": "print('déjà"#,
        };
        reply.end_call(Err(unread), &mut collect);
        reply.push_code("\n", &mut collect);
        reply.push_code("  ", &mut collect);
        reply.push_code("print('café')\n", &mut collect);
        reply.end_call(Ok(()), &mut collect);

        let written: Vec<&str> = reply.calls.iter().map(|call| call.code.as_str()).collect();
        assert_eq!(written, ["print('déjà", "\n  print('café')\n"]);
        let at: Vec<usize> = reply.calls.iter().map(|call| call.at).collect();
        let before = "Trying the naïve way first.".len();
        assert_eq!(at, [before, reply.text.len()]);
        let shown = [
            (0, "print('déjà".to_owned()),
            (1, "\n  print('café')\n".to_owned()),
        ];
        assert_eq!(events, shown);
    }

    /// A cutoff right as the second call starts leaves the fully received first call incomplete too
    #[test]
    fn cut_after_call() {
        let cut = Script {
            calls: vec![Ok("print('alpha')".to_owned()), Ok(String::new())],
            finish: Finish::Length,
            ..reply("")
        };
        let mut agent = agent(vec![cut, reply("Shortened it.")]);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        assert_eq!(step.error, Some(StepError::Cut));
        assert!(printed(step).is_none());
    }

    #[test]
    fn empty_reply_retried() {
        let replies = vec![Ok(reply("")), Ok(reply(" \n")), Ok(reply("Hello."))];
        let (mut retried, ends) = recording(replies);

        let mut memory = AgentMemory::default();
        let answer = retried.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, Some("Hello.".to_owned()));

        let step = only(&memory);
        assert_eq!(step.error, None);
        assert_eq!(*ends.lock().expect("ends lock"), [true, true, false]);

        let silent = vec![reply(""), reply(""), reply(""), reply("")];
        let mut silent = agent(silent);
        let mut memory = AgentMemory::default();
        let error = silent.turn(&mut memory, "task").expect_err("gives up");
        assert!(error.to_string().contains("empty reply"), "{error}");
        assert!(actions(&memory).is_empty());

        let thinking = Script {
            reasoning: Some("I only think and never write a reply".to_owned()),
            ..reply("")
        };
        let replies = vec![Ok(thinking); 4];
        let model = ScriptedModel::new(replies);
        let mut thoughtful = scripted(model, Box::new(|_| {}));
        let mut memory = AgentMemory::default();
        let error = thoughtful.turn(&mut memory, "task").expect_err("gives up");
        let error = error.to_string();
        let named = "stopped after reasoning without replying";
        assert!(error.contains(named), "{error}");
    }

    #[test]
    fn transient_retried() {
        let answered = || Ok(reply("1"));
        let busy = || ModelError::transient("http 503: service busy");
        let replies = vec![Err(busy()), Err(busy()), answered()];
        let (mut flaky, ends) = recording(replies);

        let mut memory = AgentMemory::default();
        let answer = flaky.turn(&mut memory, "Add one to x").expect("run");
        assert_eq!(answer, Some("1".to_owned()));
        assert_eq!(actions(&memory).len(), 1);
        assert_eq!(*ends.lock().expect("ends lock"), [true, true, false]);

        let refused = ModelError::new("http 400: bad request");
        let (mut agent, ends) = recording(vec![Err(refused), answered()]);
        let mut memory = AgentMemory::default();
        let ran = agent.turn(&mut memory, "Add one to x");
        assert!(ran.is_err());
        assert_eq!(*ends.lock().expect("ends lock"), [false]);
    }

    #[test]
    fn cut_reply_unused() {
        let cut = Script {
            calls: vec![Ok("print('half".to_owned())],
            finish: Finish::Length,
            ..reply("Starting.\n")
        };
        let replies = vec![Ok(cut), Ok(reply("Short."))];
        let mut run = recorded(replies, None, "", Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = run.agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, Some("Short.".to_owned()));

        let step = actions(&memory)[0];
        assert_eq!(step.error, Some(StepError::Cut));
        assert_eq!(step.text, "Starting.");
        assert_eq!(codes(step), ["print('half"]);
        assert!(step.dropped() && printed(step).is_none());
        let told = prompt::error(step).unwrap_or_default();
        assert!(told.contains("hit the output length limit"), "{told}");

        let requests = run.requests.lock().expect("requests");
        let next: Vec<String> = requests[1][1..].iter().map(content).collect();
        assert_eq!(next, ["New task:\ntask".to_owned(), told]);
    }

    #[test]
    fn provider_stop_fails() {
        let filtered = Script {
            finish: Finish::Other("content_filter".to_owned()),
            ..reply("")
        };
        let mut agent = agent(vec![filtered]);

        let mut memory = AgentMemory::default();
        let error = agent.turn(&mut memory, "Write verse").expect_err("stopped");
        assert!(error.to_string().contains("content_filter"), "{error}");
    }

    #[test]
    fn updates_join_turn() {
        let mut agent = running(&["print(1)"]);
        let update = Task {
            text: "Reply in French".to_owned(),
            mentions: Vec::new(),
        };
        let mut pending = vec![update];
        let take = move || mem::take(&mut pending);
        *agent.queue.updates.lock().expect("updates") = Some(Box::new(take));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Print 1").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let conversation = memory.conversation("", &|_| true, None).into_iter().skip(1);
        let contents: Vec<String> = conversation.map(|message| content(&message)).collect();
        let expected = [
            "New task:\nPrint 1",
            "eval:\nprint(1)",
            "1",
            "Task update:\nReply in French",
            "42",
        ];
        assert_eq!(contents, expected);
    }

    #[test]
    fn tool_calls_run() {
        let call = |content: &str, code: &str| Script {
            calls: vec![Ok(code.to_owned())],
            ..reply(content)
        };
        let unread = || Script {
            calls: vec![Err((TOOL, "{}"))],
            ..reply("")
        };
        let tagged = || call("", "<eval>");
        let code = "résumé = '''\n</eval>\n'''\nx = 6 * 7\nprint(x)";
        let replies = vec![
            call("Let me work it out.", code),
            call("", "print('again')"),
            unread(),
            tagged(),
            tagged(),
            unread(),
            tagged(),
            unread(),
            unread(),
            call("", ""),
        ];
        let mut agent = agent(replies);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, Some("42".to_owned()));

        let steps = actions(&memory);
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[0].text, "Let me work it out.");
        assert_eq!(codes(steps[0]), [code]);
        assert_eq!(printed(steps[0]), Some("42"));
        assert_eq!(codes(steps[1]), ["print('again')"]);
        for step in &steps[2..4] {
            assert_eq!(step.error, None);
            assert_eq!(step.calls[0].error, Some(CallError::NoCode));
            assert!(printed(step).is_none());
        }
        assert_eq!(steps[2].calls[0].arguments.as_deref(), Some("{}"));
    }

    #[test]
    fn empty_thought_kept() {
        let thoughts = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&thoughts);
        let on_event = Box::new(move |event: Event| {
            if let Event::Thought(_) = event {
                *counted.lock().expect("thoughts lock") += 1;
            }
        });
        let silent = Script {
            reasoning: Some(String::new()),
            ..called("print('one')")
        };
        let mut run = recorded(vec![Ok(silent)], None, "", on_event);
        let mut memory = AgentMemory::default();
        run.agent.turn(&mut memory, "task").expect("run");

        let requests = run.requests.lock().expect("requests");
        let ChatMessage::Assistant {
            reasoning, calls, ..
        } = &requests[1][2]
        else {
            panic!("{:?}", requests[1]);
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(reasoning.as_deref(), Some(""));
        assert_eq!(*thoughts.lock().expect("thoughts lock"), 0);
    }

    #[test]
    fn repaired_code_saved() {
        let written = "<parameter name=\"code\">\nprint('naïve')\n</parameter>";
        let tagged = Script {
            calls: vec![Ok(written.to_owned())],
            ..reply("Running it first.")
        };
        let mut agent = agent(vec![tagged]);

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");
        let step = actions(&memory)[0];
        assert_eq!(step.text, "Running it first.");
        assert_eq!(codes(step), ["print('naïve')"]);
        assert_eq!(printed(step), Some("naïve"));
        assert_eq!(step.calls[0].unrepaired.as_deref(), Some(written));

        // The original stays in the record and is never sent to the model
        let action = MemoryStep::Action(Box::new(step.clone()));
        let sent: Vec<String> = action.messages(&mut 0).iter().map(content).collect();
        assert!(
            sent.iter().all(|text| !text.contains("</parameter>")),
            "{sent:?}"
        );
    }

    /// A call with no readable code is shown on arrival and stays out of the text
    #[test]
    fn unknown_tool_shown() {
        let mut shown = Vec::new();
        let mut collect = |event: Event| {
            if let Event::Unread(index, call) = event {
                shown.push((index, call.clone()));
            }
        };
        let mut reply = Reply {
            text: "Listing the folder first.".to_owned(),
            ..Reply::default()
        };
        let arguments = r#"{"command": "ls docs"}"#;
        let unread = Unread {
            name: "bash",
            arguments,
        };
        reply.end_call(Err(unread), &mut collect);

        assert_eq!(reply.text, "Listing the folder first.");
        let call = Call {
            at: reply.text.len(),
            arguments: Some(arguments.to_owned()),
            error: Some(CallError::NoTool("bash".to_owned())),
            ..Call::default()
        };
        assert_eq!(shown, [(0, call.clone())]);
        assert_eq!(reply.calls, [call]);
    }

    #[test]
    fn update_beats_cancel() {
        let mut agent = running(&["print(1)"]);
        let interrupt = agent.agent.interrupt.clone();
        let take = move || {
            interrupt.cancel(u64::MAX);
            let update = Task {
                text: "One more thing".to_owned(),
                mentions: Vec::new(),
            };
            vec![update]
        };
        *agent.queue.updates.lock().expect("updates") = Some(Box::new(take));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, None);

        let update = |step: &MemoryStep| matches!(step, MemoryStep::Update(_));
        assert!(memory.steps.iter().any(update), "{:?}", memory.steps);
    }

    #[test]
    fn printed_capped() {
        let code = "text = '\\n'.join(f'Zoë {i}' for i in range(1, 100001))\n\
            print(len(text))\nprint(text, limit=None)";
        let mut agent = running(&[code]);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        let printed = printed(step).expect("printed");
        let whole = printed.starts_with("988894\nZoë 1\nZoë 2\n");
        assert!(whole);
        assert!(printed.contains("\n[972517 characters cut]\n"), "{printed}");
        assert!(printed.chars().count() < MAX_OBSERVATION_CHARS + 100);
    }

    #[test]
    fn error_shares_budget() {
        let code = "print('café' * 10000, limit=None)\nraise ValueError('über' * 10000)";
        let mut agent = running(&[code]);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        let Some(CallError::Execution(error)) = &step.calls[0].error else {
            panic!("{:?}", step.calls);
        };
        let printed = printed(step).expect("printed");
        let error = error.chars().count();
        let printed = printed.chars().count();
        assert!(error <= MAX_OBSERVATION_CHARS / 2 + 100, "{error}");
        assert!(
            error + printed <= MAX_OBSERVATION_CHARS + 100,
            "{error} + {printed}"
        );

        let kept = MAX_OBSERVATION_CHARS - error;
        let cut = prompt::cut(40_000 - kept);
        let marker = format!("\n{cut}\n");
        assert_eq!(printed, kept + marker.chars().count(), "{error}");
    }

    #[test]
    fn failed_code_logs() {
        let mut agent = running(&["print('partial')\n1 / 0"]);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        let step = actions(&memory)[0];
        assert_eq!(printed(step), Some("partial"));
        assert!(step.usage.decode_duration.is_some());
        assert_eq!(step.error, None);

        let error = step.calls[0].error.as_ref().expect("error").to_string();
        assert!(error.contains("line 2: 1 / 0"), "{error}");
    }

    #[test]
    fn generation_failure_ends() {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&errors);
        let on_event = Box::new(move |event: Event| {
            if let Event::StepEnd(step) = event {
                seen.lock().expect("errors lock").push(step.error.clone());
            }
        });
        let replies = vec![Err(ModelError::new("model went offline"))];
        let model = ScriptedModel::new(replies);
        let mut agent = scripted(model, on_event);

        let mut memory = AgentMemory::default();
        let error = agent.turn(&mut memory, "task").expect_err("error");
        assert!(error.to_string().contains("model went offline"), "{error}");
        assert!(actions(&memory).is_empty());

        let errors = errors.lock().expect("errors lock");
        assert!(matches!(errors[..], [Some(StepError::Model(_))]));
    }

    #[test]
    fn interrupt_generation() {
        let handle = Arc::new(OnceLock::<Interrupt>::new());
        let trigger = Arc::clone(&handle);
        let thinking = Script {
            calls: vec![Ok("print(1)".to_owned())],
            ..reply("Let me think.\n")
        };
        let on_event = Box::new(move |event: Event| {
            if let (Event::Text(_), Some(interrupt)) = (event, trigger.get()) {
                interrupt.cancel(u64::MAX);
            }
        });
        let model = ScriptedModel::new(vec![Ok(thinking)]);
        let mut agent = scripted(model, on_event);
        let interrupt = agent.agent.interrupt.clone();
        handle.set(interrupt).expect("set");

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, None);

        let step = interrupted(&memory);
        assert_eq!(step.text, "Let me think.");
        assert!(step.calls.is_empty());
    }

    /// Code interrupted while it is still only leading whitespace does not count as unreadable
    #[test]
    fn blank_call_interrupted() {
        let handle = Arc::new(OnceLock::<Interrupt>::new());
        let trigger = Arc::clone(&handle);
        let thinking = Script {
            reasoning: Some("Thinking first".to_owned()),
            ..called("\nprint('unfinished')")
        };
        let on_event = Box::new(move |event: Event| {
            if let (Event::Thought(_), Some(interrupt)) = (event, trigger.get()) {
                interrupt.cancel(u64::MAX);
            }
        });
        let model = ScriptedModel::new(vec![Ok(thinking)]);
        let mut agent = scripted(model, on_event);
        let interrupt = agent.agent.interrupt.clone();
        handle.set(interrupt).expect("set");

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, None);

        let step = interrupted(&memory);
        assert_eq!(codes(step), [""]);
        assert_eq!(step.calls[0].error, None);
        let told = prompt::call_error(step, &step.calls[0]).unwrap_or_default();
        assert_eq!(told, "Error:\ninterrupted by the user before the code ran");
    }

    /// An interrupt while the worker restarts tells the model the code never ran
    #[test]
    fn interrupted_before_run() {
        let handle = Arc::new(OnceLock::<Interrupt>::new());
        let trigger = Arc::clone(&handle);
        let mut executed = 0;
        let on_event = Box::new(move |event: Event| {
            if let (Event::Execute(..), Some(interrupt)) = (event, trigger.get()) {
                executed += 1;
                if executed == 2 {
                    interrupt.cancel(u64::MAX);
                }
            }
        });
        let replies = vec![
            Ok(called("import os\nos._exit(0)")),
            Ok(called("print('never ran')")),
        ];
        let model = ScriptedModel::new(replies);
        let mut agent = scripted(model, on_event);
        let interrupt = agent.agent.interrupt.clone();
        handle.set(interrupt).expect("set");

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "task").expect("run");
        assert_eq!(answer, None);

        let steps = actions(&memory);
        let last = steps.last().expect("step");
        assert_eq!(last.error, Some(StepError::Interrupted));
        assert!(printed(last).is_none());
        let told = prompt::call_error(last, &last.calls[0]).unwrap_or_default();
        assert_eq!(told, "Error:\ninterrupted by the user before the code ran");
    }

    #[test]
    fn interrupt_code() {
        let id = new_id();
        let name = format!("smol-started-{id}");
        let code = format!("print('started')\nopen({name:?}, 'w').close()\nwhile True:\n    pass");
        let mut agent = running(&[code.as_str()]);
        let started = Instant::now();
        interrupt_started(&agent, &name);

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "task").expect("run");

        assert!(started.elapsed() < Duration::from_secs(30));
        let step = interrupted(&memory);
        assert_eq!(printed(step), Some("started"));
        assert_eq!(step.calls[0].error, Some(CallError::Interrupted));
    }
}
