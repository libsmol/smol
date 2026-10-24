use crate::prompt;
use crate::tools::attach::{shown_path, viewed};
use crate::tools::files::{grep, ls};
use crate::tools::web::fetch;
use crate::tools::ToolError;
use crate::utils::block_on;
use crate::worker::{request, send, Message, WorkerCall, WorkerNote};
use reaper::described;
use rustpython_host_env::os::ErrorExt;
use rustpython_vm::builtins::{PyBaseExceptionRef, PyDictRef, PyListRef, PyStrRef};
use rustpython_vm::{py_serde, AsObject, PyObjectRef, PyResult, VirtualMachine};
use serde_json::value::Serializer;
use serde_json::Map;
use std::env;
use std::fmt::Display;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time;

const CHECK_EVERY: Duration = Duration::from_millis(50);

pub(crate) fn builtins(vm: &VirtualMachine, copies: PathBuf) -> [(&'static str, PyObjectRef); 5] {
    // Paths use the filesystem encoding, matching `os.listdir` and `open()` on non-UTF-8 names
    let list = |path: PyStrRef, recursive: bool, vm: &VirtualMachine| -> PyResult<PyListRef> {
        let path = vm.fsencode(&path)?;
        let path = PathBuf::from(&*path);
        let listed = blocking(vm, move |stop| ls(&path, recursive, stop))?;
        let names = finish(vm, listed)?;

        let mut items = Vec::new();
        for name in names {
            let name = vm.fsdecode(name);
            items.push(name.into());
        }
        let names = vm.ctx.new_list(items);
        Ok(names)
    };

    let search = |pattern: String, path: PyStrRef, glob: Option<String>, vm: &VirtualMachine| {
        let path = vm.fsencode(&path)?;
        let path = PathBuf::from(&*path);
        let searching = move |stop: &AtomicBool| grep(&pattern, &path, glob.as_deref(), stop);
        let found = blocking(vm, searching)?;
        let found = finish(vm, found)?;

        let mut rows = Vec::new();
        for (path, line, text) in found {
            let path = vm.fsdecode(path);
            let line = vm.ctx.new_int(line);
            let text = vm.ctx.new_str(text);
            let row = vec![path.into(), line.into(), text.into()];
            let row = vm.ctx.new_tuple(row);
            rows.push(row.into());
        }
        let rows = vm.ctx.new_list(rows);
        PyResult::Ok(rows)
    };

    let get = |url: String, vm: &VirtualMachine| {
        let fetching = fetch(&url);
        let page = wait(vm, fetching)?;
        finish(vm, page)
    };

    let call_tool =
        |server: String, tool: String, args: PyDictRef, called: String, vm: &VirtualMachine| {
            let mut arguments = Map::new();
            for (key, value) in args {
                let key = key.str(vm)?.to_string();
                let json = match py_serde::serialize(vm, &value, Serializer) {
                    Ok(json) => json,
                    Err(_) => {
                        let kind = value.class().name().to_owned();
                        let message = prompt::unserializable(&key, &kind);
                        return Err(vm.new_type_error(message));
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
            let calling = request(call);
            let result = wait(vm, calling)?;
            let result = finish(vm, result)?;
            py_serde::deserialize(vm, result).map_err(|e| value_error(vm, e))
        };

    // No code has run yet at startup and this is still the session directory
    let cwd = env::current_dir().unwrap_or_default();
    let view = move |path: PyStrRef, absolute: PyStrRef, vm: &VirtualMachine| {
        let path = vm.fsencode(&path)?;
        let path = PathBuf::from(&*path);
        let copies = copies.clone();
        let copied = blocking(vm, move |_| viewed(&path, &copies))?;
        let media = finish(vm, copied)?;

        let absolute = vm.fsencode(&absolute)?;
        let absolute = Path::new(&*absolute);
        let label = shown_path(absolute, &cwd);
        let note = WorkerNote::Viewed { label, media };
        send(&Message::Note(note));
        PyResult::Ok(())
    };

    let list = vm.new_function("_ls", list);
    let search = vm.new_function("_grep", search);
    let get = vm.new_function("_fetch", get);
    let call_tool = vm.new_function("_call", call_tool);
    let view = vm.new_function("_view", view);

    [
        ("_ls", list.into()),
        ("_grep", search.into()),
        ("_fetch", get.into()),
        ("_call", call_tool.into()),
        ("_view", view.into()),
    ]
}

/// Runs the work on another thread while this one checks for interrupts and sets `stop` on one
fn blocking<T: Send + 'static>(
    vm: &VirtualMachine,
    work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> PyResult<T> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let (done, receiver) = oneshot::channel();
    thread::spawn(move || {
        let result = work(&stopping);
        done.send(result).ok();
    });

    let waited = wait(vm, receiver);
    stop.store(true, Ordering::Relaxed);
    let result = waited?;
    Ok(result.expect("blocking"))
}

/// An interrupt drops `future`, which also cancels its call to the agent
fn wait<T>(vm: &VirtualMachine, future: impl Future<Output = T>) -> PyResult<T> {
    let mut future = pin!(future);
    loop {
        let waiting = async { time::timeout(CHECK_EVERY, &mut future).await };
        let waited = vm.allow_threads(|| block_on(waiting));
        match waited {
            Ok(done) => return Ok(done),
            Err(_) => vm.check_signals()?,
        }
    }
}

fn finish<T>(vm: &VirtualMachine, result: Result<T, ToolError>) -> PyResult<T> {
    // An interrupt that arrived while blocked is raised here
    vm.check_signals()?;

    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };

    // Python picks the `OSError` subclass from the errno, and Windows error codes are not errnos
    if let ToolError::Os { path, source, .. } = &error {
        if source.raw_os_error().is_some() {
            let errno = source.posix_errno();
            let strerror = described(source);
            let class = vm.ctx.exceptions.os_error.as_object();
            let args = (errno, strerror, path.clone());
            let raised = class.call(args, vm)?;
            let exception = raised.downcast().expect("exception");
            return Err(exception);
        }
    }

    Err(value_error(vm, error))
}

pub(crate) fn value_error(vm: &VirtualMachine, error: impl Display) -> PyBaseExceptionRef {
    let message = error.to_string();
    vm.new_value_error(message)
}
