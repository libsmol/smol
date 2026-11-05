#[cfg(unix)]
use crate::executable;
#[cfg(unix)]
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
#[cfg(unix)]
use reaper::cause;
use std::env;
use std::ffi::CString;
use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
#[cfg(unix)]
use std::process;

const VENV: &str = include_str!("venv.py");
/// Held in a venv while a worker enters it
const LOCK: &str = ".lock";
/// The interpreter's link, which `venv.py` names too, since one named after libsmol would start the agent
const LINKED: &str = "python3";

/// Where a venv keeps its interpreter and the scripts pip installs
#[cfg(unix)]
const SCRIPTS: &str = "bin";
#[cfg(windows)]
const SCRIPTS: &str = "Scripts";
#[cfg(unix)]
const EXE: &str = "python";
#[cfg(windows)]
const EXE: &str = "python.exe";
#[cfg(unix)]
const PIP: &str = "pip";
#[cfg(windows)]
const PIP: &str = "pip.exe";

/// Turns an isolated interpreter into the python of `venv`, making or completing the venv first.
/// This runs the venv's `.pth` files, which the code can write
pub fn enter(py: Python<'_>, venv: &Path) -> PyResult<()> {
    let bin = venv.join(SCRIPTS);
    let interpreter = bin.join(EXE);

    // Other workers of the same directory may be entering it alongside
    let locking = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(venv.join(LOCK));
    let lock = locking.ok();
    if let Some(lock) = &lock {
        // A file system without locks only loses the protection
        lock.lock().ok();
    }

    // The link leads to the libsmol that made it, which may have moved since,
    // and venv keeps a link that is there, also one that leads nowhere
    #[cfg(unix)]
    {
        let linked = bin.join(LINKED);
        let exe = executable();
        let stale = fs::read_link(&linked).is_ok_and(|target| target != exe);
        if stale {
            // A rename replaces it in one step, while programs of other sessions may be starting it
            let id = process::id();
            let aside = bin.join(format!(".{LINKED}-{id}"));
            fs::remove_file(&aside).ok();
            let relinked = symlink(&exe, &aside).and_then(|()| fs::rename(&aside, &linked));
            if let Err(e) = relinked {
                let reason = cause(&e);
                let shown = linked.display();
                let message = format!("worker: {shown}: {reason}");
                return Err(PyRuntimeError::new_err(message));
            }
        }
    }

    // `venv` writes `pyvenv.cfg` before it finishes, and pip after the interpreter
    let found = |name: &str| fs::symlink_metadata(bin.join(name)).is_ok();
    if !found(EXE) || !found(PIP) {
        let code = CString::new(VENV).expect("venv.py");
        let globals = PyDict::new(py);
        globals.set_item("_LINKED", LINKED)?;
        py.run(&code, Some(&globals), None)?;
        let repairing = globals.as_any().get_item("_repair")?;
        repairing.call1((venv.as_os_str(),))?;
    }
    drop(lock);

    // Programs the code starts find the venv's python and pip first, as once it is activated
    let environ = py.import("os")?.getattr("environ")?;
    let mut paths = vec![bin];
    if let Some(listed) = env::var_os("PATH") {
        paths.extend(env::split_paths(&listed));
    }
    if let Ok(joined) = env::join_paths(paths) {
        environ.set_item("PATH", joined)?;
    }
    environ.set_item("VIRTUAL_ENV", venv.as_os_str())?;

    // PyO3 would make a path a `pathlib.Path`, where these are `str`
    let sys = py.import("sys")?;
    sys.setattr("executable", interpreter.as_os_str())?;
    // venv links a new venv to it and names the links after it
    sys.setattr("_base_executable", interpreter.as_os_str())?;
    sys.setattr("prefix", venv.as_os_str())?;
    sys.setattr("exec_prefix", venv.as_os_str())?;

    // Site runs again with the prefixes it took at start replaced
    let site = py.import("site")?;
    site.setattr("PREFIXES", [venv.as_os_str()])?;
    site.call_method0("main")?;
    Ok(())
}
