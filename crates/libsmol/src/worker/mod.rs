use crate::config;
use crate::model::Media;
use crate::prompt;
use crate::tools::mcp::{Called, Server};
use crate::utils::{Clip, Interrupt, KEPT_BYTES};
use crate::worker::sandbox::Sandbox;
use crate::worker::wit::smol::host::control;
use crate::worker::wit::{Worker, WorkerPre};
use bytes::Bytes;
use reaper::cause;
use serde_json::{Map, Value};
use std::fs;
use std::future::Future;
use std::io;
use std::mem;
use std::path::Path;
use std::pin::Pin;
use std::process;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tokio::runtime::Builder;
use tokio::select;
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task;
use tracing::{info, warn};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store, Trap, UpdateDeadline};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{self, OutputStream, Pollable, StreamResult};
use wasmtime_wasi::{async_trait, I32Exit, WasiCtx, WasiCtxView, WasiView};

mod builtins;
pub(crate) mod sandbox;

pub(crate) use crate::worker::wit::{Function, Installed, Ran};

mod wit {
    wasmtime::component::bindgen!({
        world: "worker",
        path: "wit",
        imports: { "smol:host/tools": async },
        exports: { default: async },
    });
}

/// Each level of Python recursion takes a lot of native stack
const MAX_STACK_BYTES: usize = 512 << 20;
/// Must be larger than `MAX_STACK_BYTES`, and only reserves address space until used
const FIBER_STACK_BYTES: usize = 520 << 20;
/// Code that swallows exceptions may have caught the previous interrupt
const CANCEL_EVERY: Duration = Duration::from_millis(500);
const WORKER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/worker.wasm.zst"));

static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
    let mut config = Config::new();
    config
        .epoch_interruption(true)
        .max_wasm_stack(MAX_STACK_BYTES)
        .async_stack_size(FIBER_STACK_BYTES);
    Engine::new(&config).expect("engine")
});

static PREPARED: LazyLock<Result<WorkerPre<Host>, String>> = LazyLock::new(|| {
    let dir = config::dir().join("cache");
    let cache = match cfg!(test) {
        true => None,
        false => Some(dir.as_path()),
    };
    let component = compiled(cache)?;

    let failed = |e: wasmtime::Error| e.to_string();
    let mut linker = Linker::new(&ENGINE);
    p2::add_to_linker_async(&mut linker).map_err(failed)?;
    Worker::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |host| host).map_err(failed)?;
    let pre = linker.instantiate_pre(&component).map_err(failed)?;
    WorkerPre::new(pre).map_err(failed)
});

/// Compiles the guest before the first session waits for it, and removes stale sandboxes
pub(crate) fn prepare() {
    thread::spawn(|| LazyLock::force(&PREPARED));
    thread::spawn(sandbox::sweep);
}

/// Loads the compiled guest from `cache`, or compiles it and saves it there
fn compiled(cache: Option<&Path>) -> Result<Component, String> {
    let hash = env!("WORKER_HASH");
    let name = format!("worker-{hash}.cwasm");
    if let Some(cache) = cache {
        let path = cache.join(&name);
        // SAFETY: Only this function writes the file, from what this engine compiled,
        // and loading checks that the engine still matches
        let loaded = unsafe { Component::deserialize_file(&ENGINE, &path) };
        if let Ok(component) = loaded {
            return Ok(component);
        }
    }

    let started = Instant::now();
    let wasm = zstd::decode_all(WORKER).map_err(|e| cause(&e))?;
    let component = Component::new(&ENGINE, &wasm).map_err(|e| e.to_string())?;
    let secs = started.elapsed().as_secs_f64();
    info!("worker: compiled [secs={secs:.1}]");

    let Some(cache) = cache else {
        return Ok(component);
    };
    let pid = process::id();
    let partial = format!(".{name}.{pid}");
    let partial = cache.join(partial);
    // Another libsmol may be writing the same file, so each writes its own and renames it
    let saved = match component.serialize() {
        Ok(bytes) => fs::create_dir_all(cache)
            .and_then(|()| fs::write(&partial, bytes))
            .and_then(|()| fs::rename(&partial, cache.join(&name))),
        Err(e) => Err(io::Error::other(e.to_string())),
    };
    match saved {
        Ok(()) => {
            let entries = fs::read_dir(cache).into_iter().flatten().flatten();
            for entry in entries {
                let file = entry.file_name();
                let file = file.to_string_lossy();
                if file.starts_with("worker-") && file.ends_with(".cwasm") && file != name {
                    fs::remove_file(entry.path()).ok();
                }
            }
        }
        Err(e) => {
            fs::remove_file(&partial).ok();
            let reason = cause(&e);
            warn!("worker: cache not saved, {reason}");
        }
    }
    Ok(component)
}

/// What the code printed and the media it viewed, during the call running now
struct Shared {
    output: Clip,
    media: Vec<(String, Media)>,
}

/// An MCP call made by the code, which the session runs and answers through `reply`
pub(crate) struct ToolCall {
    pub(crate) server: String,
    pub(crate) tool: String,
    pub(crate) args: Map<String, Value>,
    pub(crate) called: String,
    pub(crate) reply: oneshot::Sender<Called>,
}

/// Why a Python is gone, after which its variables are lost
pub(crate) enum Lost {
    Exited(i32),
    Crashed(String),
    Stopped,
}

enum Command {
    Run {
        code: String,
        reply: oneshot::Sender<Result<Ran, Lost>>,
    },
    AddServer {
        source: String,
        description: String,
        tools: String,
        reply: oneshot::Sender<Result<Result<Installed, String>, Lost>>,
    },
    RemoveServer {
        name: String,
    },
}

/// A Python running on its own thread, which is stopped when dropped
pub(crate) struct Guest {
    commands: mpsc::UnboundedSender<Command>,
    pub(crate) calls: mpsc::UnboundedReceiver<ToolCall>,
    shared: Arc<Mutex<Shared>>,
    kill: watch::Sender<bool>,
}

impl Guest {
    pub(crate) async fn start(
        sandbox: &Arc<Sandbox>,
        interrupt: &Interrupt,
    ) -> Result<(Self, Vec<Function>), String> {
        let preparing = task::spawn_blocking(|| LazyLock::force(&PREPARED));
        let prepared = preparing.await.expect("prepare");
        let pre = prepared.as_ref().map_err(String::clone)?;

        let shared = Shared {
            output: Clip::new(KEPT_BYTES),
            media: Vec::new(),
        };
        let shared = Arc::new(Mutex::new(shared));
        let output = Output(Arc::clone(&shared));
        let ctx = sandbox.context(output)?;
        let (kill, killed) = watch::channel(false);
        let (asking, calls) = mpsc::unbounded_channel();
        let host = Host {
            ctx,
            table: ResourceTable::new(),
            interrupt: interrupt.clone(),
            told: None,
            shared: Arc::clone(&shared),
            calls: asking,
            sandbox: Arc::clone(sandbox),
            killed,
            memory: Memory::default(),
        };

        let (commands, inbox) = mpsc::unbounded_channel();
        let (begun, started) = oneshot::channel();
        let builder = thread::Builder::new().name("python".to_owned());
        let spawned = builder.spawn(move || serve(pre, host, inbox, begun));
        if let Err(e) = spawned {
            return Err(cause(&e));
        }
        let functions = match started.await {
            Ok(begun) => begun?,
            Err(_) => return Err(prompt::crashed(prompt::ABORTED)),
        };

        let guest = Self {
            commands,
            calls,
            shared,
            kill,
        };
        Ok((guest, functions))
    }

    /// Sends the code now, and the future doesn't hold on to `self`
    pub(crate) fn run(&self, code: &str) -> impl Future<Output = Result<Ran, Lost>> + 'static {
        let (reply, replied) = oneshot::channel();
        let code = code.to_owned();
        let command = Command::Run { code, reply };
        // A thread that has ended drops `reply`
        self.commands.send(command).ok();
        async move { replied.await.unwrap_or_else(vanished) }
    }

    pub(crate) async fn add_server(
        &self,
        server: &Server,
    ) -> Result<Result<Installed, String>, Lost> {
        let (reply, replied) = oneshot::channel();
        let tools = Value::Array(server.tools.clone());
        let command = Command::AddServer {
            source: server.name.clone(),
            description: server.description.clone(),
            tools: tools.to_string(),
            reply,
        };
        self.commands.send(command).ok();
        replied.await.unwrap_or_else(vanished)
    }

    pub(crate) fn remove_server(&self, name: &str) {
        let name = name.to_owned();
        let command = Command::RemoveServer { name };
        self.commands.send(command).ok();
    }

    /// Forgets what the previous call printed and viewed
    pub(crate) fn clear(&self) {
        let mut shared = self.shared.lock().expect("shared lock");
        shared.output = Clip::new(KEPT_BYTES);
        shared.media.clear();
    }

    /// The clipped output, only when more arrived since the last time
    pub(crate) fn fresh(&self, max: usize) -> Option<String> {
        let mut shared = self.shared.lock().expect("shared lock");
        shared.output.fresh(max)
    }

    /// The output with the characters cut from its middle, and the media, of the call just run
    pub(crate) fn taken(&self) -> (String, usize, Vec<(String, Media)>) {
        let mut shared = self.shared.lock().expect("shared lock");
        let (output, cut) = shared.output.text();
        let media = mem::take(&mut shared.media);
        (output, cut, media)
    }

    /// Stops the code wherever it is, through the epoch check in compiled code
    pub(crate) fn kill(&self) {
        self.kill.send_replace(true);
        ENGINE.increment_epoch();
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Runs the guest's calls in order until one loses it or the `Guest` is dropped
fn serve(
    pre: &WorkerPre<Host>,
    host: Host,
    mut inbox: mpsc::UnboundedReceiver<Command>,
    begun: oneshot::Sender<Result<Vec<Function>, String>>,
) {
    let built = Builder::new_current_thread().enable_all().build();
    let runtime = built.expect("runtime");
    let mut killed = host.killed.clone();
    let mut store = Store::new(&ENGINE, host);
    store.limiter(|host| &mut host.memory);
    // Other sessions' kills also bump the epoch, so only this store's own kill stops it
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|state| match *state.data().killed.borrow() {
        true => Ok(UpdateDeadline::Interrupt),
        false => Ok(UpdateDeadline::Continue(1)),
    });

    runtime.block_on(async move {
        let instance = match pre.instantiate_async(&mut store).await {
            Ok(instance) => instance,
            Err(e) => {
                begun.send(Err(e.to_string())).ok();
                return;
            }
        };
        let functions = match instance.call_start(&mut store).await {
            Ok(Ok(functions)) => functions,
            Ok(Err(traceback)) => {
                begun.send(Err(traceback)).ok();
                return;
            }
            Err(e) => {
                let lost = lost(e, store.data());
                begun.send(Err(prompt::lost(&lost))).ok();
                return;
            }
        };
        begun.send(Ok(functions)).ok();

        while let Some(command) = inbox.recv().await {
            match command {
                Command::Run { code, reply } => {
                    store.data_mut().told = None;
                    store.data_mut().memory.exhausted = false;
                    let running = instance.call_run(&mut store, &code);
                    // Code waiting on the host never reaches the epoch check, so it is dropped
                    let stopped = killed.wait_for(|killed| *killed);
                    let finished = select! {
                        biased;
                        ran = running => Some(ran),
                        _ = stopped => None,
                    };
                    // A kill as the code finished still leaves this Python unusable
                    let usable = !*killed.borrow();
                    let ran = match finished {
                        Some(Ok(ran)) if usable => Ok(ran),
                        Some(Err(e)) => Err(lost(e, store.data())),
                        _ => Err(Lost::Stopped),
                    };
                    let gone = ran.is_err();
                    reply.send(ran).ok();
                    if gone {
                        return;
                    }
                }

                Command::AddServer {
                    source,
                    description,
                    tools,
                    reply,
                } => {
                    let adding =
                        instance.call_add_server(&mut store, &source, &description, &tools);
                    let installed = adding.await.map_err(|e| lost(e, store.data()));
                    let gone = installed.is_err();
                    reply.send(installed).ok();
                    if gone {
                        return;
                    }
                }

                Command::RemoveServer { name } => {
                    let removing = instance.call_remove_server(&mut store, &name);
                    if removing.await.is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// A command goes unanswered only when the host panicked or removing a server lost the guest
fn vanished<T>(_: RecvError) -> Result<T, Lost> {
    let reason = prompt::ABORTED.to_owned();
    Err(Lost::Crashed(reason))
}

fn lost(error: wasmtime::Error, host: &Host) -> Lost {
    if let Some(exit) = error.downcast_ref::<I32Exit>() {
        return Lost::Exited(exit.0);
    }
    if *host.killed.borrow() {
        return Lost::Stopped;
    }

    let reason = match error.downcast_ref::<Trap>() {
        Some(Trap::StackOverflow) => prompt::OUT_OF_STACK.to_owned(),
        // RustPython aborts when an allocation fails
        Some(Trap::UnreachableCodeReached) if host.memory.exhausted => {
            prompt::OUT_OF_MEMORY.to_owned()
        }
        Some(Trap::UnreachableCodeReached) => prompt::ABORTED.to_owned(),
        Some(trap) => trap.to_string(),
        None => error.to_string(),
    };
    Lost::Crashed(reason)
}

pub(crate) struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    interrupt: Interrupt,
    /// When `interrupted` last answered true during this call
    told: Option<Instant>,
    shared: Arc<Mutex<Shared>>,
    calls: mpsc::UnboundedSender<ToolCall>,
    sandbox: Arc<Sandbox>,
    killed: watch::Receiver<bool>,
    memory: Memory,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl control::Host for Host {
    fn interrupted(&mut self) -> bool {
        if !self.interrupt.triggered() {
            return false;
        }
        let due = self.told.is_none_or(|told| told.elapsed() >= CANCEL_EVERY);
        if due {
            self.told = Some(Instant::now());
        }
        due
    }
}

/// Only notes when growing the guest's memory fails, without limiting it
#[derive(Default)]
struct Memory {
    exhausted: bool,
}

impl ResourceLimiter for Memory {
    fn memory_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _: wasmtime::Error) -> wasmtime::Result<()> {
        self.exhausted = true;
        Ok(())
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

/// Both of the guest's output streams, which write in the order the code wrote
#[derive(Clone)]
pub(crate) struct Output(Arc<Mutex<Shared>>);

impl Output {
    fn push(&self, bytes: &[u8]) {
        let mut shared = self.0.lock().expect("shared lock");
        shared.output.push(bytes);
    }
}

impl IsTerminal for Output {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for Output {
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl Pollable for Output {
    async fn ready(&mut self) {}
}

impl OutputStream for Output {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.push(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(usize::MAX)
    }
}

impl AsyncWrite for Output {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.push(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::temp_dir;
    use crate::worker::compiled;
    use std::fs;
    use std::path::Path;

    fn saved(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir).expect("dir").flatten() {
            let name = entry.file_name();
            names.push(name.to_string_lossy().into_owned());
        }
        names.sort();
        names
    }

    /// Loads the saved copy, and a broken one is compiled again and replaces the copies of old builds
    #[test]
    fn cache_reused() {
        let dir = temp_dir("cache");
        compiled(Some(&dir)).expect("compile");
        let names = saved(&dir);
        assert_eq!(names.len(), 1, "{names:?}");
        let current = dir.join(&names[0]);

        let old = dir.join("worker-0000000000000000.cwasm");
        fs::write(&old, "old build").expect("write");
        compiled(Some(&dir)).expect("load");
        assert!(old.exists());

        fs::write(&current, "broken").expect("write");
        compiled(Some(&dir)).expect("compile");
        assert_eq!(saved(&dir), names);
        let size = fs::metadata(&current).expect("size").len();
        fs::remove_dir_all(&dir).ok();
        assert!(size > 1 << 20, "{size}");
    }
}
