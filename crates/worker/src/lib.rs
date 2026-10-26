use crate::builtins::{builtins, value_error};
use crate::smol::host::control;
use rustls::crypto::ring::{self, sign};
use rustpython_pylib::FROZEN_STDLIB;
use rustpython_stdlib::ssl::providers::CryptoExt;
use rustpython_stdlib::stdlib_module_defs;
use rustpython_vm::builtins::{PyAnyDict, PyDictRef, PyTupleRef};
use rustpython_vm::signal::set_interrupt_check;
use rustpython_vm::{
    py_serde, AsObject, Interpreter, PyRef, PyResult, Settings, TryFromObject, VirtualMachine,
};
use serde_json::Value;
use std::cell::RefCell;
use std::env;
use wasip2::cli::environment;

mod builtins;
mod prompt;

wit_bindgen::generate!({ world: "worker", path: "../libsmol/wit" });

/// They must share one file name, which `_run` uses to drop them from tracebacks
const BOOTSTRAP: [&str; 3] = [
    include_str!("bootstrap/builtins.py"),
    include_str!("bootstrap/mcp.py"),
    include_str!("bootstrap/run.py"),
];

thread_local! {
    static PYTHON: RefCell<Option<Python>> = const { RefCell::new(None) };
}

struct Worker;

impl Guest for Worker {
    fn start() -> Result<Vec<Function>, String> {
        // wasi-libc starts in / and leaves the initial directory to the program
        if let Some(cwd) = environment::initial_cwd() {
            let moved = env::set_current_dir(&cwd);
            if let Err(e) = moved {
                let reason = e.kind();
                return Err(format!("{cwd}: {reason}"));
            }
        }
        set_interrupt_check(control::interrupted).ok();

        let crypto = CryptoExt {
            all_cipher_suites: Some(ring::ALL_CIPHER_SUITES),
            default_cipher_suites: Some(ring::DEFAULT_CIPHER_SUITES),
            all_kx_groups: Some(ring::ALL_KX_GROUPS),
            any_supported_key: Some(sign::any_supported_type),
            ticketer: ring::Ticketer::new,
        };
        let provider = ring::default_provider();
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
            let tools = builtins(vm);

            let scope = vm.new_scope_with_builtins();
            for (name, value) in tools {
                let set = scope.globals.set_item(name, value, vm);
                set.expect("set global");
            }

            for source in BOOTSTRAP {
                let ran = vm.run_string(scope.clone(), source, "<libsmol_bootstrap>");
                if let Err(err) = ran {
                    let mut traceback = String::new();
                    vm.write_exception(&mut traceback, &err).ok();
                    return Err(traceback);
                }
            }
            Ok(scope.globals)
        })?;

        let python = Python {
            interpreter,
            bootstrap,
        };
        let listed = python.enter(functions)?;
        PYTHON.with_borrow_mut(|slot| *slot = Some(python));
        Ok(listed)
    }

    fn run(code: String) -> Ran {
        let ran = with_python(|python| {
            python.enter(|vm, bootstrap| {
                let run = bootstrap.get_item("_run", vm)?;
                let result = run.call((code,), vm)?;
                let result = PyDictRef::try_from_object(vm, result)?;

                let error = result.get_item("error", vm)?;
                let code = result.get_item("code", vm)?;
                Ok(Ran {
                    error: Option::try_from_object(vm, error)?,
                    code: Option::try_from_object(vm, code)?,
                })
            })
        });

        match ran {
            Ok(ran) => ran,
            Err(error) => Ran {
                error: Some(error),
                code: None,
            },
        }
    }

    fn add_server(source: String, description: String, tools: String) -> Result<Installed, String> {
        with_python(|python| {
            python.enter(|vm, bootstrap| {
                let invalid = |e| value_error(vm, e);
                let tools: Value = serde_json::from_str(&tools).map_err(invalid)?;
                let tools = py_serde::deserialize(vm, tools).map_err(invalid)?;
                let add = bootstrap.get_item("_server", vm)?;
                let installed = add.call((source, description, tools), vm)?;
                let installed = PyDictRef::try_from_object(vm, installed)?;

                let name = installed.get_item("name", vm)?;
                let server = match installed.get_item_opt("server", vm)? {
                    Some(server) => Some(String::try_from_object(vm, server)?),
                    None => None,
                };
                let headers = match installed.get_item_opt("headers", vm)? {
                    Some(headers) => Vec::try_from_object(vm, headers)?,
                    None => Vec::new(),
                };
                Ok(Installed {
                    name: String::try_from_object(vm, name)?,
                    server,
                    headers,
                })
            })
        })
    }

    fn remove_server(name: String) {
        let removed = with_python(|python| {
            python.enter(|vm, bootstrap| {
                let remove = bootstrap.get_item("_unserver", vm)?;
                remove.call((name,), vm)
            })
        });
        removed.ok();
    }
}

export!(Worker);

/// The host calls `start` first and never calls exports while one is running
fn with_python<T>(f: impl FnOnce(&Python) -> T) -> T {
    PYTHON.with_borrow(|python| {
        let python = python.as_ref().expect("started");
        f(python)
    })
}

struct Python {
    interpreter: Interpreter,
    bootstrap: PyRef<PyAnyDict>,
}

impl Python {
    fn enter<T>(
        &self,
        f: impl FnOnce(&VirtualMachine, &PyAnyDict) -> PyResult<T>,
    ) -> Result<T, String> {
        self.interpreter.enter(|vm| {
            // A SIGINT the previous call left unhandled isn't for this one
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

fn functions(vm: &VirtualMachine, bootstrap: &PyAnyDict) -> PyResult<Vec<Function>> {
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
