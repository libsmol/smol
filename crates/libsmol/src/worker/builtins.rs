use crate::prompt::NO_SERVER;
use crate::tools::attach::{shown_path, viewed};
use crate::tools::web;
use crate::tools::{files, ToolError};
use crate::utils::Interrupt;
use crate::worker::sandbox::{guest_path, host_path, normalized, Reach, Sandbox};
use crate::worker::wit::smol::host::tools::{self, Found, OsError};
use crate::worker::{Host, ToolCall};
use serde_json::{Map, Value};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::select;
use tokio::sync::oneshot;
use tokio::task;

type Refused = tools::ToolError;

impl tools::Host for Host {
    async fn ls(
        &mut self,
        cwd: String,
        path: String,
        recursive: bool,
    ) -> Result<Vec<String>, Refused> {
        let (base, root) = resolved(&self.sandbox, &cwd, &path)?;

        let sandbox = Arc::clone(&self.sandbox);
        let listing = move |stop: &AtomicBool| {
            let follows = |link: &Path| matches!(sandbox.reach(link), Ok(Reach::Inside));
            files::ls(&base, &root, recursive, &follows, stop)
        };
        let listed = blocking(&self.interrupt, listing).await?;
        listed.map_err(|e| refused(e, &path))
    }

    async fn grep(
        &mut self,
        cwd: String,
        pattern: String,
        path: String,
        glob: Option<String>,
    ) -> Result<Vec<Found>, Refused> {
        let (base, root) = resolved(&self.sandbox, &cwd, &path)?;

        let searching =
            move |stop: &AtomicBool| files::grep(&pattern, &base, &root, glob.as_deref(), stop);
        let searched = blocking(&self.interrupt, searching).await?;
        let rows = searched.map_err(|e| refused(e, &path))?;

        let mut found = Vec::new();
        for (name, line, text) in rows {
            // Only an absolute path to search gives absolute names, which are in host form
            let named = Path::new(&name);
            let path = match named.is_absolute() {
                true => guest_path(named),
                false => name,
            };
            found.push(Found { path, line, text });
        }
        Ok(found)
    }

    async fn fetch(&mut self, url: String) -> Result<String, Refused> {
        let fetching = web::fetch(&url);
        let fetched = select! {
            fetched = fetching => fetched,
            () = self.interrupt.wait() => return Err(Refused::Interrupted),
        };
        fetched.map_err(|e| refused(e, &url))
    }

    async fn view(&mut self, cwd: String, path: String) -> Result<(), Refused> {
        let (base, root) = resolved(&self.sandbox, &cwd, &path)?;
        let full = base.join(&root);
        let copies = &self.sandbox.copies;
        let media = viewed(&full, &path, copies).map_err(|e| refused(e, &path))?;

        // Labels count from the session directory, wherever the code moved since
        let absolute = normalized(&full);
        let session = &self.sandbox.cwd;
        let label = match absolute.starts_with(session) {
            true => shown_path(&absolute, session),
            false => guest_path(&absolute),
        };
        let mut shared = self.shared.lock().expect("shared lock");
        shared.media.push((label, media));
        Ok(())
    }

    async fn call(
        &mut self,
        server: String,
        tool: String,
        args: String,
        called: String,
    ) -> Result<String, Refused> {
        let args: Map<String, Value> = match serde_json::from_str(&args) {
            Ok(args) => args,
            Err(e) => return Err(Refused::Failed(e.to_string())),
        };
        let (reply, replied) = oneshot::channel();
        let call = ToolCall {
            server,
            tool,
            args,
            called,
            reply,
        };
        if self.calls.send(call).is_err() {
            return Err(Refused::Failed(NO_SERVER.to_owned()));
        }

        // Dropping `replied` on an interrupt tells the session to stop the call
        let answered = select! {
            answered = replied => answered,
            () = self.interrupt.wait() => return Err(Refused::Interrupted),
        };
        let Ok(called) = answered else {
            return Err(Refused::Failed(NO_SERVER.to_owned()));
        };

        let mut shared = self.shared.lock().expect("shared lock");
        shared.media.extend(called.media);
        match called.result {
            Ok(result) => Ok(result.to_string()),
            Err(reason) => Err(Refused::Failed(reason)),
        }
    }
}

/// `cwd` and `path` in host form, failing as the guest's own `open()` would when `path` leads
/// out of the mounts
fn resolved(sandbox: &Sandbox, cwd: &str, path: &str) -> Result<(PathBuf, PathBuf), Refused> {
    let base = host_path(cwd);
    let root = host_path(path);
    let full = base.join(&root);
    let errno = match sandbox.reach(&full) {
        Ok(Reach::Inside) => return Ok((base, root)),
        Ok(Reach::Escaped) => "EPERM",
        Ok(Reach::Outside) => "ENOENT",
        Err(e) => {
            let error = ToolError::os("read", path, e);
            return Err(refused(error, path));
        }
    };

    let error = OsError {
        errno: errno.to_owned(),
        path: path.to_owned(),
    };
    Err(Refused::Os(error))
}

/// Runs the work on the blocking pool and gives up on an interrupt, setting `stop` for it
async fn blocking<T: Send + 'static>(
    interrupt: &Interrupt,
    work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> Result<T, Refused> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let running = task::spawn_blocking(move || work(&stopping));

    select! {
        done = running => Ok(done.expect("blocking")),
        () = interrupt.wait() => {
            stop.store(true, Ordering::Relaxed);
            Err(Refused::Interrupted)
        }
    }
}

fn refused(error: ToolError, written: &str) -> Refused {
    let ToolError::Os { source, .. } = &error else {
        return Refused::Failed(error.to_string());
    };
    let errno = match source.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::IsADirectory => "EISDIR",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        ErrorKind::ReadOnlyFilesystem => "EROFS",
        ErrorKind::InvalidFilename => "ENAMETOOLONG",
        ErrorKind::TooManyLinks => "EMLINK",
        ErrorKind::StorageFull => "ENOSPC",
        ErrorKind::FileTooLarge => "EFBIG",
        ErrorKind::ResourceBusy => "EBUSY",
        ErrorKind::CrossesDevices => "EXDEV",
        ErrorKind::InvalidInput => "EINVAL",
        ErrorKind::Interrupted => "EINTR",
        ErrorKind::TimedOut => "ETIMEDOUT",
        ErrorKind::OutOfMemory => "ENOMEM",
        _ if source.raw_os_error().is_some() => "EIO",
        _ => return Refused::Failed(error.to_string()),
    };

    let error = OsError {
        errno: errno.to_owned(),
        path: written.to_owned(),
    };
    Refused::Os(error)
}
