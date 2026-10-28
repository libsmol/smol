use crate::agent::{logged_tokens, Agent, HelperView, Helpers, Place, Task};
use crate::memory::{AgentMemory, Ending, Helper};
use crate::prompt;
use crate::worker::host::{Added, Called, Calling, Delegating, Worker};
use serde_json::{json, Value};
use std::future;
use std::sync::Arc;
use std::time::Instant;
use tokio::select;
use tokio::task::JoinSet;
use tracing::{error_span, info, warn, Instrument, Span};

pub(crate) const MAX_PARALLEL_AGENTS: usize = 8;

impl Agent {
    /// Runs the tasks of each `agent()` call made by the code of one call,
    /// and numbers the agents it starts on across those calls
    pub(crate) fn delegate(&self, servers: &[&Added], views: Box<dyn Helpers>) -> Box<Delegating> {
        let Place::Main(defined) = &self.place else {
            let refused = |_: Vec<String>, _: Vec<Option<String>>| -> Calling {
                let reply = Err(prompt::NESTED.to_owned());
                let called = Called::from(reply);
                Box::pin(future::ready(called))
            };
            return Box::new(refused);
        };

        let defined = defined.clone();
        let plain = Agent {
            place: Place::Delegated(None),
            ..self.clone()
        };
        let servers: Vec<Added> = servers.iter().map(|added| (*added).clone()).collect();
        let views: Arc<dyn Helpers> = Arc::from(views);
        // Spawned tasks don't inherit the session's span, which puts its id on log lines
        let span = Span::current();
        let mut started = 0;
        let delegate = move |tasks: Vec<String>, roles: Vec<Option<String>>| -> Calling {
            let mut assigned = Vec::new();
            for (task, role) in tasks.into_iter().zip(roles) {
                let mut helper = plain.clone();
                if let Some(name) = role {
                    let Some(role) = defined.iter().find(|role| role.name == name) else {
                        let reply = Err(prompt::unknown_role(&name, &defined));
                        let called = Called::from(reply);
                        return Box::pin(future::ready(called));
                    };
                    if let Some(model) = &role.model {
                        helper.model = Arc::clone(model);
                    }
                    helper.place = Place::Delegated(Some(role.clone()));
                }
                assigned.push((task, helper));
            }

            let first = started + 1;
            started += assigned.len();
            let helping = Self::helped(assigned, first, servers.clone(), Arc::clone(&views));
            Box::pin(helping.instrument(span.clone()))
        };
        Box::new(delegate)
    }

    /// Answers each task in order, as `{"answer"}` or `{"error"}`, by the agent assigned to it
    async fn helped(
        assigned: Vec<(String, Agent)>,
        first: usize,
        servers: Vec<Added>,
        views: Arc<dyn Helpers>,
    ) -> Called {
        // Views open in task order before any agent waits for a free slot, the order replay uses
        let mut running = JoinSet::new();
        let mut agents = Vec::new();
        for (index, (task, helper)) in assigned.into_iter().enumerate() {
            let number = first + index;
            let role = helper.place.role().map(|role| role.name.clone());
            let view = views.open(number, &task, role.as_deref());
            let span = error_span!("", agent = number);
            let helping = helper.help(task.clone(), view, servers.clone());
            let numbered = async move { (index, helping.await) }.instrument(span);
            running.spawn(numbered);

            // A task that panicked keeps this record, which says what its dropped view showed
            let end = Ending::Error(prompt::UNFINISHED.to_owned());
            let memory = AgentMemory::default();
            agents.push(Helper {
                task,
                role,
                end,
                memory,
            });
        }
        while let Some(joined) = running.join_next().await {
            if let Ok((index, helper)) = joined {
                agents[index] = helper;
            }
        }

        let mut results = Vec::new();
        for helper in &agents {
            let result = match &helper.end {
                Ending::Answer(answer) => json!({ "answer": answer }),
                Ending::Error(error) => json!({ "error": error }),
                Ending::Interrupted {} => json!({ "error": prompt::STOPPED }),
            };
            results.push(result);
        }

        Called {
            result: Ok(Value::Array(results)),
            media: Vec::new(),
            agents,
        }
    }

    async fn help(
        self,
        task: String,
        mut view: Box<dyn HelperView>,
        servers: Vec<Added>,
    ) -> Helper {
        let started = Instant::now();
        let role = self.place.role().map(|role| role.name.clone());
        let mut memory = AgentMemory::default();
        // One still waiting for a slot stops on an interrupt without starting a worker
        let answered = select! {
            biased;
            () = self.interrupt.wait() => Ok(None),
            slot = self.slots.acquire() => {
                let _slot = slot.expect("slots");
                match &role {
                    Some(role) => info!("agent: started [role={role}]"),
                    None => info!("agent: started"),
                }
                let has_roles = false;
                let restarted = false;
                let sandbox = Arc::clone(&self.sandbox);
                let worker = Worker::new(&self.interrupt, sandbox, has_roles, restarted).await;
                match worker {
                    Ok(mut worker) => {
                        let installed: Vec<&Added> = servers.iter().collect();
                        let asked = Task {
                            text: task.clone(),
                            mentions: Vec::new(),
                        };
                        let view = &mut *view;
                        let running = self.run(&mut worker, &installed, &mut memory, asked, view);
                        running.await
                    }
                    Err(error) => Err(error),
                }
            }
        };

        let (usage, steps) = memory.spent();
        let seconds = started.elapsed().as_secs_f64();
        let tokens = logged_tokens(&usage);
        let counted = format!("[steps={steps}, tokens={tokens}, elapsed={seconds:.1}s]");
        let end = match answered {
            Ok(Some(answer)) => {
                info!("agent: answered {counted}");
                Ending::Answer(answer)
            }
            Ok(None) => {
                info!("agent: interrupted {counted}");
                Ending::Interrupted {}
            }
            Err(error) => {
                warn!("agent: failed, {error} {counted}");
                Ending::Error(error.to_string())
            }
        };

        let helper = Helper {
            task,
            role,
            end,
            memory,
        };
        view.close(&helper);
        helper
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tests::{scripted, with_roles};
    use crate::agent::{Unseen, MAX_PARALLEL_AGENTS};
    use crate::config::Role;
    use crate::memory::{AgentMemory, Ending, MemoryStep};
    use crate::model::{
        ChatMessage, Finish, Finished, Generating, Model, Purpose, StreamDelta, TokenUsage,
    };
    use crate::prompt;
    use crate::tests::content;
    use crate::utils::block_on;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use tokio::time;

    type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

    /// The main agent starts four agents, which answer with their task, except one that crashes
    /// and one that tries to start an agent itself and answers with what happened
    struct Teamed(Requests);

    impl Model for Teamed {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            _: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            self.0.lock().expect("requests").push(conversation.to_vec());
            let system = content(&conversation[0]);
            let asked = content(&conversation[1]);
            let task = asked.strip_prefix("New task:\n").unwrap_or_default();
            let last = conversation.last().map(content).unwrap_or_default();
            let first = conversation.len() == 2;
            let (text, code) = match (system.contains("goes back to the agent"), first) {
                (true, true) if task == "Start another agent" => {
                    (String::new(), Some("print(agent('deeper'))"))
                }
                (true, true) if task == "Crash" => panic!("{task}"),
                (true, true) => (format!("Answer: {task}"), None),
                (true, false) => (last, None),
                (false, true) => {
                    let code = "print(agent(['Count the cafés', 'Crash', 'Spell naïve', 'Start another agent']))";
                    (String::new(), Some(code))
                }
                (false, false) => ("All three answered.".to_owned(), None),
            };

            Box::pin(async move {
                match code {
                    Some(code) => {
                        on_delta(StreamDelta::CallCode(code));
                        on_delta(StreamDelta::CallEnd(Ok(())));
                    }
                    None => on_delta(StreamDelta::Content(&text)),
                }
                let usage = Some(TokenUsage::new(10, 5));
                Ok(Finished {
                    finish: Finish::Stop,
                    usage,
                })
            })
        }
    }

    #[test]
    fn helpers_answer() {
        let requests = Requests::default();
        let model = Teamed(Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Split the work").expect("run");
        assert_eq!(answer.as_deref(), Some("All three answered."));

        let MemoryStep::Action(step) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        let result = step.calls[0].result.as_ref().expect("ran");
        let nested = "Error:\nValueError: agent: not available to agents started by agent()\n  line 1: print(agent('deeper'))";
        let crashed = "RuntimeError('agent: stopped before it answered')";
        let printed =
            format!("['Answer: Count the cafés', {crashed}, 'Answer: Spell naïve', {nested:?}]");
        assert_eq!(result.printed.as_deref(), Some(printed.as_str()));
        let tasks: Vec<&str> = result.agents.iter().map(|helper| &*helper.task).collect();
        let asked = [
            "Count the cafés",
            "Crash",
            "Spell naïve",
            "Start another agent",
        ];
        assert_eq!(tasks, asked);
        // The crashed agent keeps the record its dropped view showed in its place
        let unfinished = Ending::Error(prompt::UNFINISHED.to_owned());
        assert_eq!(result.agents[1].end, unfinished);
        for index in [0, 2, 3] {
            let helper = &result.agents[index];
            assert!(matches!(helper.end, Ending::Answer(_)), "{helper:?}");
            let (usage, _) = helper.memory.spent();
            assert!(usage.tokens.is_some(), "{helper:?}");
        }

        // Agents started by agent() don't see it or task updates, and answer the code
        let requests = requests.lock().expect("requests");
        let main = content(&requests[0][0]);
        let listed =
            "\n- agent(task: str | list[str]) -> str | list[str | RuntimeError]  # a new agent";
        assert!(main.contains(listed), "{main}");
        assert!(main.contains("Task update"), "{main}");
        let helped = requests.iter().map(|request| content(&request[0]));
        let helper = helped
            .into_iter()
            .find(|text| *text != main)
            .expect("helper");
        assert!(!helper.contains("- agent("), "{helper}");
        assert!(!helper.contains("Task update"), "{helper}");
        assert!(
            helper.contains(
                "\n\n# Answer\n\nOnly your answer goes back to the agent that gave you this task"
            ),
            "{helper}"
        );
    }

    /// The main agent gives tasks to roles, and agents answer with their role and model
    struct Cast(&'static str);

    impl Model for Cast {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            _: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            let system = content(&conversation[0]);
            let own = system.split("# Role\n\n").nth(1);
            let own = own.and_then(|rest| rest.lines().next());
            let main = !system.contains("goes back to the agent");
            let first = conversation.len() == 2;
            let (text, code) = match (main, first) {
                (true, true) => {
                    let code = "print(agent('Check café.py', role='reviewer'))\n\
                        print(agent(['Check café.py', 'Spell naïve'], role=['reviewer', 'tester']))\n\
                        agent('Spell naïve', role='critic')";
                    (String::new(), Some(code))
                }
                (true, false) => ("Both answered.".to_owned(), None),
                (false, _) => {
                    let own = own.unwrap_or("no role");
                    (format!("{own} on {}", self.0), None)
                }
            };

            Box::pin(async move {
                match code {
                    Some(code) => {
                        on_delta(StreamDelta::CallCode(code));
                        on_delta(StreamDelta::CallEnd(Ok(())));
                    }
                    None => on_delta(StreamDelta::Content(&text)),
                }
                Ok(Finished {
                    finish: Finish::Stop,
                    usage: None,
                })
            })
        }
    }

    #[test]
    fn roles_played() {
        let role = |name: &str, description: &str, model: Option<Arc<dyn Model>>| Role {
            name: name.to_owned(),
            description: description.to_owned(),
            model,
        };
        let roles = vec![
            role(
                "reviewer",
                "Reviews the café's code",
                Some(Arc::new(Cast("its own model"))),
            ),
            role("tester", "Writes naïve tests", None),
        ];
        let mut agent = with_roles(Cast("the session's model"), Box::new(|_| {}), roles);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Check the code").expect("run");
        assert_eq!(answer.as_deref(), Some("Both answered."));

        let MemoryStep::Action(step) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        let call = &step.calls[0];
        let result = call.result.as_ref().expect("ran");
        let printed = "Your role is `reviewer`: Reviews the café's code on its own model\n\
            [\"Your role is `reviewer`: Reviews the café's code on its own model\", \
            \"Your role is `tester`: Writes naïve tests on the session's model\"]";
        assert_eq!(result.printed.as_deref(), Some(printed));
        let unknown = "ValueError: agent: role: critic: unknown (roles: reviewer, tester)";
        let error = call.error.as_ref().expect("error").to_string();
        assert!(error.starts_with(unknown), "{error}");
        let played: Vec<Option<&str>> = result
            .agents
            .iter()
            .map(|helper| helper.role.as_deref())
            .collect();
        assert_eq!(played, [Some("reviewer"), Some("reviewer"), Some("tester")]);
    }

    /// One still waiting for a slot ends on an interrupt without starting a worker
    #[test]
    fn queued_agent_stops() {
        let model = Teamed(Requests::default());
        let tested = scripted(model, Box::new(|_| {}));
        let mut agent = tested.agent.clone();
        agent.slots = Arc::new(Semaphore::new(0));
        agent.interrupt.cancel(0);

        let helping = agent.help("Count the cafés".to_owned(), Box::new(Unseen), Vec::new());
        let waiting = async { time::timeout(Duration::from_secs(5), helping).await };
        let helper = block_on(waiting).expect("stopped");
        assert_eq!(helper.end, Ending::Interrupted {});
        assert!(helper.memory.steps.is_empty(), "{helper:?}");
    }

    #[test]
    fn cap_documented() {
        let builtins = include_str!("../worker/bootstrap/builtins.py");
        let documented =
            format!("A list of tasks runs up to {MAX_PARALLEL_AGENTS} agents at a time");
        assert!(builtins.contains(&documented), "{documented}");
    }
}
