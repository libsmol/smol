use crate::agent::MAX_PARALLEL_AGENTS;
use crate::prompt::{DONE, NO_AGENT, NO_MEMORY, SURROGATE};
use crate::steps::MAX_PRINT_CHARS;
use crate::tools::memories::{MAX_MEMORY_CHARS, MAX_RECALLED};
use crate::tools::ToolError;
use crate::utils::{block_on, Waiting};
use crate::worker::guest::builtins::{builtins, value_error};
use crate::worker::{
    end_marker, greeting, FromWorker, Function, HostCall, HostNote, Message, ToWorker, WorkerCall,
    WorkerNote, AGENTS_ENV, CONTROL_ENV, COPIES_ENV, WORKER_ARG,
};
use pyo3::exceptions::{PyKeyboardInterrupt, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyDict};
use reaper::cause;
use serde_json::{json, Value};
use std::env;
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::ptr;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;
use tokio::sync::mpsc::{self, UnboundedSender};

mod builtins;

/// They must share one file name, which `_run` uses to drop them from tracebacks
const BOOTSTRAP: [&str; 3] = [
    include_str!("bootstrap/builtins.py"),
    include_str!("bootstrap/mcp.py"),
    include_str!("bootstrap/run.py"),
];

static CALLS: LazyLock<Waiting> = LazyLock::new(Waiting::default);
static CONTROL: OnceLock<Mutex<TcpStream>> = OnceLock::new();
static RUNNING: Mutex<Option<u64>> = Mutex::new(None);
/// `pthread_t` is a pointer on macOS and can't go in a static
#[cfg(unix)]
static MAIN: AtomicUsize = AtomicUsize::new(0);

/// Runs as a session's worker when started as one with `args`, and returns its exit status
pub(crate) fn main(args: &[OsString]) -> Option<i32> {
    let [_, flag, venv] = args else {
        return None;
    };
    if flag != WORKER_ARG {
        return None;
    }

    // Started through `/proc/self/exe`, the process would show as `exe`
    #[cfg(target_os = "linux")]
    {
        let exe = worker::executable();
        let own = exe.file_name().unwrap_or_default();
        if let Ok(name) = CString::new(own.as_encoded_bytes()) {
            // SAFETY: Renames only the calling thread, which is the process's main one
            unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) };
        }
    }
    let serving = |py: Python<'_>| serve(py, Path::new(venv));
    let status = worker::isolated(serving);
    Some(status)
}

/// Runs on the main thread, where Python raises `KeyboardInterrupt`, until the agent hangs up
fn serve(py: Python<'_>, venv: &Path) -> PyResult<()> {
    let os = py.import("os")?;
    let environ = os.getattr("environ")?;
    // Programs the code starts would inherit these, and they must go before any thread starts
    let control: String = taken(&environ, CONTROL_ENV)?.extract()?;
    let copies: OsString = taken(&environ, COPIES_ENV)?.extract()?;
    let agents: String = taken(&environ, AGENTS_ENV)?.extract()?;

    // Taken before any code can close fd 1
    // Python's os makes it because a Windows fd belongs to the C runtime that opened it
    let output: i32 = os.call_method1("dup", (1,))?.extract()?;
    // Bound now, as the code may replace what `os` holds
    let write = os.getattr("write")?;

    // Python leaves SIGINT ignored when libsmol was started with it ignored, and no interrupt would land
    let signal = py.import("signal")?;
    let sigint = signal.getattr("SIGINT")?;
    let handler = signal.getattr("default_int_handler")?;
    signal.call_method1("signal", (sigint, handler))?;

    // Taken before site runs the venv's .pth files, which could change it
    let cwd = env::current_dir().unwrap_or_default();
    worker::enter(py, venv)?;

    let Some((address, token)) = control.split_once('/') else {
        let message = format!("worker: {CONTROL_ENV}: not set by the agent");
        return Err(PyRuntimeError::new_err(message));
    };
    let failed = |e: io::Error| {
        let reason = cause(&e);
        let message = format!("worker: {address}: {reason}");
        PyRuntimeError::new_err(message)
    };
    let mut stream = TcpStream::connect(address).map_err(failed)?;
    let mut line = greeting(token);
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(failed)?;
    let reading = stream.try_clone().map_err(failed)?;
    CONTROL.set(Mutex::new(stream)).ok();
    let marker = end_marker(token);

    let globals = PyDict::new(py);
    let copies = PathBuf::from(copies);
    for (name, function) in builtins(py, copies, cwd)? {
        globals.set_item(name, function)?;
    }
    globals.set_item("_OUTPUT", output)?;
    globals.set_item("_AGENTS", agents)?;
    globals.set_item("_MAX_AGENTS", MAX_PARALLEL_AGENTS)?;
    globals.set_item("_MAX_RECALLED", MAX_RECALLED)?;
    globals.set_item("_MAX_MEMORY_CHARS", MAX_MEMORY_CHARS)?;
    globals.set_item("_MAX_PRINT_CHARS", MAX_PRINT_CHARS)?;
    globals.set_item("_DONE", DONE)?;
    globals.set_item("_NO_AGENT", NO_AGENT)?;
    globals.set_item("_NO_MEMORY", NO_MEMORY)?;

    let builtin = py.import("builtins")?;
    let compile = builtin.getattr("compile")?;
    let exec = builtin.getattr("exec")?;
    for source in BOOTSTRAP {
        let code = compile.call1((source, "<libsmol_bootstrap>", "exec"))?;
        exec.call1((code, &globals))?;
    }

    let scope = globals.as_any();
    let run = scope.get_item("_run")?;
    let server = scope.get_item("_server")?;
    let unserver = scope.get_item("_unserver")?;
    let listed: Vec<(String, String, String)> = scope.get_item("_FUNCTIONS")?.extract()?;

    // Added earlier, a file in the working directory named like a standard module breaks the bootstrap
    let sys = py.import("sys")?;
    sys.getattr("path")?.call_method1("insert", (0, ""))?;
    // argparse and runpy read it, and it is empty as in the interactive interpreter
    sys.setattr("argv", [""])?;

    // SAFETY: Only reads the current thread's id
    #[cfg(unix)]
    let main = unsafe { libc::pthread_self() };
    #[cfg(unix)]
    MAIN.store(main as usize, Ordering::SeqCst);
    let (queue, mut inbox) = mpsc::unbounded_channel();
    thread::spawn(move || listen(reading, queue));

    let mut functions = Vec::new();
    for (name, header, summary) in listed {
        functions.push(Function {
            name,
            header,
            summary,
        });
    }
    ended(&write, output, &marker);
    let ready = WorkerNote::Ready { functions };
    send(&Message::Note(ready));

    loop {
        // Threads the code started run between snippets only while this waits without the GIL
        let receiving = inbox.recv();
        let Some(message) = py.detach(|| block_on(receiving)) else {
            return Ok(());
        };

        // An interrupt that arrived after the previous snippet finished isn't for this one
        py.check_signals().ok();
        let (id, call) = match message {
            Message::Request { id, call } => (id, call),
            Message::Note(HostNote::RemoveServer { name }) => {
                unserver.call1((name,)).ok();
                continue;
            }
            Message::Note(HostNote::Cancel { .. })
            | Message::Answer { .. }
            | Message::Error { .. } => continue,
        };

        let result = match call {
            HostCall::Run { code } => {
                *RUNNING.lock().expect("running lock") = Some(id);
                let ran = run.call1((code,));
                // Cleared first, since an interrupt while the result converts would lose it
                *RUNNING.lock().expect("running lock") = None;
                let ran = ran.and_then(|result| json_of(&result));

                // The output is all written and reading this tells the agent it is complete
                ended(&write, output, &marker);
                // The agent takes an error reply to `Run` to mean the worker is gone
                let result = match ran {
                    Ok(result) => result,
                    Err(error) => {
                        let error = shown(py, &error);
                        json!({ "error": error })
                    }
                };
                Ok(result)
            }

            HostCall::AddServer {
                source,
                name,
                description,
                tools,
            } => {
                let tools = Value::Array(tools);
                let installed = python_of(py, &tools)
                    .and_then(|tools| server.call1((source, name, description, tools)))
                    .and_then(|installed| json_of(&installed));
                installed.map_err(|error| shown(py, &error))
            }
        };
        let reply = Message::reply(id, result);
        send(&reply);
    }
}

/// Also unsets it for the programs the code starts
fn taken<'py>(environ: &Bound<'py, PyAny>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    environ.call_method1("pop", (name, ""))
}

/// Cancels and replies are handled here, where they arrive even while the code thread is busy
fn listen(stream: TcpStream, queue: UnboundedSender<ToWorker>) {
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

    // SAFETY: The agent hung up, so the process ends at once
    // without waiting for the code or running C extensions' cleanup under it
    unsafe { libc::_exit(0) }
}

/// Like Ctrl-C, it also wakes code blocked in a system call
#[cfg(unix)]
fn interrupt() {
    let main = MAIN.load(Ordering::SeqCst) as libc::pthread_t;
    // SAFETY: The code thread lives until the process exits
    unsafe { libc::pthread_kill(main, libc::SIGINT) };
}

/// Like Ctrl-C, it also wakes CPython's own waits, which the interrupt flag alone leaves asleep
#[cfg(windows)]
fn interrupt() {
    // SAFETY: The C runtime runs Python's handler on this thread, which only sets flags and an event
    unsafe { libc::raise(libc::SIGINT) };
}

/// Marks the end of the output on `output`, the worker's own copy of the pipe the code writes to
fn ended(write: &Bound<'_, PyAny>, output: i32, marker: &[u8]) {
    // C extensions print through C's stdio, which holds what goes to a pipe until flushed
    // SAFETY: A null stream flushes every open one
    unsafe { libc::fflush(ptr::null_mut()) };
    // A SIGINT that lands while the write waits on a full pipe raises here
    // A pipe takes a write this small whole or not at all
    let py = write.py();
    loop {
        match write.call1((output, marker)) {
            Err(error) if error.is_instance_of::<PyKeyboardInterrupt>(py) => continue,
            _ => break,
        }
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

/// `Class: message`, or the class alone when the message is empty
fn shown(py: Python<'_>, error: &PyErr) -> String {
    let class = error.get_type(py).name();
    let class = class.map(|name| name.to_string()).unwrap_or_default();
    let message = error.value(py).to_string();
    match message.is_empty() {
        true => class,
        false => format!("{class}: {message}"),
    }
}

/// Raises `TypeError` or `ValueError`, as `json.dumps` does, for values JSON can't hold
fn json_of(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    let py = value.py();
    let dumps = py.import("json")?.getattr("dumps")?;
    let strict = [("allow_nan", false), ("ensure_ascii", false)].into_py_dict(py)?;
    let dumped = dumps.call((value,), Some(&strict))?;
    // Only lone surrogates keep a str from becoming UTF-8
    let Ok(text) = dumped.extract::<String>() else {
        return Err(value_error(SURROGATE));
    };
    serde_json::from_str(&text).map_err(value_error)
}

fn python_of<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    let text = value.to_string();
    let loads = py.import("json")?.getattr("loads")?;
    loads.call1((text,))
}
