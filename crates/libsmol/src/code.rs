use crate::config::Skill;
use crate::memory::{CallError, MAX_OBSERVATION_CHARS};
use crate::model::Media;
use crate::prompt::{self, NO_SERVER};
use crate::spawn::SHOWN_EVERY;
use crate::tools::mcp::{Called, Calling, Server};
use crate::utils::Interrupt;
use crate::worker::sandbox::Sandbox;
use crate::worker::{Function, Guest, Installed, ToolCall};
use crate::{Error, Result};
use reaper::terminal_view;
use std::future;
use std::mem;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

const STOP_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub(crate) struct Evaluated {
    pub(crate) logs: String,
    /// Characters dropped from the middle of `logs`
    pub(crate) cut: usize,
    pub(crate) error: Option<CallError>,
    pub(crate) media: Vec<(String, Media)>,
    /// The code actually run after stray tags were stripped
    pub(crate) code: Option<String>,
}

pub(crate) struct Added {
    /// The name used in code
    pub(crate) name: String,
    pub(crate) server: Server,
    pub(crate) headers: Vec<String>,
}

/// Callers keep track of the installed MCP servers and pass them in on every call
pub(crate) struct Worker {
    sandbox: Arc<Sandbox>,
    interrupt: Interrupt,
    functions: Vec<Function>,
    restarted: bool,
    live: Option<Live>,
}

/// Dropping it stops the Python
struct Live {
    guest: Guest,
    /// MCP calls the code is waiting on
    calls: JoinSet<()>,
}

impl Worker {
    /// `restarted` means earlier code in the session defined names this new worker lacks
    pub(crate) async fn new(
        interrupt: &Interrupt,
        cwd: &Path,
        copies: &Path,
        skills: &[Skill],
        restarted: bool,
    ) -> Result<Self> {
        let sandbox = match Sandbox::new(cwd, copies, skills) {
            Ok(sandbox) => sandbox,
            Err(reason) => {
                let message = prompt::unstarted(&reason);
                return Err(Error::Python(message));
            }
        };
        let mut worker = Self {
            sandbox: Arc::new(sandbox),
            interrupt: interrupt.clone(),
            functions: Vec::new(),
            restarted,
            live: None,
        };
        worker.connect(&[]).await.map_err(Error::Python)?;
        Ok(worker)
    }

    pub(crate) fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// Takes the flag, which turns true again only when the worker is replaced
    pub(crate) fn restarted(&mut self) -> bool {
        mem::take(&mut self.restarted)
    }

    /// A newly started worker gets `servers` installed first
    async fn connect(&mut self, servers: &[&Added]) -> Result<(), String> {
        if self.live.is_some() {
            return Ok(());
        }

        let started = Guest::start(&self.sandbox, &self.interrupt).await;
        let (guest, functions) = started.map_err(|reason| prompt::unstarted(&reason))?;
        let live = Live {
            guest,
            calls: JoinSet::new(),
        };
        for added in servers {
            live.install(&added.server).await?;
        }
        self.functions = functions;
        self.live = Some(live);
        Ok(())
    }

    /// `servers` are already installed, and a replacement for a lost worker gets them back first
    pub(crate) async fn add_server(&mut self, server: Server, servers: &[&Added]) -> Result<Added> {
        self.connect(servers).await.map_err(Error::Python)?;
        let live = self.live.as_mut().expect("connected");
        let installed = live.install(&server).await.map_err(Error::Python)?;

        let name = installed.name;
        if let Some(other) = installed.server {
            let message = format!("{name}: name already used by server {other}");
            return Err(Error::Python(message));
        }

        Ok(Added {
            name,
            server,
            headers: installed.headers,
        })
    }

    /// Doesn't restart a lost worker just for this
    pub(crate) fn remove_server(&self, name: &str) {
        if let Some(live) = &self.live {
            live.guest.remove_server(name);
        }
    }

    /// `shown` gets the clipped output so far now and then,
    /// and stopping before the code is sent returns Err
    pub(crate) async fn evaluate(
        &mut self,
        code: &str,
        servers: &[&Added],
        shown: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Evaluated, CallError> {
        let interrupt = self.interrupt.clone();
        let failed = |reason: String| {
            let error = prompt::python(&reason);
            CallError::Execution(error)
        };
        let connected = select! {
            biased;
            connected = self.connect(servers) => connected.map_err(failed),
            () = interrupt.wait() => Err(CallError::Interrupted),
        };
        connected?;

        let live = self.live.as_mut().expect("connected");
        // Calls still running from the previous snippet don't belong to this one
        live.calls.abort_all();
        live.guest.clear();
        let running = live.guest.run(code);
        let mut running = pin!(running);

        let mut stop_at = None;
        let mut killed = false;
        let mut show_at = Instant::now() + SHOWN_EVERY;
        let replied = loop {
            let stopping = stop_at.unwrap_or_else(Instant::now);
            select! {
                biased;
                replied = &mut running => break replied,
                Some(call) = live.guest.calls.recv() => {
                    let ToolCall {
                        server,
                        tool,
                        args,
                        called,
                        mut reply,
                    } = call;
                    let found = servers.iter().find(|added| added.name == server);
                    let calling: Calling = match found {
                        Some(added) => (added.server.call)(tool, args, called),
                        None => {
                            let reply = Err(NO_SERVER.to_owned());
                            let called = Called::from(reply);
                            let ready = future::ready(called);
                            Box::pin(ready)
                        }
                    };
                    let shared = Arc::clone(&live.guest.shared);
                    // The guest gives up on the reply when interrupted, which stops the call
                    let answering = async move {
                        let called = select! {
                            called = calling => called,
                            () = reply.closed() => return,
                        };
                        let mut shared = shared.lock().expect("shared lock");
                        shared.media.extend(called.media);
                        reply.send(called.result).ok();
                    };
                    live.calls.spawn(answering);
                }
                Some(_) = live.calls.join_next() => {}
                () = interrupt.wait(), if stop_at.is_none() => {
                    stop_at = Some(Instant::now() + STOP_WAIT);
                }
                () = time::sleep_until(stopping), if stop_at.is_some() && !killed => {
                    live.guest.kill();
                    killed = true;
                }
                () = time::sleep_until(show_at) => {
                    show_at = Instant::now() + SHOWN_EVERY;
                    if let Some(text) = live.guest.fresh(MAX_OBSERVATION_CHARS) {
                        shown(&text);
                    }
                }
            }
        };

        let (logs, cut, media) = live.guest.taken();
        let logs = terminal_view(&logs);
        let mut output = Evaluated {
            logs,
            cut,
            media,
            ..Evaluated::default()
        };

        match replied {
            Ok(ran) if !killed => {
                let judged = |error: String| match interrupt.triggered() {
                    true => CallError::Interrupted,
                    false => CallError::Execution(error),
                };
                output.error = ran.error.map(judged);
                output.code = ran.code;
            }
            // A kill as the code finished still leaves this Python unusable
            Ok(_) => {
                self.live = None;
                self.restarted = true;
                output.error = Some(CallError::Interrupted);
            }
            Err(lost) => {
                self.live = None;
                self.restarted = true;
                let error = match interrupt.triggered() {
                    true => CallError::Interrupted,
                    false => failed(prompt::lost(&lost)),
                };
                output.error = Some(error);
            }
        }
        Ok(output)
    }
}

impl Live {
    async fn install(&self, server: &Server) -> Result<Installed, String> {
        let installing = self
            .guest
            .add_server(&server.name, &server.description, &server.tools);
        match installing.await {
            Ok(installed) => installed,
            Err(lost) => Err(prompt::lost(&lost)),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::code::{Added, Evaluated, Worker, STOP_WAIT};
    use crate::memory::CallError;
    use crate::prompt::system;
    use crate::tests::{cancel_soon, temp_dir, write_file, Temp};
    use crate::tools::mcp::tests::{fake, started};
    use crate::tools::mcp::{Called, Calling, Server};
    use crate::utils::{block_on, Interrupt};
    use serde_json::{json, Map, Value};
    #[cfg(unix)]
    use std::fs::Permissions;
    use std::fs::{self, File};
    use std::ops::{Deref, DerefMut};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use tokio::time;

    /// Keeps track of installed servers in place of the MCP pool
    struct Tried {
        worker: Worker,
        servers: Vec<Added>,
    }

    impl Deref for Tried {
        type Target = Worker;

        fn deref(&self) -> &Worker {
            &self.worker
        }
    }

    impl DerefMut for Tried {
        fn deref_mut(&mut self) -> &mut Worker {
            &mut self.worker
        }
    }

    impl Tried {
        fn eval(&mut self, code: &str) -> Evaluated {
            let mut ignore = |_: &str| {};
            let servers: Vec<&Added> = self.servers.iter().collect();
            let evaluating = self.worker.evaluate(code, &servers, &mut ignore);
            block_on(evaluating).expect("evaluate")
        }

        /// Replaces a server with the same name and returns its name in code
        fn add(&mut self, server: &Server) -> Result<String, String> {
            self.remove(&server.name);
            let servers: Vec<&Added> = self.servers.iter().collect();
            let adding = self.worker.add_server(server.clone(), &servers);
            let added = block_on(adding).map_err(|e| e.to_string())?;
            let name = added.name.clone();
            self.servers.push(added);
            Ok(name)
        }

        fn remove(&mut self, source: &str) {
            let found = self
                .servers
                .iter()
                .position(|added| added.server.name == source);
            if let Some(index) = found {
                let added = self.servers.remove(index);
                self.worker.remove_server(&added.name);
            }
        }
    }

    fn build(interrupt: Interrupt, cwd: PathBuf) -> Temp<Tried> {
        let dir = temp_dir("session");
        let copies = dir.join("copies");
        let building = Worker::new(&interrupt, &cwd, &copies, &[], false);
        let worker = block_on(building).expect("worker");
        let value = Tried {
            worker,
            servers: Vec::new(),
        };
        Temp { value, dir }
    }

    fn fresh() -> Temp<Tried> {
        let (worker, _) = interruptible();
        worker
    }

    fn workspace(name: &str) -> (Temp<Tried>, PathBuf) {
        let dir = temp_dir(name);
        fs::create_dir_all(&dir).expect("dir");
        let worker = build(Interrupt::default(), dir.clone());
        (worker, dir)
    }

    fn run(code: &str) -> Evaluated {
        fresh().eval(code)
    }

    fn interruptible() -> (Temp<Tried>, Interrupt) {
        let interrupt = Interrupt::default();
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let worker = build(interrupt.clone(), cwd);
        (worker, interrupt)
    }

    fn server(name: &str, call: impl Fn(&str, &Value) -> Value + Send + Sync + 'static) -> Server {
        let echo = json!({
            "name": "echo-text",
            "description": "Echoes the text.",
            "inputSchema": {"type": "object", "properties": {"the-text": {"type": "string"}}},
        });
        let call = move |function: String, args: Map<String, Value>, _: String| -> Calling {
            let value = call(&function, &Value::Object(args));
            let called = Called::from(Ok(value));
            Box::pin(async move { called })
        };

        Server {
            name: name.to_owned(),
            description: format!("MCP server {name}"),
            tools: vec![echo],
            call: Arc::new(call),
        }
    }

    fn error(output: Evaluated) -> String {
        let error = output.error.expect("error");
        error.to_string()
    }

    #[test]
    fn interrupt_escapes_except() {
        let (mut worker, interrupt) = interruptible();
        cancel_soon(&interrupt, 0);
        let code = "noted = 'café'\n\
            try:\n    while True:\n        pass\nexcept Exception:\n    print('caught')\n";
        let output = worker.eval(code);

        assert_eq!(output.logs, "");
        assert_eq!(output.error, Some(CallError::Interrupted));

        interrupt.begin(1);
        let output = worker.eval("[noted, 1 + 1]");
        assert_eq!(output.logs, "['café', 2]\n");
        assert!(!worker.restarted());
    }

    #[test]
    fn worker_restarted() {
        let mut worker = fresh();
        worker.eval("x = 1");
        assert!(!worker.restarted());

        let exited = worker.eval("import os\nprint('leaving')\nos._exit(3)");
        assert_eq!(exited.logs, "leaving\n");
        let message = error(exited);
        assert_eq!(message, "Python exited with code 3");
        assert!(worker.restarted());
        assert!(!worker.restarted());
        let output = worker.eval("'x' in globals()");
        assert_eq!(output.logs, "False\n");

        // Running code doesn't take the flag
        let crashed = worker.eval("import os\nos.abort()");
        let crashed = error(crashed);
        assert_eq!(crashed, "Python crashed: aborted");
        let output = worker.eval("1 + 1");
        assert_eq!(output.logs, "2\n");
        assert!(worker.restarted());
    }

    #[test]
    fn connect_interrupted() {
        let (mut worker, interrupt) = interruptible();
        worker.live = None;
        interrupt.cancel(0);
        let mut ignore = |_: &str| {};
        let evaluating = worker.evaluate("print('skipped')", &[], &mut ignore);
        let stopped = block_on(evaluating);
        assert_eq!(stopped.err(), Some(CallError::Interrupted));

        interrupt.begin(1);
        let output = worker.eval("print('ran')");
        assert_eq!(output.logs, "ran\n");
    }

    /// With a small thread stack, deep recursion crashes once the limit is raised
    #[test]
    fn deep_recursion() {
        let code = "import sys\n\
            def deep(n):\n    return 0 if n == 0 else 1 + deep(n - 1)\n\
            try:\n    deep(10 ** 4)\nexcept RecursionError:\n    print('too deep')\n\
            sys.setrecursionlimit(10 ** 6)\nprint(deep(50000))";
        let output = run(code);
        assert_eq!(output.error, None);
        assert_eq!(output.logs, "too deep\n50000\n");
    }

    #[test]
    fn stray_tags_removed() {
        let mut worker = fresh();
        let mut ran = |code: &str| {
            let output = worker.eval(code);
            assert_eq!(output.error, None, "{code}");
            let repaired = output.code.as_deref().map(str::trim).map(str::to_owned);
            (output.logs, repaired)
        };

        let wrapped = ran("<eval>\n<eval>\nx = 'café'\nprint(x)\n</eval>\n</eval>");
        let code = "x = 'café'\nprint(x)".to_owned();
        assert_eq!(wrapped, ("café\n".to_owned(), Some(code)));
        let inline = ran("print('naïve')</eval>");
        assert_eq!(inline.1.as_deref(), Some("print('naïve')"));
        let faked = ran("print(3)</eval>Observation:\n3");
        assert_eq!(faked.1.as_deref(), Some("print(3)"));
        let closed = ran("<eval>\nprint(4)\n<eval>\nThen check the output.");
        assert_eq!(closed, ("4\n".to_owned(), Some("print(4)".to_owned())));
        let closers = ran("print('résumé')\n</parameter>\n  </invoke>");
        assert_eq!(
            closers,
            ("résumé\n".to_owned(), Some("print('résumé')".to_owned()))
        );
        let bar = '\u{ff5c}';
        let wide = format!(
            "print('jalapeño')\n</{bar}{bar}X{bar}{bar} parameter>\n</{bar}{bar}X{bar}{bar} invoke>"
        );
        let wide = ran(&wide);
        assert_eq!(
            wide,
            (
                "jalapeño\n".to_owned(),
                Some("print('jalapeño')".to_owned())
            )
        );
        let misspelled = ran("<evals>\nprint('façade')");
        assert_eq!(
            misspelled,
            ("façade\n".to_owned(), Some("print('façade')".to_owned()))
        );
        let invoked = "<invoke name=\"eval\">\n<parameter name=\"code\">\nprint('über')\n</parameter>\n</invoke>";
        let invoked = ran(invoked);
        assert_eq!(
            invoked,
            ("über\n".to_owned(), Some("print('über')".to_owned()))
        );
        let led = format!(
            "<parameter name=\"code\">x = 'piñata'\nprint(x)\n</{bar}{bar}X{bar}{bar} parameter>"
        );
        let led = ran(&led);
        let code = "x = 'piñata'\nprint(x)".to_owned();
        assert_eq!(led, ("piñata\n".to_owned(), Some(code)));
        let one_line =
            ran("<invoke name=\"eval\">\n<parameter name=\"code\">print('déjà vu')</parameter>");
        assert_eq!(one_line.1.as_deref(), Some("print('déjà vu')"));
        // Code in another call that follows isn't run
        let looped = "print('crème brûlée')\n</</eval>\n</invoke>\n<parameter name=\"code\">print('repeated')\n</parameter>\n</calls>";
        let looped = ran(looped);
        assert_eq!(
            looped,
            (
                "crème brûlée\n".to_owned(),
                Some("print('crème brûlée')".to_owned())
            )
        );

        // Tags inside strings are left alone
        let quoted = ran("s = '''\n</eval>\n'''\nprint(len(s), '<eval>')\n</eval>");
        let code = "s = '''\n</eval>\n'''\nprint(len(s), '<eval>')".to_owned();
        assert_eq!(quoted, ("9 <eval>\n".to_owned(), Some(code)));
        let page = ran("résumé = '''\n</div>\n'''\nprint(len(résumé))\n</invoke>");
        let code = "résumé = '''\n</div>\n'''\nprint(len(résumé))".to_owned();
        assert_eq!(page, ("8\n".to_owned(), Some(code)));
        let block = ran("façade = '''\n<div>\n'''\nprint(len(façade))\n</eval>");
        let code = "façade = '''\n<div>\n'''\nprint(len(façade))".to_owned();
        assert_eq!(block, ("7\n".to_owned(), Some(code)));
        let valid = ran("html = '<eval>café</eval>'\nprint(html)");
        assert_eq!(valid, ("<eval>café</eval>\n".to_owned(), None));

        let broken = worker.eval("print(1\n</eval>");
        assert_eq!(broken.code, None);
        let error = error(broken);
        assert!(error.starts_with("syntax error"), "{error}");
    }

    #[test]
    fn definitions_shown() {
        let mut worker = fresh();
        let code = "def logged(f):\n    return f\n\
             @logged\ndef count(rows, *, unique=False) -> int:\n    # count the rows\n    return len(rows)\n\
             class Node(dict):\n    level = 1\n\
             if True:\n    def nested(x):\n        return x";
        let output = worker.eval(code);
        assert_eq!(output.error, None);

        // Source is attached only after the step finishes
        let logs = worker.eval("help(count)\nhelp(Node)\nhelp(nested)").logs;
        let counted = "@logged\ndef count(rows, *, unique=False) -> int:\n    # count the rows\n    return len(rows)\n";
        assert!(logs.contains(counted), "{logs}");
        assert!(
            logs.contains("class Node(dict):\n    level = 1\n"),
            "{logs}"
        );
        assert!(logs.ends_with("def nested(x):\n    return x\n"), "{logs}");

        // A `def` that never ran isn't attached to the old function,
        // and neither is one rebound to another value later
        let failed = worker.eval("1 / 0\ndef count(rows):\n    return 0");
        let message = error(failed);
        assert!(message.starts_with("ZeroDivisionError"), "{message}");
        let swapped = worker.eval("def nested(x):\n    return 2 * x\nnested = Node");
        assert_eq!(swapped.error, None);
        let logs = worker.eval("help(count)\nhelp(nested)").logs;
        assert!(logs.contains(counted), "{logs}");
        assert!(
            logs.ends_with("class Node(dict):\n    level = 1\n"),
            "{logs}"
        );
    }

    /// Code busy inside a builtin never checks for an interrupt and is stopped after `STOP_WAIT`
    #[test]
    fn builtin_stopped() {
        let (mut worker, interrupt) = interruptible();
        worker.eval("x = 1");
        cancel_soon(&interrupt, 0);
        let started = Instant::now();
        let output = worker.eval("import itertools\nsum(itertools.count())");
        let waited = started.elapsed();
        assert_eq!(output.error, Some(CallError::Interrupted));
        assert!(waited < STOP_WAIT + Duration::from_secs(1), "{waited:?}");

        interrupt.begin(1);
        let output = worker.eval("'x' in globals()");
        assert_eq!(output.logs, "False\n");
        assert!(worker.restarted());
    }

    /// Dropping the worker stops code still running, and the temporary folder goes with it
    #[test]
    fn session_dropped() {
        let mut worker = fresh();
        let temp = worker
            .eval("import tempfile\nprint(tempfile.gettempdir())")
            .logs;
        let temp = PathBuf::from(temp.trim());
        assert!(temp.is_dir(), "{temp:?}");

        let mut ignore = |_: &str| {};
        let looping = worker.evaluate("while True:\n    pass", &[], &mut ignore);
        let abandoned = async { time::timeout(Duration::from_millis(300), looping).await };
        assert!(block_on(abandoned).is_err());
        drop(worker);

        let started = Instant::now();
        while temp.exists() {
            assert!(started.elapsed() < Duration::from_secs(5), "{temp:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Files in the temporary folder outlive a restart within the session
    #[test]
    fn temp_kept() {
        let mut worker = fresh();
        let code = "import os, tempfile\npath = os.path.join(tempfile.gettempdir(), 'café.txt')\n\
            with open(path, 'w') as f:\n    f.write('kept')\nprint(path)";
        let path = worker.eval(code).logs;
        worker.eval("import os\nos._exit(0)");
        assert!(worker.restarted());

        let code = format!("print(open({:?}).read())", path.trim());
        let output = worker.eval(&code);
        assert_eq!(output.logs, "kept\n");
    }

    #[test]
    fn environment_limited() {
        let code = "import os\nallowed = {'HOME', 'USER', 'LOGNAME', 'PATH', 'LANG', 'TZ', 'TZDIR', 'PYTHONTZPATH', 'TMPDIR', 'SSL_CERT_FILE'}\n\
            [sorted(set(os.environ) - allowed), os.environ['LANG']]";
        let output = run(code);
        assert_eq!(output.logs, "[[], 'C.UTF-8']\n");
    }

    /// A worker stuck after an interrupt and one that exits on its own are both replaced
    #[test]
    fn cancel_replaced() {
        let stuck = "print('stuck')\nimport time\n\
            while True:\n    try:\n        time.sleep(10)\n    \
            except BaseException:\n        pass";
        let exiting =
            "import os, time\ntry:\n    time.sleep(30)\nexcept BaseException:\n    os._exit(1)";
        let (mut worker, interrupt) = interruptible();

        let mut turn = 0;
        for (code, said) in [(stuck, "stuck\n"), (exiting, "")] {
            worker.eval("x = 1");
            cancel_soon(&interrupt, turn);
            let stopped = worker.eval(code);
            assert_eq!(stopped.logs, said, "{code}");
            assert_eq!(stopped.error, Some(CallError::Interrupted), "{code}");

            turn += 1;
            interrupt.begin(turn);
            let output = worker.eval("'x' in globals()");
            assert_eq!(output.logs, "False\n", "{code}");
            assert!(worker.restarted(), "{code}");
        }
    }

    #[test]
    fn flush_stop_kept() {
        let (mut worker, interrupt) = interruptible();
        let code = "import sys, time\n\
            class Slow:\n    closed = False\n    def write(self, text):\n        return len(text)\n    def flush(self):\n        time.sleep(30)\n\
            x = 'kept'\nsys.stdout = Slow()";
        cancel_soon(&interrupt, 0);
        let output = worker.eval(code);
        assert_eq!(output.error, Some(CallError::Interrupted));

        interrupt.begin(1);
        let kept = worker.eval("x");
        assert_eq!(kept.logs, "kept\n");
    }

    #[test]
    fn error_location() {
        let mut worker = fresh();
        let output = worker.eval("print('before')\nx = 1\n1 / 0");
        assert_eq!(output.logs, "before\n");

        let message = error(output);
        assert!(message.starts_with("ZeroDivisionError"), "{message}");
        assert!(message.contains("line 3: 1 / 0"), "{message}");

        let output = worker.eval("def f():\n    return {}['k']\n\nf()\nx = 1");
        let message = error(output);
        assert!(message.starts_with("KeyError"), "{message}");
        assert!(message.contains("line 2: return {}['k']"), "{message}");

        // An error inside a function from an earlier step reports the line in this snippet
        worker.eval("def parse(x):\n    return int(x)");
        let output = worker.eval("total = 0\nfor d in ['1', 'two']:\n    total += parse(d)");
        let message = error(output);
        assert!(message.starts_with("ValueError"), "{message}");
        assert!(
            message.contains("\n  line 3: total += parse(d)"),
            "{message}"
        );

        // An exception without a message shows only its class name
        let output = worker.eval("value = 1\nassert value == 2");
        let message = error(output);
        assert_eq!(message, "AssertionError\n  line 2: assert value == 2");
    }

    #[test]
    fn output_streamed() {
        let mut worker = fresh();
        let mut seen = Vec::new();
        let mut shown = |text: &str| seen.push(text.to_owned());
        let code = "import time\nprint('\\x1b[1mfirst line\\x1b[0m', flush=True)\ntime.sleep(0.5)\nprint('second line')";
        let evaluating = worker.evaluate(code, &[], &mut shown);
        let output = block_on(evaluating).expect("evaluate");

        assert_eq!(output.logs, "first line\nsecond line\n");
        let first = seen.first().map(String::as_str);
        assert_eq!(first, Some("first line\n"), "{seen:?}");
    }

    #[test]
    fn values_echoed() {
        let mut worker = fresh();
        worker.eval("x = {'café': [1, 2]}");

        let output = worker.eval("x");
        assert_eq!(output.logs, "{'café': [1, 2]}\n");

        let every =
            "x = 1\nx\nprint('printed')\n'text'\nNone\nfor i in range(3):\n    i\n[x, 'naïve']";
        let output = worker.eval(every);
        assert_eq!(output.logs, "1\nprinted\ntext\n[1, 'naïve']\n");
    }

    #[test]
    fn syntax_caret() {
        let mut worker = fresh();
        let output = worker.eval("x = (");
        let message = error(output);
        let expected = "syntax error on line 1: '(' was never closed\nx = (\n    ^";
        assert_eq!(message, expected);

        // Wide characters take two columns in a terminal
        let output = worker.eval("café = '🐍' +");
        let message = error(output);
        assert!(
            message.ends_with("\ncafé = '🐍' +\n             ^"),
            "{message}"
        );
        let output = worker.eval("if True:\n\tcafé = '🍕' + 🚀1");
        let message = error(output);
        assert!(
            message.ends_with("\n\tcafé = '🍕' + 🚀1\n\t              ^"),
            "{message}"
        );

        let output = worker.eval("print('first')\nreturn 1");
        let message = error(output);
        let expected = "syntax error on line 2: 'return' outside function\nreturn 1\n^";
        assert_eq!(message, expected);
    }

    #[test]
    fn error_in_module() {
        let (mut worker, dir) = workspace("helper");
        fs::write(dir.join("café.py"), "def fail():\n    return {}['crème']\n").expect("write");
        let code = "import importlib.util\nspec = importlib.util.spec_from_file_location('helper', 'café.py')\n\
             helper = importlib.util.module_from_spec(spec)\nspec.loader.exec_module(helper)\nhelper.fail()";

        let output = worker.eval(code);
        let message = error(output);
        fs::remove_dir_all(&dir).ok();

        assert!(message.starts_with("KeyError: 'crème'"), "{message}");
        assert!(message.contains("line 5: helper.fail()"), "{message}");
        let raised = "line 2, in fail: return {}['crème']";
        assert!(message.contains(raised), "{message}");
    }

    #[test]
    fn builtin_frames_hidden() {
        let output = run("edit('café.md', '', 'b')");
        let message = error(output);
        let expected =
            "ValueError: edit: café.md: old text is empty\n  line 1: edit('café.md', '', 'b')";
        assert_eq!(message, expected);
    }

    #[test]
    fn views_copied() {
        let dir = temp_dir("views");
        fs::create_dir_all(&dir).expect("dir");
        let copies = dir.join("copies");
        let interrupt = Interrupt::default();
        let building = Worker::new(&interrupt, &dir, &copies, &[], false);
        let worker = block_on(building).expect("worker");
        let mut worker = Tried {
            worker,
            servers: Vec::new(),
        };

        // Rewriting the file later doesn't change the image viewed first
        let code = "import os\n\
            with open('screenshot.png', 'wb') as f:\n    f.write(b'\\x89PNG\\r\\n\\x1a\\n' + 'first'.encode())\n\
            view('screenshot.png')\n\
            with open('screenshot.png', 'wb') as f:\n    f.write(b'\\x89PNG\\r\\n\\x1a\\n' + 'second'.encode())\n\
            os.mkdir('subdir')\nos.chdir('subdir')\nview('../screenshot.png')";
        let output = worker.eval(code);
        assert_eq!(output.error, None);
        let mut seen = Vec::new();
        for (path, media) in &output.media {
            let copy = copies.join(&media.file);
            let copied = fs::read(copy).expect("copy");
            let text = String::from_utf8_lossy(&copied[8..]).into_owned();
            seen.push((path.as_str(), text));
        }
        let first = ("screenshot.png", "first".to_owned());
        let second = ("screenshot.png", "second".to_owned());
        assert_eq!(seen, [first, second]);
        let next = worker.eval("1");
        assert!(next.media.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// Modules in the working directory are importable, and argparse and runpy read `sys.argv[0]`
    #[test]
    fn interpreter_settings() {
        let dir = temp_dir("settings");
        fs::create_dir_all(&dir).expect("create dir");
        let module = dir.join("smol_local.py");
        fs::write(&module, "value = 42\n").expect("write");

        let mut worker = build(Interrupt::default(), dir.clone());
        let code = "import argparse, pathlib, runpy, smol_local\n\
             print(smol_local.value)\n\
             _ = pathlib.Path('script.py').write_text('import sys\\nprint(sys.argv[0])')\n\
             _ = runpy.run_path('script.py')\n\
             parser = argparse.ArgumentParser()\n\
             _ = parser.add_argument('name')\n\
             parser.parse_args(['Zoë']).name";
        let output = worker.eval(code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        assert_eq!(output.logs, "42\nscript.py\nZoë\n");
    }

    #[test]
    fn argument_binding() {
        let mut worker = fresh();
        let header = "edit(path: str | os.PathLike, old: str, new: str) -> None";
        let unbound = [
            ("edit()", "missing a required argument: 'path'"),
            ("edit('a', 'b', 'c', 'd')", "too many positional arguments"),
            (
                "edit('a', 'b', 'c', lines=1)",
                "got an unexpected keyword argument 'lines'",
            ),
            (
                "edit('a', 'b', 'c', path='x')",
                "multiple values for argument 'path'",
            ),
        ];
        for (code, expected) in unbound {
            let output = worker.eval(code);
            let message = error(output);
            let first = message.lines().next().unwrap_or_default();
            assert_eq!(first, format!("TypeError: {header}: {expected}"), "{code}");
        }

        let invalid = [
            (
                "edit(1, 'a', 'b')",
                "TypeError: edit: path must be str or a path, not int",
            ),
            (
                "edit('a', 1, 'b')",
                "TypeError: edit: old must be str, not int",
            ),
            (
                "edit('notes.md', '', 'b')",
                "ValueError: edit: notes.md: old text is empty",
            ),
            ("grep(1)", "TypeError: grep: pattern must be str, not int"),
        ];
        for (code, expected) in invalid {
            let output = worker.eval(code);
            let message = error(output);
            assert_eq!(message.lines().next(), Some(expected), "{code}");
        }
    }

    #[test]
    fn edit_replaces_once() {
        let (mut worker, dir) = workspace("edit");
        let path = dir.join("café.py");
        fs::write(&path, "name = 'café'\nalias = 'café'\n").expect("write");
        let shown = "café.py";

        let twice = format!("edit({shown:?}, \"'café'\", \"'bistro'\")");
        let output = worker.eval(&twice);
        let message = error(output);
        let found_twice = format!("ValueError: edit: {shown}: old text found 2 times; include more surrounding text so it appears once");
        assert_eq!(message.lines().next(), Some(found_twice.as_str()));

        let once = format!("edit({shown:?}, \"name = 'café'\", \"name = 'bistro'\")");
        let output = worker.eval(&once);
        assert_eq!(output.error, None);
        let edited = fs::read_to_string(&path).expect("read");
        assert_eq!(edited, "name = 'bistro'\nalias = 'café'\n");

        // `open()` reads \n where the file has \r\n
        fs::write(&path, "café\r\nnaïve\r\n").expect("write");
        let lines = format!("edit({shown:?}, 'café\\nnaïve', 'one\\ntwo')");
        let output = worker.eval(&lines);
        assert_eq!(output.error, None);
        let edited = fs::read_to_string(&path).expect("read");
        assert_eq!(edited, "one\r\ntwo\r\n");

        fs::write(&path, "café\nnaïve\nrésumé\r\n").expect("write");
        let output = worker.eval(&lines);
        assert_eq!(output.error, None);
        let edited = fs::read_to_string(&path).expect("read");
        assert_eq!(edited, "one\ntwo\nrésumé\r\n");

        fs::write(&path, "façade\ncafé\r\nnaïve\r\n").expect("write");
        let output = worker.eval(&lines);
        assert_eq!(output.error, None);
        let edited = fs::read_to_string(&path).expect("read");
        assert_eq!(edited, "façade\none\r\ntwo\r\n");

        fs::write(&path, "café\r\nnaïve\r\n").expect("write");
        let leading = format!("edit({shown:?}, '\\nnaïve', '\\ntwo')");
        let output = worker.eval(&leading);
        assert_eq!(output.error, None);
        let edited = fs::read_to_string(&path).expect("read");
        assert_eq!(edited, "café\r\ntwo\r\n");

        fs::write(&path, b"caf\xe9").expect("write");
        let output = worker.eval(&once);
        let message = error(output);
        let expected = format!("ValueError: edit: {shown}: not UTF-8 text");
        assert_eq!(message.lines().next(), Some(expected.as_str()));

        fs::write(&path, "café\n").expect("write");
        let surrogate = format!("edit({shown:?}, 'café', chr(0xdc80))");
        let output = worker.eval(&surrogate);
        let message = error(output);
        assert!(message.starts_with("UnicodeEncodeError"), "{message}");
        let kept = fs::read_to_string(&path).expect("read");
        assert_eq!(kept, "café\n");

        // Overlapping matches count too
        fs::write(&path, "    }\n}\n}\n# end\n").expect("write");
        let overlapping = format!("edit({shown:?}, '}}\\n}}\\n', 'merged\\n')");
        let output = worker.eval(&overlapping);
        let message = error(output);
        assert_eq!(message.lines().next(), Some(found_twice.as_str()));
        let kept = fs::read_to_string(&path).expect("read");
        assert_eq!(kept, "    }\n}\n}\n# end\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn surrogates_replaced() {
        // Undecodable file names on Linux look like this
        let output = run("raise ValueError('bad file ' + chr(0xdcff))");
        let message = error(output);
        assert!(
            message.starts_with("ValueError: bad file \\udcff"),
            "{message}"
        );
    }

    #[test]
    fn closed_stdout_reopened() {
        let mut worker = fresh();
        let output = worker.eval("import sys\nprint('written')\nsys.stdout.close()");
        assert_eq!(output.logs, "written\n");

        let output = worker.eval("print('next')\nprint('original', file=sys.__stdout__)\n1 + 1");
        assert_eq!(output.error, None);
        assert_eq!(output.logs, "next\noriginal\n2\n");
    }

    #[test]
    fn output_captured() {
        let code = "import pprint, sys, warnings\npprint.pprint({'a': 1})\nsys.stdout.write('raw\\n')\nprint('café', 'naïve', sep='-', end='!')\nprint('résumé')\nwarnings.warn('careful')\nprint('dunder', file=sys.__stdout__)";
        let output = run(code);

        assert_eq!(output.error, None);
        for expected in [
            "{'a': 1}\n",
            "raw\n",
            "café-naïve!résumé\n",
            "UserWarning: careful",
            "dunder\n",
        ] {
            let logs = &output.logs;
            assert!(logs.contains(expected), "{logs}");
        }
    }

    #[test]
    fn stdlib_imports() {
        let modules = "asyncio imaplib multiprocessing zipfile tarfile gzip bz2 lzma shutil tempfile glob pathlib ssl \
            urllib.request http.client http.server smtplib socketserver xmlrpc.client logging logging.handlers json csv \
            sqlite3 uuid email concurrent.futures threading platform webbrowser getpass pdb unittest doctest pydoc \
            importlib.resources mimetypes shelve dbm fileinput";
        let code = format!(
            "failed = []\nfor name in {modules:?}.split():\n    try:\n        __import__(name)\n    except Exception as e:\n        failed.append(f'{{name}}: {{e}}')\nfailed"
        );

        let output = run(&code);
        assert_eq!(output.logs, "[]\n");
    }

    /// Only a stop in place replies and keeps variables set by the interrupted snippet
    #[test]
    fn stops_in_place() {
        let blocking = [
            "import time\ntime.sleep(30)",
            "import socket\nserver = socket.create_server(('127.0.0.1', 0))\n\
             quiet = socket.create_connection(server.getsockname())\nquiet.recv(1)",
            "import asyncio\nasyncio.run(asyncio.sleep(30))",
            "while True:\n    print('a line')",
        ];
        let (mut worker, interrupt) = interruptible();

        let mut turn = 0;
        for (index, code) in blocking.into_iter().enumerate() {
            interrupt.begin(turn);
            cancel_soon(&interrupt, turn);
            let code = format!("stopped_at = {index}\n{code}");
            let output = worker.eval(&code);
            assert_eq!(output.error, Some(CallError::Interrupted), "{code}");

            turn += 1;
            interrupt.begin(turn);
            let kept = worker.eval("stopped_at");
            assert_eq!(kept.logs, format!("{index}\n"), "{code}");
            turn += 1;
        }
    }

    #[test]
    fn os_errors() {
        let mut worker = fresh();

        let caught = "import errno\ntry:\n    ls('/nope/café')\nexcept FileNotFoundError as e:\n    print(e.errno == errno.ENOENT, e.filename)";
        let output = worker.eval(caught);
        assert_eq!(output.logs, "True /nope/café\n");

        // The guest numbers errors and words them its own way, the same on every system
        let output = worker.eval("ls('/nope/café')");
        let message = error(output);
        let expected = "FileNotFoundError: [Errno 44] No such file or directory: '/nope/café'";
        assert_eq!(message.lines().next(), Some(expected));
    }

    #[test]
    fn shadowing_until_deleted() {
        let mut worker = fresh();
        let notes = server("notes", |_, _| json!("echo"));
        worker.add(&notes).expect("add server");
        let mut run = |code: &str| worker.eval(code);

        let hidden = run("grep = 'constant'\nnotes = 1\ngrep('x')");
        let hidden = error(hidden);
        let not_callable = "TypeError: 'str' object is not callable";
        assert!(hidden.starts_with(not_callable), "{hidden}");

        let help = run("help('grep')").logs;
        assert!(help.starts_with("grep(pattern: str"), "{help}");

        let back = run("del grep, notes\n[callable(grep), notes.echo_text('x')]");
        assert_eq!(back.logs, "[True, 'echo']\n");
        assert_eq!(back.error, None);
    }

    #[test]
    fn taken_names_renamed() {
        let mut worker = fresh();
        for taken in ["fetch", "print", "time"] {
            let named = server(taken, |_, _| json!("echo"));
            let added = worker.add(&named).expect("added");
            assert_eq!(added, format!("{taken}_mcp"));
        }

        let code = "import time\n[callable(fetch), callable(print), print_mcp.echo_text('x'), time_mcp.echo_text('y')]";
        let kept = worker.eval(code);
        assert_eq!(kept.logs, "[True, True, 'echo', 'echo']\n");
        let listed = worker.eval("help(time_mcp)").logs;
        let titled = "MCP server time_mcp (configured as time)";
        assert!(listed.starts_with(titled), "{listed}");
    }

    #[test]
    fn server_names_clash() {
        let mut worker = fresh();
        let first = server("a-b", |_, _| json!("first"));
        worker.add(&first).expect("add server");

        let second = server("a_b", |_, _| json!("second"));
        let refused = worker.add(&second).expect_err("clash");
        assert_eq!(refused, "python: a_b: name already used by server a-b");
        let kept = worker.eval("a_b.echo_text('x')");
        assert_eq!(kept.logs, "first\n");

        let again = server("a-b", |_, _| json!("again"));
        worker.add(&again).expect("replace");
        let replaced = worker.eval("a_b.echo_text('x')");
        assert_eq!(replaced.logs, "again\n");

        worker.remove("a_b");
        assert_eq!(worker.servers.len(), 1);
    }

    #[test]
    fn server_removed() {
        let mut worker = fresh();
        let notes = server("café-notes", |_, _| json!("echo"));
        worker.add(&notes).expect("add server");

        worker.remove("café-notes");
        let gone = worker.eval("café_notes");
        let gone = error(gone);
        assert!(gone.starts_with("NameError"), "{gone}");

        // Sent straight to the worker, as `remove` skips names it never added
        worker.remove_server("print");
        let kept = worker.eval("callable(print)");
        assert_eq!(kept.logs, "True\n");

        // A lost worker isn't restarted just for this
        worker.add(&notes).expect("add server");
        worker.eval("import os\nos._exit(1)");
        worker.remove("café-notes");
        assert!(worker.servers.is_empty());
        assert!(worker.live.is_none());
        let output = worker.eval("café_notes");
        let gone = error(output);
        assert!(gone.starts_with("NameError"), "{gone}");
    }

    #[test]
    fn server_functions() {
        let mut worker = fresh();
        let mut forms = server("my-forms", |name, args| json!([name, args]));
        let submit = json!({"name": "submit", "inputSchema": {"type": "object", "properties": {"form": {}}}});
        forms.tools.push(submit);
        forms.call = Arc::new(
            |name: String, args: Map<String, Value>, _: String| -> Calling {
                let result = match name.as_str() {
                    "submit" => {
                        let form = &args["form"];
                        Err(format!("bad form {form}"))
                    }
                    _ => Ok(json!([name, args])),
                };
                let called = Called::from(result);
                Box::pin(async move { called })
            },
        );
        let named = worker.add(&forms).expect("add server");
        assert_eq!(named, "my_forms");

        let echoed = worker.eval("my_forms.echo_text(the_text='déjà vu')");
        assert_eq!(echoed.logs, "['echo-text', {'the-text': 'déjà vu'}]\n");
        let unset = worker.eval("my_forms.echo_text(None)");
        assert_eq!(unset.logs, "['echo-text', {}]\n");

        let output = worker.eval("my_forms.echo_text(the_text=object())");
        let message = error(output);
        let first = message.lines().next();
        let refused = "TypeError: my_forms.echo_text: the-text: object is not JSON-serializable";
        assert_eq!(first, Some(refused));

        let output = worker.eval("my_forms.submit('a')");
        let message = error(output);
        let first = message.lines().next();
        assert_eq!(first, Some("ValueError: my_forms.submit: bad form \"a\""));

        let output = worker.eval("my_forms.search('x')");
        let message = error(output);
        let first = message.lines().next();
        let unknown =
            "AttributeError: my_forms: search: no such function (functions: echo_text, submit)";
        assert_eq!(first, Some(unknown));
    }

    #[test]
    fn calls_server() {
        let mut worker = fresh();
        let server = fake();
        worker.add(&server).expect("add server");

        let called = worker.eval("[fake.add(a=2, b=3), fake.echo_text(the_text='café')]");
        assert_eq!(called.logs, "['5', 'café']\n");
        assert_eq!(called.error, None);

        let failed = worker.eval("fake.fail()");
        let failed = error(failed);
        assert!(
            failed.starts_with("ValueError: fake.fail: boom"),
            "{failed}"
        );
    }

    #[test]
    fn server_media_kept() {
        let mut worker = fresh();
        let copies = temp_dir("mcp-media");
        let (server, _link) = started(copies.clone());
        worker.add(&server).expect("add server");

        let output = worker.eval("fake.mixed()");
        let mut kept = Vec::new();
        for (label, media) in &output.media {
            let copy = copies.join(&media.file);
            let copied = fs::read(copy).expect("copy");
            kept.push((label.as_str(), copied));
        }
        fs::remove_dir_all(&copies).ok();

        assert_eq!(output.error, None);
        let image = b"\x89PNG\r\n\x1a\nhello".to_vec();
        assert_eq!(kept, [("fake.mixed", image)]);
    }

    #[test]
    fn stopped_call_cancelled() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let call = move |_: String, _: Map<String, Value>, _: String| -> Calling {
            let guard = Dropped(Arc::clone(&flag));
            Box::pin(async move {
                let _guard = guard;
                time::sleep(Duration::from_secs(60)).await;
                Called::from(Ok(Value::Null))
            })
        };
        let mut slow = server("slow", |_, _| Value::Null);
        slow.call = Arc::new(call);

        let (mut worker, interrupt) = interruptible();
        worker.add(&slow).expect("add server");
        cancel_soon(&interrupt, 0);
        let output = worker.eval("called = 1\nslow.echo_text('x')");
        assert_eq!(output.error, Some(CallError::Interrupted));

        let started = Instant::now();
        while !dropped.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(5));
            // Agent-side tasks only run inside `block_on`
            let waiting = async { time::sleep(Duration::from_millis(20)).await };
            block_on(waiting);
        }

        // Only a stop in place keeps it
        interrupt.begin(1);
        let kept = worker.eval("called");
        assert_eq!(kept.logs, "1\n");
    }

    #[test]
    fn server_help() {
        let mut worker = fresh();
        let faked = fake();
        worker.add(&faked).expect("add server");
        let mut news = server("news", |_, _| json!("ok"));
        for i in 0..40 {
            let about = "née".repeat(100);
            let tool = json!({"name": format!("tool{i}"), "description": about});
            news.tools.push(tool);
        }
        worker.add(&news).expect("add server");

        let output = worker.eval("help(fake)\nhelp('fake.wait')");
        assert_eq!(output.error, None);
        let logs = output.logs;
        assert!(logs.starts_with("MCP server fake\n"), "{logs}");
        let wait = "\nfake.wait(seconds: float = None) -> str\nSleeps, then answers";
        assert!(logs.contains(wait), "{logs}");

        let logs = worker.eval("help(news)").logs;
        let shown = logs.chars().count();
        assert!(shown < 8192, "{shown}");
        assert!(logs.contains("news.tool39("), "{logs}");
        assert!(logs.contains("née…\n"), "{logs}");
        let full = worker.eval("help('news.tool39')").logs;
        let about = "née".repeat(100);
        assert!(full.contains(&about), "{full}");
    }

    #[test]
    fn servers_in_prompt() {
        let mut worker = fresh();
        let mut news = server("news-web", |_, _| Value::Null);
        news.description = "Searches the web and the news\n\nRead-only".to_owned();
        worker.add(&news).expect("add server");
        let mut notes = server("notes", |_, _| Value::Null);
        notes.description = String::new();
        worker.add(&notes).expect("add server");

        let servers: Vec<&Added> = worker.servers.iter().collect();
        let cwd = Path::new("/work");
        let text = system(&[], &servers, &[], cwd, &[]);
        let listed = "\n\n## MCP `news_web`\n\nSearches the web and the news\n\nRead-only\n\n\
                      - news_web.echo_text(the_text: str = None) -> str\n\n\
                      ## MCP `notes`\n\n\
                      - notes.echo_text(the_text: str = None) -> str\n\n# Rules\n";
        assert!(text.contains(listed), "{text}");
    }

    #[test]
    fn unicode_names_kept() {
        let mut worker = fresh();
        let echo = |function: &str, args: &Value| json!(format!("{function} {args}"));
        let mut maps = server("café", echo);
        let query = json!({"type": "object", "properties": {"crème": {}, "jalapeños": {}}});
        maps.tools = vec![
            json!({"name": "get-user", "inputSchema": query}),
            json!({"name": "get_user"}),
        ];
        let name = worker.add(&maps).expect("add server");
        assert_eq!(name, "café");

        let code = "print(café.get_user(crème='brûlée', jalapeños=2))\nprint(café.get_user_2())";
        let called = worker.eval(code);
        assert_eq!(
            called.logs,
            "get-user {\"crème\":\"brûlée\",\"jalapeños\":2}\nget_user {}\n"
        );

        // The agent receives the name as written in code
        let mut notes = server("déjà-vu", |_, _| Value::Null);
        notes.call = Arc::new(
            |_: String, _: Map<String, Value>, called: String| -> Calling {
                let called = Called::from(Ok(json!(called)));
                Box::pin(async move { called })
            },
        );
        let name = worker.add(&notes).expect("add server");
        assert_eq!(name, "déjà_vu");
        let called = worker.eval("déjà_vu.echo_text(the_text='hi')");
        assert_eq!(called.logs, "déjà_vu.echo_text\n");
    }

    #[test]
    fn server_schema_types() {
        let mut worker = fresh();
        let mut news = server("news", |_, args| args.clone());
        let countries: Vec<String> = (0..12).map(|i| format!("c{i}")).collect();
        let find = json!({
            "name": "find",
            "inputSchema": {
                "type": "object",
                "required": ["query", "limit"],
                "properties": {
                    "query": {"type": "string"},
                    "topic": {"const": "general"},
                    "range": {"anyOf": [{"enum": ["day", "week"], "type": "string"}, {"type": "null"}]},
                    "since": {"type": ["string", "null"]},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "kind": {"$ref": "#/$defs/Kind"},
                    "chain": {"$ref": "#/$defs/Chain"},
                    "country": {"enum": countries},
                    "limit": {"type": "integer"},
                },
                "$defs": {
                    "Kind": {"enum": ["a", "b"]},
                    "Chain": {"anyOf": [{"$ref": "#/$defs/Chain"}, {"type": "null"}]},
                },
            },
        });
        news.tools = vec![find];
        worker.add(&news).expect("add server");

        let output = worker.eval("help('news.find')");
        let header = output.logs.lines().next();
        let countries = "'c0', 'c1', 'c2', 'c3', 'c4', 'c5', 'c6', 'c7', 'c8', 'c9', 'c10', 'c11'";
        let expected = format!("news.find(query: str, limit: int, topic: Literal['general'] = None, range: Literal['day', 'week'] | None = None, since: str | None = None, tags: list[str] = None, kind: Literal['a', 'b'] = None, chain: Any | None = None, country: Literal[{countries}] = None) -> str");
        assert_eq!(header, Some(expected.as_str()));

        let called = worker.eval("news.find('weather', 3)");
        assert_eq!(called.logs, "{'query': 'weather', 'limit': 3}\n");

        // A call missing a required argument isn't sent
        let missing = worker.eval("news.find('weather')");
        assert_eq!(missing.logs, "");
        let message = error(missing);
        let first = message.lines().next();
        let refused = format!("TypeError: {expected}: missing a required argument: 'limit'");
        assert_eq!(first, Some(refused.as_str()));
    }

    #[test]
    fn cycles_collected() {
        let mut worker = fresh();
        let cycles = "class Node:\n    pass\nchain = [Node() for _ in range(2000)]\nfor a, b in zip(chain, chain[1:]):\n    a.next, b.prev = b, a";
        let count = "import gc\nlen(gc.get_objects())";
        let counted = |worker: &mut Tried| -> usize {
            let logs = worker.eval(count).logs;
            logs.trim().parse().expect("count")
        };

        worker.eval(cycles);
        let before = counted(&mut worker);
        for _ in 0..5 {
            worker.eval(cycles);
        }
        let after = counted(&mut worker);
        assert!(after < before + 1000, "{before} {after}");
    }

    #[test]
    fn help_lookup() {
        let mut worker = fresh();
        // Over the 8192 characters a print shows uncut
        let about = "café".repeat(3000);
        let defined = format!("def long():\n    '{about}'");
        worker.eval(&defined);
        let mut help = |request: &str| {
            let code = format!("help({request})");
            let output = worker.eval(&code);
            assert_eq!(output.error, None);
            output.logs
        };

        let edit = help("edit");
        let header = "edit(path: str | os.PathLike, old: str, new: str) -> None\n";
        assert!(edit.starts_with(header), "{edit}");

        let ls = help("'ls'");
        let header = "ls(path: str | os.PathLike = '.', recursive: bool = False) -> list[str]\n";
        assert!(ls.starts_with(header), "{ls}");

        let own = help("help");
        let header = "help(request: object) -> None\nprints the signature and docs of a function or module\n\nPrints ";
        assert!(own.starts_with(header), "{own}");

        let zipfile = help("'zipfile.ZipFile'");
        assert!(zipfile.starts_with("zipfile.ZipFile("), "{zipfile}");
        let summary = ")\nClass with methods to open";
        assert!(zipfile.contains(summary), "{zipfile}");

        let quote = help("__import__('urllib.parse').parse.quote");
        assert!(quote.starts_with("urllib.parse.quote("), "{quote}");

        let json = help("__import__('json')");
        assert!(json.starts_with("module json\nMembers: "), "{json}");
        assert!(json.contains("dumps"), "{json}");

        let long = help("long");
        assert_eq!(long, format!("long()\n{defined}\n"));

        assert_eq!(help("'nope.nothing'"), "help: nope.nothing: not found\n");
    }

    #[test]
    fn open_works() {
        let (mut worker, dir) = workspace("open");
        let code = "import csv, io, json, pathlib, zipfile\n\
             with open('menu.txt', 'w') as f:\n    f.write('café\\n')\n\
             with open('menu.txt', 'a') as f:\n    print('second', file=f)\n\
             f = open('b.bin', 'wb')\nf.write(b'\\x00\\x01')\nf.close()\n\
             with zipfile.ZipFile('c.zip', 'w') as z:\n    z.writestr('a.txt', 'zipped')\n\
             json.dump({'k': 1}, open('d.json', 'w'))\n\
             with open('e.csv', 'w', newline='') as f:\n    csv.writer(f).writerow(['a', 'b'])\n\
             pathlib.Path('f.txt').write_text('via pathlib')\n\
             try:\n    open('menu.txt', 'x')\nexcept FileExistsError:\n    exists = 'x refuses'\n\
             try:\n    open('nope.txt')\nexcept FileNotFoundError:\n    missing = 'not found'\n\
             [open('menu.txt').read(), open('b.bin', 'rb').read().hex(), zipfile.ZipFile('c.zip').read('a.txt').decode(),\n\
              json.load(open('d.json')), next(csv.reader(open('e.csv', newline=''))), pathlib.Path('f.txt').read_text(), exists, missing]";

        let output = worker.eval(code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        let expected = "2\n11\n['café\\nsecond\\n', '0001', 'zipped', {'k': 1}, ['a', 'b'], 'via pathlib', 'x refuses', 'not found']\n";
        assert_eq!(output.logs, expected);
    }

    #[test]
    fn open_files_flushed() {
        let (mut worker, dir) = workspace("flush");
        let output = worker.eval("log = open('log.txt', 'w')\nlog.write('still open')");
        assert_eq!(output.error, None);

        let written = fs::read_to_string(dir.join("log.txt")).expect("read");
        let closed = worker.eval("log.closed").logs;
        fs::remove_dir_all(&dir).ok();

        assert_eq!(written, "still open");
        assert_eq!(closed, "False\n");
    }

    #[test]
    fn print_limit() {
        let (mut worker, dir) = workspace("print");
        let code =
            "print('🐍' * 10000)\nprint('y' * 10000, limit=None)\nprint('z' * 100, limit=10)\n\
             with open('out.txt', 'w') as f:\n    print('w' * 10000, file=f)";

        let output = worker.eval(code);
        let written = fs::read_to_string(dir.join("out.txt")).unwrap_or_default();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        let wide = "🐍".repeat(4096);
        let cut = format!(
            "{wide}\n[1808 characters cut; print(..., limit=None) skips this cut]\n{wide}\n"
        );
        let logs = &output.logs;
        assert!(logs.starts_with(&cut), "{logs}");

        let whole = format!("\n{}\n", "y".repeat(10000));
        assert!(logs.contains(&whole));

        let short = "zzzzz\n[90 characters cut; print(..., limit=None) skips this cut]\nzzzzz\n";
        assert!(logs.ends_with(short));
        // Text files end lines with \r\n on Windows
        assert_eq!(written.trim_end(), "w".repeat(10000));

        let captured = "import contextlib, io\nbuf = io.StringIO()\nwith contextlib.redirect_stdout(buf):\n    print('c' * 10000)\nlen(buf.getvalue())";
        let captured = run(captured);
        assert_eq!(captured.logs, "10001\n");
    }

    #[test]
    fn fetch_refused() {
        let output = run("fetch('http://127.0.0.1:1/')");
        let message = error(output);
        let expected = "ValueError: fetch: http://127.0.0.1:1/: cannot connect to 127.0.0.1:1: ";
        assert!(message.starts_with(expected), "{message}");
    }

    #[test]
    fn grep_rows() {
        let (mut worker, dir) = workspace("grep");
        let code = "fn main() {}\nconst CAFÉ: u8 = 1;\n";
        write_file(&dir.join("deep/café.rs"), code.as_bytes());

        let output = worker.eval("grep('const')");
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        assert_eq!(
            output.logs,
            "[('deep/café.rs', 2, 'const CAFÉ: u8 = 1;')]\n"
        );
    }

    /// A search interrupted halfway stops in place without waiting for the worker to be killed
    #[test]
    fn search_interrupted() {
        let dir = temp_dir("slow-grep");
        // This regex takes a long time on random a's and b's that never repeat
        let mut state: u32 = 1;
        let mut text = String::new();
        for _ in 0..4000 {
            for _ in 0..1000 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let letter = if state >> 31 == 0 { 'a' } else { 'b' };
                text.push(letter);
            }
            text.push('\n');
        }
        write_file(&dir.join("random.txt"), text.as_bytes());
        let interrupt = Interrupt::default();
        let mut worker = build(interrupt.clone(), dir.clone());

        worker.eval("noted = 1");
        cancel_soon(&interrupt, 0);
        let started = Instant::now();
        let output = worker.eval("grep(r'a[ab]{30}c', 'random.txt')");
        assert_eq!(output.error, Some(CallError::Interrupted));
        assert!(started.elapsed() < STOP_WAIT, "{:?}", started.elapsed());

        interrupt.begin(1);
        let kept = worker.eval("noted");
        fs::remove_dir_all(&dir).ok();
        assert_eq!(kept.logs, "1\n");
        assert!(!worker.restarted());
    }

    #[test]
    #[cfg(unix)]
    fn grep_refused() {
        let (mut worker, dir) = workspace("locked");
        let locked = dir.join("locked.txt");
        fs::write(&locked, "secret").expect("write");
        let closed = dir.join("closed");
        fs::create_dir_all(&closed).expect("dir");
        fs::write(closed.join("inside.txt"), "secret").expect("write");
        let none = Permissions::from_mode(0o000);
        fs::set_permissions(&locked, none.clone()).expect("chmod");
        fs::set_permissions(&closed, none).expect("chmod");
        // Root reads files whatever their permissions
        if fs::read(&locked).is_ok() {
            fs::remove_dir_all(&dir).ok();
            return;
        }

        let file = worker.eval("grep('secret', 'locked.txt')");
        let file = error(file);
        let folder = worker.eval("grep('secret', 'closed')");
        let folder = error(folder);
        let skipped = worker.eval("grep('secret')").logs;
        let open = Permissions::from_mode(0o700);
        fs::set_permissions(&closed, open).expect("chmod");
        fs::remove_dir_all(&dir).ok();

        // EACCES is 2 in the guest
        let denied = "PermissionError: [Errno 2] Permission denied";
        assert_eq!(
            file.lines().next(),
            Some(format!("{denied}: 'locked.txt'").as_str())
        );
        assert_eq!(
            folder.lines().next(),
            Some(format!("{denied}: 'closed'").as_str())
        );
        assert_eq!(skipped, "[]\n");
    }

    /// Builtins fail like `open()` on paths outside the mounts and on links that lead out of them,
    /// including links the code made itself
    #[cfg(unix)]
    #[test]
    fn outside_mounts() {
        let (mut worker, dir) = workspace("mounts");
        let code = "import os\nup = '../' * os.getcwd().count('/')\n\
            os.symlink(up + 'etc', 'out')\nos.symlink(up + 'etc/hosts', 'hosts.png')\n\
            def tried(attempt):\n    try:\n        attempt()\n    except OSError as e:\n        return f'{type(e).__name__} {e.errno}'\n\
            attempts = [lambda: open('/etc/hosts'), lambda: ls('/etc'), lambda: open('out/hosts'),\n    \
                lambda: ls('out'), lambda: grep('localhost', 'out'), lambda: view('hosts.png')]\n\
            print([tried(attempt) for attempt in attempts])\nprint(ls())";
        let output = worker.eval(code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        let missing = "'FileNotFoundError 44'";
        let escaped = "'PermissionError 63'";
        let tried = format!("[{missing}, {missing}, {escaped}, {escaped}, {escaped}, {escaped}]");
        // A link out of the mounts isn't followed to tell whether it is a folder
        let expected = format!("{tried}\n['hosts.png', 'out']\n");
        assert_eq!(output.logs, expected);
    }

    #[test]
    fn view_refused() {
        let (mut worker, dir) = workspace("unviewable");
        fs::write(dir.join("latin1.txt"), b"caf\xe9").expect("write");
        fs::write(dir.join("fake.png"), "not an image".as_bytes()).expect("write");
        let big = File::create(dir.join("big.jpg")).expect("create");
        big.set_len(65).expect("size");
        fs::create_dir_all(dir.join("album.png")).expect("dir");

        let mut refused = Vec::new();
        for name in ["latin1.txt", "fake.png", "big.jpg", "album.png"] {
            let code = format!("view({name:?})");
            let output = worker.eval(&code);
            let message = error(output);
            let first = message.lines().next().unwrap_or_default().to_owned();
            refused.push(first);
        }
        let missing = worker.eval("view('missing.png')");
        let missing = error(missing);
        fs::remove_dir_all(&dir).ok();

        let unviewable = "not an image (png, jpg, jpeg, gif, webp) or video (mp4, webm, mov)";
        let expected = [
            format!("ValueError: view: latin1.txt: {unviewable}"),
            format!("ValueError: view: fake.png: {unviewable}"),
            "ValueError: view: big.jpg: 65 bytes, over the 64-byte limit for an attached image"
                .to_owned(),
            "ValueError: read: album.png: not a regular file".to_owned(),
        ];
        assert_eq!(refused, expected);
        assert!(missing.starts_with("FileNotFoundError: "), "{missing}");
        assert!(missing.contains("missing.png"), "{missing}");
    }

    /// Output that programs started by the code write to stdout counts too
    #[cfg(unix)]
    #[test]
    #[ignore = "the sandbox starts no programs until the process interface lands"]
    fn programs_run() {
        let code = "import os, subprocess\n\
            print(subprocess.run(['echo', 'café'], capture_output=True, text=True).stdout, end='')\n\
            print('before', flush=True)\n_ = os.system('echo direct')\nprint('after')";
        let output = run(code);
        assert_eq!(output.error, None);
        assert_eq!(output.logs, "café\nbefore\ndirect\nafter\n");
    }
}
