use crate::agent::compact::squeezed;
use crate::agent::{retry, text_reply, Event, Turn};
use crate::memory::{estimate, text_bytes, AgentStep, Check, MemoryStep, StepError, Verdict};
use crate::model::{ChatMessage, ModelError, Purpose};
use crate::prompt::{self, BLOCKED, CHECKER, MET, UNMET};
use crate::utils::excerpt;
use std::time::Instant;

const UNREADABLE: &str = "check reply: no verdict";

impl Turn<'_> {
    /// `step` is the answering step, not yet added to memory
    pub(crate) async fn check(&mut self, step: &AgentStep) -> Result<Check, StepError> {
        let model = &*self.agent.model;
        let window = model.context_window();
        let visible = self.memory.visible();
        let earlier = visible.summarized(window);
        let answered = MemoryStep::Action(Box::new(step.clone()));
        let mut steps = visible.steps;
        steps.push(&answered);

        // The summary and the task text are in the request too
        let taken = earlier.as_deref().map_or(0, text_bytes);
        let taken = estimate(taken);
        // Compaction only runs before steps, so the check after the answer must also fit the window
        let transcript = match window {
            Some(window) => {
                let room = window.saturating_sub(taken);
                squeezed(&steps, room)
            }
            None => prompt::transcript(&steps, usize::MAX),
        };
        let asked = prompt::check(&transcript, earlier.as_deref());
        let request = [
            ChatMessage::System(CHECKER.to_owned()),
            ChatMessage::user(asked),
        ];

        self.sink.event(Event::Check);
        let interrupt = &self.agent.interrupt;
        let mut tries = 0;
        let started = Instant::now();
        loop {
            let replying = text_reply(model, interrupt, &request, Purpose::Check);
            let (replied, mut usage) = replying.await;
            let error = match replied {
                Ok(text) => {
                    if let Some((verdict, reason)) = parsed(&text) {
                        // Time spent waiting to retry counts too
                        usage.duration = started.elapsed().as_secs_f64();
                        let check = Check {
                            verdict,
                            reason,
                            usage,
                        };
                        self.sink.event(Event::CheckEnd(Ok(&check)));
                        return Ok(check);
                    }
                    let said = excerpt(&text);
                    let message = format!("{UNREADABLE}: {said}");
                    let unreadable = ModelError::transient(message);
                    StepError::Model(unreadable)
                }
                Err(error) => error,
            };
            if retry(&mut tries, &error, interrupt, &mut *self.sink).await {
                continue;
            }

            self.sink.event(Event::CheckEnd(Err(&error)));
            return Err(error);
        }
    }
}

fn parsed(reply: &str) -> Option<(Verdict, String)> {
    let line = reply.lines().map(str::trim).find(|line| !line.is_empty())?;
    let forms = [
        (UNMET, Verdict::Unmet),
        (BLOCKED, Verdict::Blocked),
        (MET, Verdict::Met),
    ];

    for (word, verdict) in forms {
        let head = line.get(..word.len());
        if !head.is_some_and(|head| head.eq_ignore_ascii_case(word)) {
            continue;
        }
        let rest = &line[word.len()..];
        if rest.starts_with(char::is_alphanumeric) {
            continue;
        }

        let reason = rest.trim_start_matches([':', '：']).trim();
        if verdict != Verdict::Met && reason.is_empty() {
            return None;
        }
        return Some((verdict, reason.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::agent::check::*;
    use crate::agent::compact::MAX_TRANSCRIPT_SHARE;
    use crate::agent::tests::{called, recorded, reply, Run, Script};
    use crate::memory::tests::asked;
    use crate::memory::{AgentMemory, GoalStatus, SummaryStep, Unchecked};
    use crate::tests::content;
    use crate::Result;

    fn agent(replies: &[&str]) -> Run {
        let replies = replies.iter().map(|text| reply(text));
        windowed(replies.collect(), None)
    }

    fn windowed(replies: Vec<Script>, window: Option<u64>) -> Run {
        let replies = replies.into_iter().map(Ok);
        recorded(replies.collect(), window, CHECKER, Box::new(|_| {}))
    }

    fn pursue(run: &mut Run, memory: &mut AgentMemory, goal: &str) -> Result<Option<String>> {
        run.agent.pursue(memory, goal, &[], true)
    }

    fn checks(memory: &AgentMemory) -> Vec<Option<&Check>> {
        let mut checks = Vec::new();
        for step in &memory.steps {
            if let MemoryStep::Action(step) = step {
                checks.push(step.check.as_ref());
            }
        }
        checks
    }

    fn verdicts(memory: &AgentMemory) -> Vec<Option<Verdict>> {
        let mut verdicts = Vec::new();
        for check in checks(memory) {
            verdicts.push(check.map(|check| check.verdict));
        }
        verdicts
    }

    fn unchecked(memory: &AgentMemory) -> Option<&Unchecked> {
        let Some(MemoryStep::Action(step)) = memory.steps.last() else {
            panic!("{:?}", memory.steps);
        };
        step.unchecked.as_ref()
    }

    #[test]
    fn unmet_continues() {
        let replies = vec![
            reply("Fixed it."),
            reply("not met: run the tests"),
            called("print('tests passed')"),
            reply("All tests pass."),
            reply("met"),
        ];
        let mut run = windowed(replies, None);
        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Fix parsing").expect("run");
        assert_eq!(answer.as_deref(), Some("All tests pass."));

        let expected = [Some(Verdict::Unmet), None, Some(Verdict::Met)];
        assert_eq!(verdicts(&memory), expected);

        let conversation = memory.conversation("", None);
        let contents: Vec<String> = conversation.iter().map(content).collect();
        let fed = prompt::unmet("run the tests");
        assert_eq!(
            contents[1..4],
            ["New task:\nFix parsing", "Fixed it.", fed.as_str()]
        );

        let requests = run.requests.lock().expect("requests");
        let [system, asked] = &requests[1][..] else {
            panic!("{:?}", requests[1]);
        };
        assert_eq!(content(system), CHECKER);
        let start = "<conversation>\n[Task]: Fix parsing\n\n[Agent]: Fixed it.\n</conversation>";
        let asked = content(asked);
        assert!(asked.starts_with(start), "{asked}");
        let last = content(&requests[4][1]);
        assert!(last.contains("[Check]: not met: run the tests"), "{last}");
    }

    #[test]
    fn blocked_ends() {
        let mut run = agent(&["I need the DB password.", "blocked: the DB password"]);
        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Migrate the data").expect("run");
        assert_eq!(answer.as_deref(), Some("I need the DB password."));

        let check = checks(&memory)[0].expect("check");
        assert_eq!(check.verdict, Verdict::Blocked);
        assert_eq!(check.reason, "the DB password");

        let conversation = memory.conversation("", None);
        let last = conversation.last().map(content);
        assert_eq!(last, Some(prompt::blocked("the DB password")));
    }

    #[test]
    fn stalled_stops() {
        let mut run = agent(&[
            "The certificate is missing.",
            "not met: import the certificate",
            "Still no certificate.",
            "not met: import the certificate",
        ]);
        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Import the cert").expect("run");
        assert_eq!(answer.as_deref(), Some("Still no certificate."));

        let unmet = Some(Verdict::Unmet);
        assert_eq!(verdicts(&memory), [unmet, unmet]);
        assert_eq!(run.requests.lock().expect("requests").len(), 4);
        let goal = memory.goal().expect("goal");
        assert_eq!(goal.status, GoalStatus::Paused);
    }

    #[test]
    fn unreadable_fails() {
        let mut run = agent(&["Done.", "Sure", "Sure", "Sure", "Sure, I had a look"]);
        let mut memory = AgentMemory::default();
        let error = pursue(&mut run, &mut memory, "Tidy up").expect_err("unreadable");
        let error = error.to_string();
        let shown = format!("model: {UNREADABLE}: Sure, I had a look");
        assert_eq!(error, shown);

        assert_eq!(run.requests.lock().expect("requests").len(), 1 + 4);
        assert_eq!(checks(&memory), [None]);
        let failed = Unchecked::Failed(shown);
        assert_eq!(unchecked(&memory), Some(&failed));
    }

    #[test]
    fn late_update_checked() {
        let replies = vec![
            reply("Done."),
            called("print('tests added')"),
            reply("Tests are added too."),
            reply("met"),
        ];
        let mut run = windowed(replies, None);
        let mut given = vec![vec![("Add tests too".to_owned(), Vec::new())]];
        run.agent.probe.updates = Box::new(move || given.pop().unwrap_or_default());

        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Fix the parser").expect("run");
        assert_eq!(answer.as_deref(), Some("Tests are added too."));

        assert_eq!(verdicts(&memory), [None, None, Some(Verdict::Met)]);
        let requests = run.requests.lock().expect("requests");
        let asked = content(&requests[3][1]);
        assert!(asked.contains("[Task update]: Add tests too"), "{asked}");
    }

    #[test]
    fn checked_update_continues() {
        let replies = vec![
            reply("I need the DB password."),
            reply("blocked: the DB password"),
            called("print('connected')"),
            reply("Migration done."),
            reply("met"),
        ];
        let mut run = windowed(replies, None);
        // The first is taken when the answer is done and the second arrives during the check
        let mut given = vec![vec![("It is in .env".to_owned(), Vec::new())], Vec::new()];
        run.agent.probe.updates = Box::new(move || given.pop().unwrap_or_default());

        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Migrate the data").expect("run");
        assert_eq!(answer.as_deref(), Some("Migration done."));

        let expected = [Some(Verdict::Blocked), None, Some(Verdict::Met)];
        assert_eq!(verdicts(&memory), expected);
        let mut updates = Vec::new();
        for step in &memory.steps {
            if let MemoryStep::Update(update) = step {
                updates.push(update.text.as_str());
            }
        }
        assert_eq!(updates, ["It is in .env"]);
    }

    #[test]
    fn update_kept_interrupted() {
        let mut run = agent(&["Done."]);
        let interrupt = run.agent.agent.interrupt.clone();
        run.agent.probe.updates = Box::new(move || {
            interrupt.cancel(0);
            vec![("Use the new API".to_owned(), Vec::new())]
        });

        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Fix the parser").expect("run");
        assert_eq!(answer, None);
        let Some(MemoryStep::Update(update)) = memory.steps.last() else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(update.text, "Use the new API");
    }

    #[test]
    fn check_fits_window() {
        // Dense filler lets the plain head make up for the cut marker
        let long = format!("Printed it.{}", "🍮".repeat(7500));
        let replies = vec![reply(&long), reply("met")];
        let mut run = windowed(replies, Some(4000));
        let earlier = SummaryStep {
            summary: "Tasted the crème brûlée. ".repeat(160),
            ..SummaryStep::default()
        };
        let mut memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(asked("Taste the menu", false)),
                MemoryStep::Summary(earlier),
            ],
        };
        let answer = pursue(&mut run, &mut memory, "Print").expect("run");
        assert_eq!(answer, Some(long));

        let requests = run.requests.lock().expect("requests");
        let mut size = 0;
        for message in &requests[1] {
            size += text_bytes(&content(message));
        }
        assert!(estimate(size) <= 4000, "{size}");
        let asked = content(&requests[1][1]);
        assert!(asked.contains("[Task]: Print"), "{asked}");
        assert!(asked.contains("Printed it."), "{asked}");

        let (summary, rest) = asked.split_once("\n\n<conversation>\n").expect("split");
        let (transcript, _) = rest.split_once("\n</conversation>").expect("split");
        assert!(summary.contains("Tasted the crème brûlée."), "{summary}");
        let taken = text_bytes(summary);
        let taken = estimate(taken);
        let budget = ((4000 - taken) as f64 * MAX_TRANSCRIPT_SHARE) as u64;
        let size = text_bytes(transcript);
        assert!(estimate(size) <= budget, "{size} {taken}");
    }

    #[test]
    fn interrupted_check() {
        let mut run = agent(&["Done.", "met"]);
        let interrupt = run.agent.agent.interrupt.clone();
        *run.interrupt.lock().expect("interrupt") = Some(interrupt);

        let mut memory = AgentMemory::default();
        let answer = pursue(&mut run, &mut memory, "Tidy up").expect("run");
        assert_eq!(answer, None);
        assert_eq!(checks(&memory), [None]);
        assert_eq!(unchecked(&memory), Some(&Unchecked::Interrupted));
    }

    #[test]
    fn verdicts_parsed() {
        let unmet = parsed("not met: 2 tests still fail");
        let reason = "2 tests still fail".to_owned();
        assert_eq!(unmet, Some((Verdict::Unmet, reason)));

        let blocked = parsed("\n  Blocked: the database password\nAn extra line");
        let reason = "the database password".to_owned();
        assert_eq!(blocked, Some((Verdict::Blocked, reason)));

        assert_eq!(parsed("MET"), Some((Verdict::Met, String::new())));

        let wide = parsed("not met：tests are missing");
        let reason = "tests are missing".to_owned();
        assert_eq!(wide, Some((Verdict::Unmet, reason)));

        for reply in [
            "metadata is fine",
            "not met",
            "blocked:  ",
            "Résumé done",
            "",
            "**met**",
        ] {
            assert_eq!(parsed(reply), None, "{reply}");
        }
    }
}
