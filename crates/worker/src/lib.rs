use libc::wchar_t;
use pyo3::exceptions::PyValueError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};
use std::env;
#[cfg(unix)]
use std::ffi::c_char;
use std::ffi::{c_int, c_void, CString, OsStr, OsString};
#[cfg(target_os = "macos")]
use std::fs;
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
#[cfg(unix)]
use std::slice;
use std::sync::OnceLock;

pub const PYTHON: &str = env!("SMOL_PYTHON");

static STDLIB: &[u8] = include_bytes!(env!("SMOL_STDLIB"));
/// `sys.path` names the stdlib by the executable's path, which only `open_code` reads as the zip
static ARCHIVE: OnceLock<PathBuf> = OnceLock::new();
const RESOURCES: &str = include_str!("resources.py");

type OpenCode = unsafe extern "C" fn(*mut ffi::PyObject, *mut c_void) -> *mut ffi::PyObject;

unsafe extern "C" {
    fn PyFile_SetOpenCodeHook(hook: OpenCode, data: *mut c_void) -> c_int;
}

/// Reads the stdlib zip in place, so every process shares the executable's pages
#[pyclass(module = "_stdlib")]
struct Stored {
    position: usize,
}

#[pymethods]
impl Stored {
    #[pyo3(signature = (size = -1))]
    fn read<'py>(&mut self, py: Python<'py>, size: isize) -> Bound<'py, PyBytes> {
        let start = self.position.min(STDLIB.len());
        let rest = &STDLIB[start..];
        let taken = match usize::try_from(size) {
            Ok(size) => size.min(rest.len()),
            Err(_) => rest.len(),
        };
        self.position = start + taken;
        PyBytes::new(py, &rest[..taken])
    }

    #[pyo3(signature = (offset, whence = 0))]
    fn seek(&mut self, offset: i64, whence: i32) -> PyResult<usize> {
        let base = match whence {
            0 => 0,
            1 => self.position,
            2 => STDLIB.len(),
            _ => return Err(PyValueError::new_err("invalid whence")),
        };
        let moved = i64::try_from(base)
            .ok()
            .and_then(|base| base.checked_add(offset));
        let Some(Ok(position)) = moved.map(usize::try_from) else {
            return Err(PyValueError::new_err("negative seek position"));
        };
        self.position = position;
        Ok(position)
    }

    fn tell(&self) -> usize {
        self.position
    }

    fn seekable(&self) -> bool {
        true
    }

    fn readable(&self) -> bool {
        true
    }

    fn close(&self) {}

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_raised))]
    fn __exit__(&self, _raised: &Bound<'_, PyTuple>) {}
}

#[pymodule]
mod _stdlib {
    use crate::{Stored, ARCHIVE};
    use pyo3::prelude::*;
    use std::ffi::OsString;

    #[pyfunction]
    fn open() -> Stored {
        Stored { position: 0 }
    }

    /// The `sys.path` entry the stdlib comes from
    #[pyfunction]
    fn path() -> OsString {
        let archive = ARCHIVE.get().cloned().unwrap_or_default();
        archive.into_os_string()
    }
}

/// zipimport reads its archive through here, also while the interpreter starts
unsafe extern "C" fn open_code(path: *mut ffi::PyObject, _data: *mut c_void) -> *mut ffi::PyObject {
    // SAFETY: CPython calls the hook holding the GIL, with a borrowed path
    let py = unsafe { Python::assume_attached() };
    let path = unsafe { Bound::from_borrowed_ptr(py, path) };

    let wanted = path.extract::<PathBuf>().ok();
    let opened = match wanted.as_ref() == ARCHIVE.get() {
        true => Bound::new(py, Stored { position: 0 }).map(Bound::into_any),
        false => match py.import("_io") {
            Ok(io) => io.call_method1("open", (&path, "rb")),
            Err(error) => Err(error),
        },
    };
    match opened {
        Ok(file) => file.into_ptr(),
        Err(error) => {
            error.restore(py);
            ptr::null_mut()
        }
    }
}

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
    // zipimport takes the path only once it finds a file there
    ARCHIVE.set(exe.clone()).ok();
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

    pyo3::append_to_inittab!(_stdlib);
    // SAFETY: Runs once per process, before the interpreter starts and with no other thread in it
    unsafe {
        PyFile_SetOpenCodeHook(open_code, ptr::null_mut());

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

        let executable = wide(exe.as_os_str());
        if let Role::Worker = role {
            config.write_bytecode = 0;
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
        config.module_search_paths_set = 1;
        let paths = &raw mut config.module_search_paths;
        checked(ffi::PyWideStringList_Append(paths, executable.as_ptr()));

        checked(ffi::Py_InitializeFromConfig(&config));
        ffi::PyConfig_Clear(&mut config);
    }

    let code = CString::new(RESOURCES).expect("resources");
    Python::attach(|py| {
        let installed = py.run(&code, None, None);
        installed.expect("resources");
    });
}

/// Runs as the python command line with `args`, its own name first, and returns its exit status
pub fn python(args: &[OsString]) -> i32 {
    initialize(Role::Python(args));
    // SAFETY: The interpreter started above, and this finalizes it
    unsafe { ffi::Py_RunMain() }
}

/// Starts the interpreter of a worker. Modules appended to the inittab before it are built in
pub fn start() {
    initialize(Role::Worker);
}

/// Flushes and stops the interpreter `start` started
pub fn finish() -> i32 {
    // SAFETY: Called once, on the thread that started it
    unsafe { ffi::Py_FinalizeEx() }
}
