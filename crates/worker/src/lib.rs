use libc::wchar_t;
#[cfg(unix)]
use pyo3::exceptions::PyRuntimeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyDict;
#[cfg(unix)]
use reaper::cause;
use std::env;
#[cfg(unix)]
use std::ffi::c_char;
use std::ffi::{CString, OsStr, OsString};
#[cfg(unix)]
use std::fs;
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process;
#[cfg(unix)]
use std::ptr;
#[cfg(unix)]
use std::slice;

mod stdlib;

pub const PYTHON: &str = env!("SMOL_PYTHON");

const VENV: &str = include_str!("venv.py");

enum Role<'a> {
    /// Isolated, writing no bytecode and in UTF-8 mode, as `-I -B -X utf8` would make it
    Worker,
    /// The command line, its own name first
    Python(&'a [OsString]),
}

fn checked(status: ffi::PyStatus) {
    // SAFETY: Reads a status CPython returned, and exits with its message on an error
    unsafe {
        if ffi::PyStatus_Exception(status) != 0 {
            ffi::Py_ExitStatusException(status);
        }
    }
}

/// Decoded as CPython decodes the command line, with the nul it ends with
fn wide(text: &OsStr) -> Vec<wchar_t> {
    #[cfg(unix)]
    {
        let bytes = CString::new(text.as_bytes()).expect("nul in path");
        // SAFETY: Decodes after preinitialization into memory CPython allocates, freed here
        unsafe {
            let decoded = ffi::Py_DecodeLocale(bytes.as_ptr(), ptr::null_mut());
            assert!(!decoded.is_null(), "decode");
            let mut length = 0;
            while *decoded.add(length) != 0 {
                length += 1;
            }
            let chars = slice::from_raw_parts(decoded, length + 1).to_vec();
            ffi::PyMem_RawFree(decoded.cast());
            chars
        }
    }
    #[cfg(windows)]
    {
        let mut chars: Vec<wchar_t> = text.encode_wide().collect();
        chars.push(0);
        chars
    }
}

/// Where this program is installed, also once an update replaced the file it runs
pub fn executable() -> PathBuf {
    let exe = env::current_exe().unwrap_or_default();
    // Linux names a file that was replaced by its old path with this mark
    #[cfg(target_os = "linux")]
    if let Some(path) = exe.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        let path = OsStr::from_bytes(path);
        return PathBuf::from(path);
    }
    // macOS gives the path the program was started by, such as a venv's link
    #[cfg(target_os = "macos")]
    if let Ok(real) = fs::canonicalize(&exe) {
        return real;
    }
    exe
}

fn initialize(role: Role<'_>) {
    let exe = executable();
    stdlib::install(&exe);
    let home = exe.parent().unwrap_or(Path::new("")).to_owned();

    let argv: Vec<OsString> = match role {
        Role::Worker => vec![exe.clone().into_os_string()],
        Role::Python(args) => args.to_vec(),
    };
    let count = argv.len() as ffi::Py_ssize_t;
    #[cfg(unix)]
    let owned: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()).expect("nul in argument"))
        .collect();
    #[cfg(unix)]
    let mut pointers: Vec<*const c_char> = owned.iter().map(|arg| arg.as_ptr()).collect();
    #[cfg(windows)]
    let mut owned: Vec<Vec<wchar_t>> = argv
        .iter()
        .map(|arg| arg.encode_wide().chain([0]).collect())
        .collect();
    #[cfg(windows)]
    let mut pointers: Vec<*mut wchar_t> = owned.iter_mut().map(|arg| arg.as_mut_ptr()).collect();

    // SAFETY: Runs once per process, before the interpreter starts and with no other thread in it
    unsafe {
        let mut pre = MaybeUninit::<ffi::PyPreConfig>::uninit();
        let mut config = MaybeUninit::<ffi::PyConfig>::uninit();
        match role {
            Role::Worker => {
                ffi::PyPreConfig_InitIsolatedConfig(pre.as_mut_ptr());
                ffi::PyConfig_InitIsolatedConfig(config.as_mut_ptr());
            }
            Role::Python(_) => {
                ffi::PyPreConfig_InitPythonConfig(pre.as_mut_ptr());
                ffi::PyConfig_InitPythonConfig(config.as_mut_ptr());
            }
        }
        let mut pre = pre.assume_init();
        let mut config = config.assume_init();

        // The command line's own -X utf8 and -I reach the preconfiguration only with it
        match role {
            Role::Worker => {
                pre.utf8_mode = 1;
                checked(ffi::Py_PreInitialize(&pre));
            }
            #[cfg(unix)]
            Role::Python(_) => {
                let arguments = pointers.as_mut_ptr().cast();
                checked(ffi::Py_PreInitializeFromBytesArgs(&pre, count, arguments));
            }
            #[cfg(windows)]
            Role::Python(_) => {
                let arguments = pointers.as_mut_ptr();
                checked(ffi::Py_PreInitializeFromArgs(&pre, count, arguments));
            }
        }

        #[cfg(unix)]
        checked(ffi::PyConfig_SetBytesArgv(
            &mut config,
            count,
            pointers.as_mut_ptr(),
        ));
        #[cfg(windows)]
        checked(ffi::PyConfig_SetArgv(
            &mut config,
            count,
            pointers.as_mut_ptr().cast(),
        ));

        if let Role::Worker = role {
            config.write_bytecode = 0;
            let executable = wide(exe.as_os_str());
            let field = &raw mut config.executable;
            checked(ffi::PyConfig_SetString(
                &mut config,
                field,
                executable.as_ptr(),
            ));
            let field = &raw mut config.base_executable;
            checked(ffi::PyConfig_SetString(
                &mut config,
                field,
                executable.as_ptr(),
            ));
        }
        // Frozen copies of os and the like have no file or source
        config.use_frozen_modules = 0;
        // Without a home, the prefix falls back to the build machine's
        let home = wide(home.as_os_str());
        let field = &raw mut config.home;
        checked(ffi::PyConfig_SetString(&mut config, field, home.as_ptr()));

        // Reads the command line's -E and -I, which decide whether PYTHONPATH counts
        if let Role::Python(_) = role {
            checked(ffi::PyConfig_Read(&mut config));
        }
        let searched = stdlib::searched(config.use_environment != 0);
        config.module_search_paths_set = 1;
        let paths = &raw mut config.module_search_paths;
        for path in &searched {
            let entry = wide(path.as_os_str());
            checked(ffi::PyWideStringList_Append(paths, entry.as_ptr()));
        }

        checked(ffi::Py_InitializeFromConfig(&config));
        ffi::PyConfig_Clear(&mut config);
    }

    Python::attach(stdlib::adapt);
}

/// Runs as the python command line when started by a name a venv gives its interpreter,
/// with `args` its own name first, and returns its exit status
pub fn python(args: &[OsString]) -> Option<i32> {
    let named = args.first().map(Path::new).and_then(Path::file_stem);
    let named = named.and_then(OsStr::to_str).unwrap_or_default();
    // A venv links python, python3, python3.14 and 𝜋thon to this program
    if !named.starts_with("python") && named != "𝜋thon" {
        return None;
    }

    initialize(Role::Python(args));
    // SAFETY: The interpreter started above, and this finalizes it
    let status = unsafe { ffi::Py_RunMain() };
    Some(status)
}

/// Starts the interpreter of a worker. Modules appended to the inittab before it are built in
pub fn start() {
    initialize(Role::Worker);
}

/// Makes the interpreter `start` started that of the venv holding `python`, the way the venv's
/// own python runs. This runs the venv's `.pth` files, which the code can write
pub fn enter(py: Python<'_>, python: &OsStr) -> PyResult<()> {
    // The venv's python leads to the program that made it, which may have moved since.
    // A rename replaces the link in one step, as another worker may be starting alongside
    #[cfg(unix)]
    {
        let bin = Path::new(python).parent().unwrap_or(Path::new(""));
        let linked = bin.join("python3");
        let exe = executable();
        if fs::read_link(&linked).ok().as_ref() != Some(&exe) {
            let id = process::id();
            let aside = bin.join(format!(".python3-{id}"));
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

    // Site runs again with the prefixes it took at start replaced
    let venv = venv_of(python);
    let sys = py.import("sys")?;
    sys.setattr("executable", python)?;
    // venv links a new venv to it and names the links after it
    sys.setattr("_base_executable", python)?;
    sys.setattr("prefix", &venv)?;
    sys.setattr("exec_prefix", &venv)?;
    let site = py.import("site")?;
    site.setattr("PREFIXES", [&venv])?;
    site.call_method0("main")?;
    Ok(())
}

/// Makes the venv of `python`, or adds what it lacks and keeps the packages it has
pub fn repair(py: Python<'_>, python: &OsStr) -> PyResult<()> {
    let venv = venv_of(python);
    let code = CString::new(VENV).expect("venv.py");
    let globals = PyDict::new(py);
    py.run(&code, Some(&globals), None)?;
    let repairing = globals.as_any().get_item("_repair")?;
    repairing.call1((venv,))?;
    Ok(())
}

/// Flushes and stops the interpreter `start` started
pub fn finish() -> i32 {
    // SAFETY: Called once, on the thread that started it
    unsafe { ffi::Py_FinalizeEx() }
}

/// The venv holding the interpreter in its `bin` or `Scripts` folder
fn venv_of(python: &OsStr) -> OsString {
    let python = Path::new(python);
    let venv = python.parent().and_then(Path::parent).unwrap_or(python);
    venv.as_os_str().to_owned()
}
