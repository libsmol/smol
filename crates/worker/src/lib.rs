use libc::wchar_t;
use pyo3::ffi;
use pyo3::prelude::*;
use std::env;
#[cfg(unix)]
use std::ffi::{c_char, CString};
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "macos")]
use std::fs;
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::ptr;
#[cfg(unix)]
use std::slice;

mod stdlib;
mod venv;
#[cfg(windows)]
mod windows;

pub use crate::venv::enter;
#[cfg(windows)]
pub use crate::windows::program;

pub const PYTHON_VERSION: &str = env!("SMOL_PYTHON_VERSION");

enum Role<'a> {
    /// Writing no bytecode and in UTF-8 mode too, as `-I -B -X utf8` would make it
    Isolated,
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

/// The file a worker starts from: this process's own on Linux, which an update replacing the file
/// leaves alone. `home` matters only on Windows
#[cfg(target_os = "linux")]
pub fn program(_home: &Path) -> Result<PathBuf, String> {
    Ok(PathBuf::from("/proc/self/exe"))
}

/// The file a worker starts from. `home` matters only on Windows
#[cfg(all(unix, not(target_os = "linux")))]
pub fn program(_home: &Path) -> Result<PathBuf, String> {
    Ok(executable())
}

fn initialize(role: Role<'_>) {
    let exe = executable();
    stdlib::install(&exe);
    #[cfg(windows)]
    windows::install();
    let home = exe.parent().unwrap_or(Path::new("")).to_owned();

    let argv: Vec<OsString> = match role {
        Role::Isolated => vec![exe.clone().into_os_string()],
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
    let mut owned: Vec<Vec<wchar_t>> = argv.iter().map(|arg| wide(arg)).collect();
    #[cfg(windows)]
    let mut pointers: Vec<*mut wchar_t> = owned.iter_mut().map(|arg| arg.as_mut_ptr()).collect();

    // SAFETY: Runs once per process, before the interpreter starts and with no other thread in it
    unsafe {
        let mut pre = MaybeUninit::<ffi::PyPreConfig>::uninit();
        let mut config = MaybeUninit::<ffi::PyConfig>::uninit();
        match role {
            Role::Isolated => {
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
            Role::Isolated => {
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

        if let Role::Isolated = role {
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
        let searched = stdlib::search_paths(config.use_environment != 0);
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

/// Runs as the python command line when started under the interpreter's name, or on Windows
/// as the relay a venv's python.exe is, with `args` its own name first, and returns its exit status
pub fn main(args: &[OsString]) -> Option<i32> {
    let named = args.first().map(Path::new).and_then(Path::file_stem);
    let named = named.and_then(OsStr::to_str).unwrap_or_default();
    // A venv links python, python3, python3.14 and 𝜋thon to this program
    #[cfg(unix)]
    if !named.starts_with("python") && named != "𝜋thon" {
        return None;
    }
    // A venv's python.exe is this program too, and the interpreter runs as python314.dll,
    // the name extension modules import CPython by
    #[cfg(windows)]
    {
        if named.eq_ignore_ascii_case("python") {
            return Some(windows::relay());
        }
        if !named.eq_ignore_ascii_case(windows::NAME) {
            return None;
        }
    }

    initialize(Role::Python(args));
    // SAFETY: The interpreter started above, and this finalizes it
    let status = unsafe { ffi::Py_RunMain() };
    Some(status)
}

/// Runs `run` in an isolated interpreter, prints the error it ends with, and returns the exit status.
/// Modules appended to the inittab before it are built in
pub fn isolated(run: impl FnOnce(Python<'_>) -> PyResult<()>) -> i32 {
    initialize(Role::Isolated);
    let ran = Python::attach(run);
    if let Err(error) = &ran {
        Python::attach(|py| error.print(py));
    }

    // SAFETY: Called once, on the thread that started it, which flushes what the code wrote
    let finished = unsafe { ffi::Py_FinalizeEx() };
    match ran {
        Ok(()) => finished,
        Err(_) => 1,
    }
}
