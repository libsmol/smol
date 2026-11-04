use pyo3::exceptions::PyValueError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};
use std::env;
use std::ffi::{c_int, c_void, CString};
use std::path::{self, Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

static STDLIB: &[u8] = include_bytes!(env!("SMOL_STDLIB"));
/// `sys.path` names the stdlib by the executable's path, which only `open_code` reads as the zip
static ARCHIVE: OnceLock<PathBuf> = OnceLock::new();
/// Hands the same bytes to what reads the stdlib as files
const ADAPTERS: &str = include_str!("stdlib.py");

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
    use crate::stdlib::{Stored, ARCHIVE};
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

/// Serves the stdlib as a zip at `archive`, which must be a file since zipimport checks.
/// Runs before the interpreter starts
pub(crate) fn install(archive: &Path) {
    ARCHIVE.set(archive.to_owned()).ok();
    pyo3::append_to_inittab!(_stdlib);
    // SAFETY: The interpreter hasn't started, and no other thread is in it
    unsafe { PyFile_SetOpenCodeHook(open_code, ptr::null_mut()) };
}

/// Where imports look, set by hand since CPython's own search can't find the stdlib.
/// That search would also add PYTHONPATH, when `environment` lets it count
pub(crate) fn searched(environment: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let listed = env::var_os("PYTHONPATH").unwrap_or_default();
    if environment && !listed.is_empty() {
        for entry in env::split_paths(&listed) {
            // CPython's search takes an empty entry as the current folder
            let absolute = path::absolute(&entry).or_else(|_| env::current_dir());
            if let Ok(absolute) = absolute {
                paths.push(absolute);
            }
        }
    }

    let archive = ARCHIVE.get().cloned().unwrap_or_default();
    paths.push(archive);
    paths
}

/// Runs once the interpreter has started
pub(crate) fn adapt(py: Python<'_>) {
    let code = CString::new(ADAPTERS).expect("stdlib.py");
    let adapted = py.run(&code, None, None);
    adapted.expect("stdlib.py");
}
