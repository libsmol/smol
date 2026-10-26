use crate::prompt;
use crate::smol::host::tools::{self, ToolError};
use rustpython_vm::builtins::{PyBaseExceptionRef, PyDictRef, PyListRef, PyStrRef};
use rustpython_vm::convert::ToPyException;
use rustpython_vm::signal;
use rustpython_vm::{py_serde, AsObject, PyObjectRef, PyResult, VirtualMachine};
use serde_json::value::Serializer;
use serde_json::{Map, Value};
use std::env;
use std::fmt::Display;

pub(crate) fn builtins(vm: &VirtualMachine) -> [(&'static str, PyObjectRef); 5] {
    let list = |path: PyStrRef, recursive: bool, vm: &VirtualMachine| -> PyResult<PyListRef> {
        let cwd = current_dir(vm)?;
        let path = path.to_string_lossy();
        let listed = tools::ls(&cwd, &path, recursive);
        let names = finish(vm, listed)?;

        let mut items = Vec::new();
        for name in names {
            let name = vm.ctx.new_str(name);
            items.push(name.into());
        }
        let names = vm.ctx.new_list(items);
        Ok(names)
    };

    let search = |pattern: String, path: PyStrRef, glob: Option<String>, vm: &VirtualMachine| {
        let cwd = current_dir(vm)?;
        let path = path.to_string_lossy();
        let found = tools::grep(&cwd, &pattern, &path, glob.as_deref());
        let found = finish(vm, found)?;

        let mut rows = Vec::new();
        for found in found {
            let path = vm.ctx.new_str(found.path);
            let line = vm.ctx.new_int(found.line);
            let text = vm.ctx.new_str(found.text);
            let row = vec![path.into(), line.into(), text.into()];
            let row = vm.ctx.new_tuple(row);
            rows.push(row.into());
        }
        let rows = vm.ctx.new_list(rows);
        PyResult::Ok(rows)
    };

    let get = |url: String, vm: &VirtualMachine| {
        let page = tools::fetch(&url);
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

            let arguments = Value::Object(arguments).to_string();
            let reply = tools::call(&server, &tool, &arguments, &called);
            let reply = finish(vm, reply)?;
            let invalid = |e| value_error(vm, e);
            let result: Value = serde_json::from_str(&reply).map_err(invalid)?;
            py_serde::deserialize(vm, result).map_err(invalid)
        };

    let view = |path: PyStrRef, vm: &VirtualMachine| {
        let cwd = current_dir(vm)?;
        let path = path.to_string_lossy();
        let viewed = tools::view(&cwd, &path);
        finish(vm, viewed)
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

/// The directory the code last changed to, which the host doesn't track
fn current_dir(vm: &VirtualMachine) -> PyResult<String> {
    let cwd = env::current_dir().map_err(|e| e.to_pyexception(vm))?;
    let cwd = cwd.to_string_lossy();
    Ok(cwd.into_owned())
}

fn finish<T>(vm: &VirtualMachine, result: Result<T, ToolError>) -> PyResult<T> {
    // An interrupt that arrived during the call is raised here
    signal::slice_ended(vm)?;

    match result {
        Ok(value) => Ok(value),
        // `OSError` picks its subclass from the errno
        Err(ToolError::Os(error)) => {
            let codes = vm.import("errno", 0)?;
            let name = vm.ctx.new_str(error.errno);
            let errno = match codes.get_attr(&name, vm) {
                Ok(errno) => errno,
                Err(_) => codes.get_attr("EIO", vm)?,
            };
            let os = vm.import("os", 0)?;
            let strerror = os.get_attr("strerror", vm)?;
            let message = strerror.call((errno.clone(),), vm)?;
            let class = vm.ctx.exceptions.os_error.as_object();
            let args = (errno, message, error.path);
            let raised = class.call(args, vm)?;
            let exception = raised.downcast().expect("exception");
            Err(exception)
        }
        Err(ToolError::Failed(message)) => Err(vm.new_value_error(message)),
        // The check above raises nothing when it isn't due or the code's own SIGINT handler returns
        Err(ToolError::Interrupted) => {
            let interrupt = vm.ctx.exceptions.keyboard_interrupt.to_owned();
            Err(vm.new_exception_empty(interrupt))
        }
    }
}

pub(crate) fn value_error(vm: &VirtualMachine, error: impl Display) -> PyBaseExceptionRef {
    let message = error.to_string();
    vm.new_value_error(message)
}
