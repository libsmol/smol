use std::ffi::c_void;
use std::io;
use std::mem;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, PROCESS_SET_QUOTA, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
};

/// HANDLE is a raw pointer and can't go in a static as it is
static JOBS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[derive(Clone, Copy)]
pub struct Mark(());

/// Job handles close with this process and take their processes with them, so no watchdog is needed
pub fn guard() {}

/// The spawned process stays suspended until it is handed to `Group::new`
pub fn prepare(command: &mut Command) -> Mark {
    // Running before it joins the job would let its children escape
    // A new process group doesn't get the terminal's Ctrl-C
    // A console of its own keeps code page changes made by ! commands off the terminal
    command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    Mark(())
}

/// Like `prepare`, but the program shares this console and gets its Ctrl-C
pub fn prepare_attached(command: &mut Command) -> Mark {
    command.creation_flags(CREATE_SUSPENDED);
    Mark(())
}

/// Dropping it kills every process in the job, descendants included
pub struct Group {
    job: usize,
    released: AtomicBool,
}

impl Group {
    /// On error the child is killed with the job or still suspended, and the caller must end it
    pub fn new(pid: u32, _mark: Mark) -> io::Result<Group> {
        // SAFETY: Null pointers mean default security attributes and no name
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Dropped on an error below, which kills what the job holds and closes it
        let group = Group {
            job: job as usize,
            released: AtomicBool::new(false),
        };
        limit(job, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;

        let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
        // SAFETY: Only opens our own child, which the caller holds, so its pid can't be reused
        let process = unsafe { OpenProcess(access, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Read before closing the handle, which may replace the last error
        // SAFETY: Both handles were just opened
        let assigned = match unsafe { AssignProcessToJobObject(job, process) } {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        };
        // SAFETY: The handle was just opened and is not used again
        unsafe { CloseHandle(process) };
        assigned?;

        // SAFETY: Only takes a snapshot of threads
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut entry = THREADENTRY32 {
            dwSize: mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut resumed = 0;
        // SAFETY: entry has its dwSize filled in
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: Only opens this thread to resume it
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    // SAFETY: The handle was just opened and is closed right after
                    unsafe {
                        ResumeThread(thread);
                        CloseHandle(thread);
                    }
                    resumed += 1;
                }
            }
            // SAFETY: Keeps reading the same snapshot
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }

        // SAFETY: The snapshot is no longer used
        unsafe { CloseHandle(snapshot) };
        if resumed == 0 {
            return Err(io::Error::other("no thread to resume"));
        }

        JOBS.lock().expect("jobs lock").push(group.job);
        Ok(group)
    }

    pub fn alive(&self) -> bool {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = Default::default();
        let size = mem::size_of_val(&info) as u32;
        let info_ptr = (&raw mut info).cast::<c_void>();
        // A failed query leaves info zeroed, which counts as empty
        // SAFETY: The handle stays open while the Group lives, and the size matches the struct
        unsafe {
            QueryInformationJobObject(
                self.handle(),
                JobObjectBasicAccountingInformation,
                info_ptr,
                size,
                ptr::null_mut(),
            )
        };
        info.ActiveProcesses > 0
    }

    /// After `release` the handle is closed, so it is never killed
    pub fn kill(&self) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        // SAFETY: The handle stays open while the Group lives and is not released
        unsafe { TerminateJobObject(self.handle(), 1) };
    }

    /// Lets go without killing, and processes left in the job outlive the closed handle
    pub fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        forget(self.job);
        limit(self.handle(), 0).ok();
        // SAFETY: Drop never touches this handle afterwards
        unsafe { CloseHandle(self.handle()) };
    }

    fn handle(&self) -> HANDLE {
        self.job as HANDLE
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        forget(self.job);
        self.kill();
        // SAFETY: The Group owns the handle and closes it once
        unsafe { CloseHandle(self.handle()) };
    }
}

pub fn kill_all() {
    for job in JOBS.lock().expect("jobs lock").drain(..) {
        // SAFETY: Registered handles are still open, since they are removed before closing
        unsafe { TerminateJobObject(job as HANDLE, 1) };
    }
}

/// The command line after the program's name, which Windows reads by rules of its own:
/// the name ends at its closing quote, or else at the first space or tab
pub fn command_tail(line: &[u16]) -> &[u16] {
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

fn limit(job: HANDLE, flags: JOB_OBJECT_LIMIT) -> io::Result<()> {
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = Default::default();
    info.BasicLimitInformation.LimitFlags = flags;
    let size = mem::size_of_val(&info) as u32;
    let info_ptr = (&raw const info).cast::<c_void>();
    // SAFETY: The size matches the struct
    let set =
        unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, info_ptr, size) };
    match set {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

fn forget(job: usize) {
    let mut jobs = JOBS.lock().expect("jobs lock");
    jobs.retain(|known| *known != job);
}

#[cfg(test)]
mod tests {
    use crate::windows::{command_tail, prepare, Group};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    fn running(pid: u32) -> bool {
        let filter = format!("PID eq {pid}");
        let listed = Command::new("tasklist")
            .args(["/FI", &filter, "/NH"])
            .output();
        let listed = listed.map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
        let pid = pid.to_string();
        listed.unwrap_or_default().contains(&pid)
    }

    #[test]
    fn tree_killed() {
        let script = "$p = Start-Process -PassThru -WindowStyle Hidden ping -ArgumentList '-n','60','127.0.0.1'; $p.Id";
        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-Command", script])
            .stdout(Stdio::piped());
        let mark = prepare(&mut command);
        let child = command.spawn().expect("spawn");
        let group = Group::new(child.id(), mark).expect("group");

        let output = child.wait_with_output().expect("wait");
        let pid = String::from_utf8_lossy(&output.stdout);
        let pid: u32 = pid.trim().parse().expect("pid");
        assert!(group.alive());
        assert!(running(pid));

        drop(group);
        let started = Instant::now();
        while running(pid) {
            assert!(started.elapsed() < Duration::from_secs(5), "{pid}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn released_kept() {
        let mut command = Command::new("ping");
        command
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null());
        let mark = prepare(&mut command);
        let mut child = command.spawn().expect("spawn");
        let group = Group::new(child.id(), mark).expect("group");

        group.release();
        drop(group);
        assert!(running(child.id()));
        child.kill().ok();
        child.wait().ok();
    }

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
