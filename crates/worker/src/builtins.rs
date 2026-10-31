use crate::{json_of, python_of, request, send};
use libsmol::guest::{
    block_on, fetch, grep, ls, shown_path, unserializable, viewed, AgentCall, Message, ToolError,
    WorkerCall, WorkerNote,
};
use pyo3::exceptions::{PyOSError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict};
use reaper::described;
use serde_json::{json, Map};
use std::env;
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time;

const CHECK_EVERY: Duration = Duration::from_millis(50);

/// The copies folder and the session directory, which `view` labels paths against
static PLACES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

pub(crate) fn builtins(
    py: Python<'_>,
    copies: PathBuf,
) -> PyResult<[(&'static str, Bound<'_, PyCFunction>); 6]> {
    // No code has run yet at startup and this is still the session directory
    let cwd = env::current_dir().unwrap_or_default();
    PLACES.set((copies, cwd)).ok();

    Ok([
        ("_ls", wrap_pyfunction!(list, py)?),
        ("_grep", wrap_pyfunction!(search, py)?),
        ("_fetch", wrap_pyfunction!(get, py)?),
        ("_call", wrap_pyfunction!(call_tool, py)?),
        ("_agent", wrap_pyfunction!(delegate, py)?),
        ("_view", wrap_pyfunction!(view, py)?),
    ])
}

/// Paths use the filesystem encoding, matching `os.listdir` and `open()` on non-UTF-8 names
#[pyfunction]
fn list(py: Python<'_>, path: PathBuf, recursive: bool) -> PyResult<Vec<OsString>> {
    let listed = blocking(py, move |stop| ls(&path, recursive, stop))?;
    finish(py, listed)
}

#[pyfunction]
#[pyo3(signature = (pattern, path, glob))]
fn search(
    py: Python<'_>,
    pattern: String,
    path: PathBuf,
    glob: Option<String>,
) -> PyResult<Vec<(OsString, u64, String)>> {
    let searching = move |stop: &AtomicBool| grep(&pattern, &path, glob.as_deref(), stop);
    let found = blocking(py, searching)?;
    finish(py, found)
}

#[pyfunction]
fn get(py: Python<'_>, url: String) -> PyResult<String> {
    let fetching = fetch(&url);
    let page = wait(py, fetching)?;
    finish(py, page)
}

#[pyfunction]
fn call_tool<'py>(
    py: Python<'py>,
    server: String,
    tool: String,
    args: Bound<'py, PyDict>,
    called: String,
) -> PyResult<Bound<'py, PyAny>> {
    let mut arguments = Map::new();
    for (key, value) in args {
        let key = key.str()?.to_string();
        let json = match json_of(&value) {
            Ok(json) => json,
            Err(_) => {
                let kind = value.get_type().name()?.to_string();
                let message = unserializable(&key, &kind);
                return Err(PyTypeError::new_err(message));
            }
        };
        arguments.insert(key, json);
    }

    let call = WorkerCall::Tool {
        server,
        tool,
        args: arguments,
        called,
    };
    asked(py, call)
}

#[pyfunction]
fn delegate<'py>(
    py: Python<'py>,
    method: String,
    params: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let params = json_of(&params)?;
    let call = json!({ "method": method, "params": params });
    let call: AgentCall = serde_json::from_value(call).map_err(value_error)?;
    asked(py, WorkerCall::Agent(call))
}

#[pyfunction]
fn view(py: Python<'_>, path: PathBuf, absolute: PathBuf) -> PyResult<()> {
    let (copies, cwd) = PLACES.get().expect("places");
    let copies = copies.clone();
    let copied = blocking(py, move |_| viewed(&path, &copies))?;
    let media = finish(py, copied)?;

    let label = shown_path(&absolute, cwd);
    let note = WorkerNote::Viewed { label, media };
    send(&Message::Note(note));
    Ok(())
}

fn asked(py: Python<'_>, call: WorkerCall) -> PyResult<Bound<'_, PyAny>> {
    let calling = request(call);
    let result = wait(py, calling)?;
    let result = finish(py, result)?;
    python_of(py, &result)
}

/// Runs the work on another thread while this one checks for interrupts and sets `stop` on one
fn blocking<T: Send + 'static>(
    py: Python<'_>,
    work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> PyResult<T> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let (done, receiver) = oneshot::channel();
    thread::spawn(move || {
        let result = work(&stopping);
        done.send(result).ok();
    });

    let waited = wait(py, receiver);
    stop.store(true, Ordering::Relaxed);
    let result = waited?;
    Ok(result.expect("blocking"))
}

/// An interrupt drops `future`, which also cancels its call to the agent
fn wait<T: Send>(py: Python<'_>, future: impl Future<Output = T> + Send) -> PyResult<T> {
    let mut future = pin!(future);
    loop {
        let waiting = async { time::timeout(CHECK_EVERY, &mut future).await };
        let waited = py.detach(|| block_on(waiting));
        match waited {
            Ok(done) => return Ok(done),
            Err(_) => py.check_signals()?,
        }
    }
}

fn finish<T>(py: Python<'_>, result: Result<T, ToolError>) -> PyResult<T> {
    // An interrupt that arrived while blocked is raised here
    py.check_signals()?;

    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };

    // Python picks the `OSError` subclass from the errno, or on Windows from the error code
    if let ToolError::Os { path, source, .. } = &error {
        if let Some(code) = source.raw_os_error() {
            let strerror = described(source);
            #[cfg(unix)]
            let args = (code, strerror, path.clone());
            #[cfg(windows)]
            let args = (0, strerror, path.clone(), code);
            return Err(PyOSError::new_err(args));
        }
    }

    Err(value_error(error))
}

pub(crate) fn value_error(error: impl Display) -> PyErr {
    let message = error.to_string();
    PyValueError::new_err(message)
}
