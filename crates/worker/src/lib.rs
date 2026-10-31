use crate::builtins::{builtins, value_error};
use libsmol::guest::{
    block_on, FromWorker, Function, HostCall, HostNote, Message, ToWorker, ToolError, Waiting,
    WorkerCall, WorkerNote, AGENTS_ENV, CONTROL_ENV, COPIES_ENV, DONE, END, MAX_PARALLEL_AGENTS,
};
#[cfg(target_os = "macos")]
use libsmol::guest::{confine, SANDBOX_ENV};
use pyo3::exceptions::PyRuntimeError;
#[cfg(windows)]
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyDict};
use reaper::cause;
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
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

#[pymodule]
fn _smol(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let serving = wrap_pyfunction!(serve, module)?;
    module.add_function(serving)?;
    let repairing = wrap_pyfunction!(repair, module)?;
    module.add_function(repairing)
}

/// Runs on the main thread, where Python raises `KeyboardInterrupt`, until the agent hangs up
#[pyfunction]
fn serve(py: Python<'_>, python: OsString) -> PyResult<()> {
    let environ = py.import("os")?.getattr("environ")?;
    confined(&environ)?;
    // Programs the code starts would inherit these, and they must go before any thread starts
    let control = taken(&environ, CONTROL_ENV)?;
    let copies = taken(&environ, COPIES_ENV)?;
    let agents = taken(&environ, AGENTS_ENV)?;

    // Runs as the venv its own interpreter would run, with sys set before site reads it
    let venv = venv_of(&python);
    let sys = py.import("sys")?;
    sys.setattr("executable", &python)?;
    sys.setattr("prefix", &venv)?;
    sys.setattr("exec_prefix", &venv)?;
    py.import("site")?.call_method0("main")?;
    // Like `python -c`, which also makes argparse and runpy read `sys.argv[0]`
    sys.getattr("path")?.call_method1("insert", (0, ""))?;
    sys.setattr("argv", [""])?;

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
    let greeting = format!("{token}\n");
    stream.write_all(greeting.as_bytes()).map_err(failed)?;
    let reading = stream.try_clone().map_err(failed)?;
    CONTROL.set(Mutex::new(stream)).ok();

    let globals = PyDict::new(py);
    let copies = PathBuf::from(copies);
    for (name, function) in builtins(py, copies)? {
        globals.set_item(name, function)?;
    }
    globals.set_item("_AGENTS", agents)?;
    globals.set_item("_MAX_AGENTS", MAX_PARALLEL_AGENTS)?;
    globals.set_item("_DONE", DONE)?;
    let python = py.import("builtins")?;
    let compile = python.getattr("compile")?;
    let exec = python.getattr("exec")?;
    for source in BOOTSTRAP {
        let code = compile.call1((source, "<libsmol_bootstrap>", "exec"))?;
        exec.call1((code, &globals))?;
    }
    let scope = globals.as_any();
    let run = scope.get_item("_run")?;
    let server = scope.get_item("_server")?;
    let unserver = scope.get_item("_unserver")?;
    let listed: Vec<(String, String, String)> = scope.get_item("_FUNCTIONS")?.extract()?;

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
    ended();
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
                let ran = run.call1((code,)).and_then(|result| json_of(&result));
                *RUNNING.lock().expect("running lock") = None;

                // The output is all written and reading this tells the agent it is complete
                ended();
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
                description,
                tools,
            } => {
                let tools = Value::Array(tools);
                let installed = python_of(py, &tools)
                    .and_then(|tools| server.call1((source, description, tools)))
                    .and_then(|installed| json_of(&installed));
                installed.map_err(|error| shown(py, &error))
            }
        };
        let reply = Message::reply(id, result);
        send(&reply);
    }
}

/// Makes the venv of `python`, or adds what it lacks and keeps the packages it has,
/// confined like the worker since the code may be using it
#[pyfunction]
fn repair(py: Python<'_>, python: OsString) -> PyResult<()> {
    let environ = py.import("os")?.getattr("environ")?;
    confined(&environ)?;
    let venv = venv_of(&python);
    py.import("venv")?.call_method1("main", ([venv],))?;
    Ok(())
}

/// The venv holding the interpreter in its `bin` or `Scripts` folder
fn venv_of(python: &OsStr) -> OsString {
    let python = Path::new(python);
    let venv = python.parent().and_then(Path::parent).unwrap_or(python);
    venv.as_os_str().to_owned()
}

/// Takes the Seatbelt profile from the environment and applies it,
/// before anything the code could have written runs
#[cfg(target_os = "macos")]
fn confined(environ: &Bound<'_, PyAny>) -> PyResult<()> {
    let profile = taken(environ, SANDBOX_ENV)?;
    if !profile.is_empty() {
        confine(&profile).map_err(PyRuntimeError::new_err)?;
    }
    Ok(())
}

/// Linux confines the worker before it starts and Windows not at all
#[cfg(not(target_os = "macos"))]
fn confined(_: &Bound<'_, PyAny>) -> PyResult<()> {
    Ok(())
}

/// Also unsets it for the programs the code starts
fn taken(environ: &Bound<'_, PyAny>, name: &str) -> PyResult<String> {
    let value = environ.call_method1("pop", (name, ""))?;
    value.extract()
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
    // The agent hung up and the running code need not finish.
    // SAFETY: Ends the process at once, without running C extensions' cleanup under the code
    unsafe { libc::_exit(0) }
}

/// Like Ctrl-C, it also wakes code blocked in a system call
#[cfg(unix)]
fn interrupt() {
    let main = MAIN.load(Ordering::SeqCst) as libc::pthread_t;
    // SAFETY: The code thread lives until the process exits
    unsafe { libc::pthread_kill(main, libc::SIGINT) };
}

#[cfg(windows)]
fn interrupt() {
    // SAFETY: Python allows it from any thread without the GIL, and SIGINT is 2 on Windows too
    unsafe { ffi::PyErr_SetInterruptEx(2) };
}

/// Marks the end of the output on the pipe the code also writes to
fn ended() {
    // C extensions print through C's stdio, which holds what goes to a pipe until flushed.
    // SAFETY: A null stream flushes every open one
    unsafe { libc::fflush(ptr::null_mut()) };
    let mut stdout = io::stdout();
    stdout.write_all(END).ok();
    stdout.flush().ok();
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
    let strict = [("allow_nan", false)].into_py_dict(py)?;
    let text: String = dumps.call((value,), Some(&strict))?.extract()?;
    serde_json::from_str(&text).map_err(value_error)
}

fn python_of<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    let text = value.to_string();
    let loads = py.import("json")?.getattr("loads")?;
    loads.call1((text,))
}
