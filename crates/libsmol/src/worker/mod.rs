use crate::model::Media;
use crate::tools::ToolError;
use crate::utils::{Reply, Waiting};
use crate::worker::builtins::{builtins, value_error};
use crate::{Error, Result};
use reaper::cause;
use rustls::crypto::aws_lc_rs::{self, sign};
use rustpython_host_env::ssl::providers::CryptoExt;
use rustpython_pylib::FROZEN_STDLIB;
use rustpython_stdlib::stdlib_module_defs;
use rustpython_vm::builtins::{PyDictRef, PyTupleRef};
#[cfg(windows)]
use rustpython_vm::signal::{self, SignalNum};
use rustpython_vm::{
    py_serde, AsObject, Interpreter, PyResult, Settings, TryFromObject, VirtualMachine,
};
use serde::{Deserialize, Serialize};
use serde_json::value::Serializer;
use serde_json::{json, Map, Value};
use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;

mod builtins;
pub(crate) mod host;
pub(crate) mod sandbox;

pub const WORKER_ARG: &str = "--worker";
/// Holds the `address/token` the agent listens on
pub(crate) const CONTROL_ENV: &str = "SMOL_CONTROL";
pub(crate) const COPIES_ENV: &str = "SMOL_COPIES";
pub(crate) const END: &[u8] = b"\x02smol\x03end\n";

/// They must share one file name, which `_run` uses to drop them from tracebacks
const BOOTSTRAP: [&str; 3] = [
    include_str!("bootstrap/builtins.py"),
    include_str!("bootstrap/mcp.py"),
    include_str!("bootstrap/run.py"),
];
/// Only reserves address space until the memory is actually used
const PYTHON_STACK_BYTES: usize = 256 << 20;

static CALLS: LazyLock<Waiting> = LazyLock::new(Waiting::default);
static CONTROL: OnceLock<Mutex<TcpStream>> = OnceLock::new();
static RUNNING: Mutex<Option<u64>> = Mutex::new(None);
/// `pthread_t` is a pointer on macOS and can't go in a static
#[cfg(unix)]
static MAIN: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) header: String,
    pub(crate) summary: String,
}

/// `untagged` tries variants in order and `Note`, which ignores `id`, must come last
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Message<C, N> {
    Request {
        id: u64,
        #[serde(flatten)]
        call: C,
    },
    Answer {
        id: u64,
        result: Value,
    },
    Error {
        id: u64,
        error: String,
    },
    Note(N),
}

impl<C, N> Message<C, N> {
    pub(crate) fn reply(id: u64, result: Reply) -> Self {
        match result {
            Ok(result) => Message::Answer { id, result },
            Err(error) => Message::Error { id, error },
        }
    }
}

pub(crate) type ToWorker = Message<HostCall, HostNote>;
pub(crate) type FromWorker = Message<WorkerCall, WorkerNote>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum HostCall {
    Run {
        code: String,
    },
    AddServer {
        source: String,
        description: String,
        tools: Vec<Value>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum HostNote {
    Cancel { id: u64 },
    RemoveServer { name: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum WorkerCall {
    /// `called` is the `server.function` as written in the code
    Tool {
        server: String,
        tool: String,
        args: Map<String, Value>,
        called: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum WorkerNote {
    Ready {
        functions: Vec<Function>,
    },
    Cancel {
        id: u64,
    },
    /// Must be sent before the reply to its snippet
    Viewed {
        label: String,
        media: Media,
    },
}

pub fn serve() -> Result<()> {
    let control = env::var(CONTROL_ENV).unwrap_or_default();
    let copies = env::var_os(COPIES_ENV).unwrap_or_default();
    let copies = PathBuf::from(copies);
    // Programs the code starts would inherit these, and they must go before any thread starts
    env::remove_var(CONTROL_ENV);
    env::remove_var(COPIES_ENV);

    let Some((address, token)) = control.split_once('/') else {
        let message = format!("worker: {CONTROL_ENV}: not set by the agent");
        return Err(Error::Python(message));
    };

    let failed = |e: io::Error| {
        let reason = cause(&e);
        let message = format!("worker: {address}: {reason}");
        Error::Python(message)
    };
    let mut stream = TcpStream::connect(address).map_err(failed)?;
    let greeting = format!("{token}\n");
    stream.write_all(greeting.as_bytes()).map_err(failed)?;
    let reading = stream.try_clone().map_err(failed)?;
    CONTROL.set(Mutex::new(stream)).ok();

    // RustPython calls are stack-hungry and the default stack overflows a few thousand levels deep
    let builder = thread::Builder::new().stack_size(PYTHON_STACK_BYTES);
    let running = builder.spawn(move || -> Result<()> {
        let python = Python::start(copies);
        // SAFETY: Only reads the current thread's id
        #[cfg(unix)]
        let main = unsafe { libc::pthread_self() };
        #[cfg(unix)]
        MAIN.store(main as usize, Ordering::SeqCst);
        let (queue, inbox) = mpsc::channel();
        thread::spawn(move || listen(reading, queue));

        let listed = python.enter(functions).map_err(Error::Python)?;
        let mut stdout = io::stdout();
        stdout.write_all(END).ok();
        stdout.flush().ok();
        let ready = WorkerNote::Ready { functions: listed };
        send(&Message::Note(ready));

        for message in inbox {
            let (id, call) = match message {
                Message::Request { id, call } => (id, call),
                Message::Note(HostNote::RemoveServer { name }) => {
                    let removed = python.enter(|vm, bootstrap| {
                        let remove = bootstrap.get_item("_unserver", vm)?;
                        remove.call((name,), vm)
                    });
                    removed.ok();
                    continue;
                }
                Message::Note(HostNote::Cancel { .. })
                | Message::Answer { .. }
                | Message::Error { .. } => continue,
            };

            let result = match call {
                HostCall::Run { code } => {
                    *RUNNING.lock().expect("running lock") = Some(id);
                    let ran = python.enter(|vm, bootstrap| {
                        let invalid = |e| value_error(vm, e);
                        let run = bootstrap.get_item("_run", vm)?;
                        let result = run.call((code,), vm)?;
                        py_serde::serialize(vm, &result, Serializer).map_err(invalid)
                    });
                    *RUNNING.lock().expect("running lock") = None;

                    // The output is all written and reading this tells the agent it is complete
                    let mut stdout = io::stdout();
                    stdout.write_all(END).ok();
                    stdout.flush().ok();
                    // The agent takes an error reply to `Run` to mean the worker is gone
                    let result = match ran {
                        Ok(result) => result,
                        Err(error) => json!({ "error": error }),
                    };
                    Ok(result)
                }

                HostCall::AddServer {
                    source,
                    description,
                    tools,
                } => python.enter(|vm, bootstrap| {
                    let invalid = |e| value_error(vm, e);
                    let tools = Value::Array(tools);
                    let tools = py_serde::deserialize(vm, tools).map_err(invalid)?;
                    let add = bootstrap.get_item("_server", vm)?;
                    let installed = add.call((source, description, tools), vm)?;
                    py_serde::serialize(vm, &installed, Serializer).map_err(invalid)
                }),
            };
            let reply = Message::reply(id, result);
            send(&reply);
        }
        Ok(())
    });
    let running = running.map_err(failed)?;
    running.join().expect("python thread")
}

/// Cancels and replies are handled here, where they arrive even while the code thread is busy
fn listen(stream: TcpStream, queue: Sender<ToWorker>) {
    let lines = BufReader::new(stream).lines();
    for line in lines.map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<ToWorker>(&line) else {
            continue;
        };
        match message {
            Message::Note(HostNote::Cancel { id }) => {
                let running = *RUNNING.lock().expect("running lock");
                if running == Some(id) {
                    interrupt();
                }
            }
            Message::Answer { id, result } => CALLS.answer(id, Ok(result)),
            Message::Error { id, error } => CALLS.answer(id, Err(error)),
            other => {
                queue.send(other).ok();
            }
        }
    }
    // The agent hung up and the running code need not finish
    process::exit(0);
}

/// Like Ctrl-C, it also wakes code blocked in a system call
#[cfg(unix)]
fn interrupt() {
    let main = MAIN.load(Ordering::SeqCst) as libc::pthread_t;
    // SAFETY: The code thread lives until the process exits
    unsafe { libc::pthread_kill(main, libc::SIGINT) };
}

/// Code blocked in a system call stays blocked and the agent drops the worker after a while
#[cfg(windows)]
fn interrupt() {
    // SIGINT is 2 on Windows too
    if let Ok(sigint) = SignalNum::try_from(2) {
        signal::set_interrupt_ex(sigint).ok();
    }
}

fn send(message: &FromWorker) {
    let mut line = serde_json::to_string(message).expect("serialize");
    line.push('\n');
    let control = CONTROL.get().expect("control");
    let mut stream = control.lock().expect("control lock");
    stream.write_all(line.as_bytes()).ok();
}

async fn request(call: WorkerCall) -> Result<Value, ToolError> {
    let (id, replied) = CALLS.ask();
    send(&Message::Request { id, call });
    let _unanswered = Unanswered(id);

    let result = replied.await.expect("reply");
    result.map_err(ToolError::Failed)
}

/// Dropping it before the reply arrives stops the call on the agent's side too
struct Unanswered(u64);

impl Drop for Unanswered {
    fn drop(&mut self) {
        if CALLS.take(self.0).is_some() {
            let cancel = WorkerNote::Cancel { id: self.0 };
            send(&Message::Note(cancel));
        }
    }
}

struct Python {
    interpreter: Interpreter,
    bootstrap: PyDictRef,
}

impl Python {
    fn start(copies: PathBuf) -> Self {
        let crypto = CryptoExt {
            all_cipher_suites: Some(aws_lc_rs::ALL_CIPHER_SUITES),
            default_cipher_suites: Some(aws_lc_rs::DEFAULT_CIPHER_SUITES),
            all_kx_groups: Some(aws_lc_rs::ALL_KX_GROUPS),
            any_supported_key: Some(sign::any_supported_type),
            ticketer: aws_lc_rs::Ticketer::new,
        };
        let provider = aws_lc_rs::default_provider();
        CryptoExt::set_provider(provider, crypto).ok();

        let mut settings = Settings::default();
        settings.utf8_mode = 1;
        settings.path_list.push(String::new());
        // argparse and runpy both read `sys.argv[0]`
        settings.argv.push(String::new());

        let builder = Interpreter::builder(settings);
        let stdlib = stdlib_module_defs(builder.ctx.as_ref());
        let interpreter = builder
            .add_native_modules(&stdlib)
            .add_frozen_modules(FROZEN_STDLIB)
            .build();

        let bootstrap = interpreter.enter(|vm| {
            let tools = builtins(vm, copies);

            let scope = vm.new_scope_with_builtins();
            for (name, value) in tools {
                let set = scope.globals.set_item(name, value, vm);
                set.expect("set global");
            }

            for source in BOOTSTRAP {
                let ran = vm.run_string(scope.clone(), source, "<libsmol_bootstrap>");
                if let Err(err) = ran {
                    vm.print_exception(&err);
                    panic!("bootstrap failed");
                }
            }

            scope.globals
        });

        Self {
            interpreter,
            bootstrap,
        }
    }

    fn enter<T>(
        &self,
        f: impl FnOnce(&VirtualMachine, &PyDictRef) -> PyResult<T>,
    ) -> Result<T, String> {
        self.interpreter.enter(|vm| {
            // An interrupt that arrived after the previous snippet finished isn't for this one
            vm.check_signals().ok();
            let err = match f(vm, &self.bootstrap) {
                Ok(value) => return Ok(value),
                Err(err) => err,
            };
            let class = err.class().name().to_string();
            let message = err.as_object().str(vm);
            let message = message.map(|text| text.to_string()).unwrap_or_default();
            match message.is_empty() {
                true => Err(class),
                false => Err(format!("{class}: {message}")),
            }
        })
    }
}

fn functions(vm: &VirtualMachine, bootstrap: &PyDictRef) -> PyResult<Vec<Function>> {
    let listed = bootstrap.get_item("_FUNCTIONS", vm)?;
    let listed = Vec::<PyTupleRef>::try_from_object(vm, listed)?;

    let mut functions = Vec::new();
    for item in listed {
        let (name, header, summary) = item.extract_tuple(vm)?;
        functions.push(Function {
            name,
            header,
            summary,
        });
    }
    Ok(functions)
}
