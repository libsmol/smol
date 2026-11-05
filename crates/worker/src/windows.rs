use crate::executable;
use pyo3::ffi;
use reaper::{cause, tilde, Group};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::ptr;
use std::slice;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Environment::GetCommandLineW;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

// Linked for the extension modules that call them
#[cfg(target_env = "msvc")]
extern crate libffi_sys as _;
#[cfg(target_env = "msvc")]
extern crate libsqlite3_sys as _;
#[cfg(target_env = "msvc")]
extern crate openssl_sys as _;

include!(concat!(env!("OUT_DIR"), "/inittab.rs"));

/// The name extension modules import CPython by, which every process running Python must have
pub(crate) const NAME: &str = env!("SMOL_PYTHON_NAME");
/// The forwarder abi3 extension modules import, which leads to `NAME`
static FORWARDER: &[u8] = include_bytes!(env!("SMOL_PYTHON3_DLL"));
const FORWARDER_FILE: &str = "python3.dll";
/// Held while a session brings the files in its home up to date
const LOCK: &str = "python.lock";

unsafe extern "C" {
    static mut PyWin_DLLhModule: HMODULE;
}

/// Must run before the interpreter starts
pub(crate) fn builtins() {
    // SAFETY: Runs once, before the interpreter starts and with no other thread in it
    unsafe {
        PyWin_DLLhModule = GetModuleHandleW(ptr::null());
        for (name, init) in MODULES {
            ffi::PyImport_AppendInittab(name.as_ptr(), Some(*init));
        }
    }
}

/// The file a worker and the python of every venv start from: a link named after `NAME` in `home`
/// to this executable, kept up to date along with `python3.dll` beside it
pub fn program(home: &Path) -> Result<PathBuf, String> {
    let failed = |path: &Path, e: io::Error| {
        let shown = tilde(path);
        let reason = cause(&e);
        format!("{shown}: {reason}")
    };
    fs::create_dir_all(home).map_err(|e| failed(home, e))?;

    // Never truncated, since Windows refuses to write a file that another session holds
    let path = home.join(LOCK);
    let lock = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path);
    let lock = lock.map_err(|e| failed(&path, e))?;
    lock.lock().map_err(|e| failed(&path, e))?;

    let program = home.join(format!("{NAME}.dll"));
    let exe = executable();
    if !same_file(&program, &exe) {
        let replaced = replace(&program, |fresh| link(&exe, fresh));
        replaced.map_err(|e| failed(&program, e))?;
    }
    let forwarder = home.join(FORWARDER_FILE);
    let written = fs::read(&forwarder).ok();
    if written.as_deref() != Some(FORWARDER) {
        let replaced = replace(&forwarder, |fresh| fs::write(fresh, FORWARDER));
        replaced.map_err(|e| failed(&forwarder, e))?;
    }

    sweep(&program);
    sweep(&forwarder);
    Ok(program)
}

/// Runs as a venv's python.exe, starting the interpreter its pyvenv.cfg names
/// with the same command line, and returns its exit status
pub(crate) fn relay() -> i32 {
    let exe = executable();
    let venv = exe.parent().and_then(Path::parent).unwrap_or(Path::new(""));
    let config = venv.join("pyvenv.cfg");
    let text = match fs::read_to_string(&config) {
        Ok(text) => text,
        Err(e) => {
            let shown = tilde(&config);
            let reason = cause(&e);
            eprintln!("python: {shown}: {reason}");
            return 1;
        }
    };
    let mut home = None;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "home" {
                home = Some(value.trim().to_owned());
            }
        }
    }
    let Some(home) = home else {
        let shown = tilde(&config);
        eprintln!("python: {shown}: no home");
        return 1;
    };

    // SAFETY: The command line lives as long as the process, and ends with a nul
    let line = unsafe {
        let start = GetCommandLineW();
        let mut length = 0;
        while *start.add(length) != 0 {
            length += 1;
        }
        slice::from_raw_parts(start, length)
    };
    let tail = OsString::from_wide(command_tail(line));

    // The interpreter shares the console and takes Ctrl-C itself, while this waits for it
    // SAFETY: The handler only answers that the event was handled
    unsafe { SetConsoleCtrlHandler(Some(handled), 1) };
    let interpreter = Path::new(&home).join(format!("{NAME}.dll"));
    let mut command = Command::new(&interpreter);
    command.raw_arg(&tail).env("__PYVENV_LAUNCHER__", &exe);
    let mark = reaper::prepare_attached(&mut command);
    let started = command.spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(e) => {
            let shown = tilde(&interpreter);
            let reason = cause(&e);
            eprintln!("python: {shown}: {reason}");
            return 1;
        }
    };
    // Ended along with this, as when a timeout kills it
    let group = match Group::new(child.id(), mark) {
        Ok(group) => group,
        Err(e) => {
            child.kill().ok();
            let reason = cause(&e);
            eprintln!("python: {reason}");
            return 1;
        }
    };

    let status = child.wait();
    // What it started outlives it, as a python's programs do elsewhere
    group.release();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

unsafe extern "system" fn handled(_: u32) -> BOOL {
    1
}

/// Whether both paths name the same file, as hard links do
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    let identity = |path: &Path| {
        let file = File::open(path).ok()?;
        // SAFETY: The struct is plain data, which the call fills in
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
        // SAFETY: The handle stays open for the call
        let got = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        let found = (
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        );
        (got != 0).then_some(found)
    };
    let known = identity(a);
    known.is_some() && known == identity(b)
}

/// A hard link, or a copy when the folders are on different volumes
pub(crate) fn link(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(_) => fs::copy(from, to).map(|_| ()),
    }
}

/// Makes a new file beside `path` and swaps it in, moving the old one aside with a name ending in `.old`,
/// as Windows renames a file that is running but doesn't delete it
pub(crate) fn replace(path: &Path, make: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH);
    let stamp = elapsed.map_or(0, |elapsed| elapsed.as_nanos());
    let unique = format!("{}.{stamp}", process::id());
    let mut fresh = path.as_os_str().to_owned();
    fresh.push(format!(".{unique}.new"));
    let fresh = PathBuf::from(fresh);
    make(&fresh)?;

    if path.exists() {
        let mut aside = path.as_os_str().to_owned();
        aside.push(format!(".{unique}.old"));
        if let Err(e) = fs::rename(path, aside) {
            fs::remove_file(&fresh).ok();
            return Err(e);
        }
    }
    fs::rename(&fresh, path)
}

/// Removes what `replace` moved aside from `path`, except the files still running,
/// which a later call finds unused
pub(crate) fn sweep(path: &Path) {
    let Some(folder) = path.parent() else {
        return;
    };
    let named = path.file_name().unwrap_or_default().to_string_lossy();
    let prefix = format!("{named}.");
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".old") {
            fs::remove_file(entry.path()).ok();
        }
    }
}

/// The command line after the program's name, which Windows reads by rules of its own:
/// the name ends at its closing quote, or else at the first space or tab
fn command_tail(line: &[u16]) -> &[u16] {
    let quote = u16::from(b'"');
    let blank = |c: &u16| *c == u16::from(b' ') || *c == u16::from(b'\t');
    let end = match line.first() == Some(&quote) {
        true => {
            let closing = line[1..].iter().position(|c| *c == quote);
            closing.map_or(line.len(), |at| at + 2)
        }
        false => line.iter().position(blank).unwrap_or(line.len()),
    };
    let rest = &line[end..];
    let start = rest.iter().position(|c| !blank(c)).unwrap_or(rest.len());
    &rest[start..]
}

#[cfg(test)]
mod tests {
    use crate::windows::command_tail;

    #[test]
    fn tail_kept() {
        let cases = [
            (
                r#""C:\Program Files\café\python.exe" -c "print('naïve')""#,
                r#"-c "print('naïve')""#,
            ),
            (
                "python.exe  \t-m pip install résumé",
                "-m pip install résumé",
            ),
            ("python.exe", ""),
            (r#""C:\café\python.exe""#, ""),
            (r#""C:\café\python.exe"-c pass"#, "-c pass"),
        ];
        for (line, expected) in cases {
            let line: Vec<u16> = line.encode_utf16().collect();
            let tail = String::from_utf16(command_tail(&line)).expect("utf-16");
            assert_eq!(tail, expected);
        }
    }
}
