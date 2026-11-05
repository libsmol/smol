use crate::config::McpConfig;
use crate::steps::{AgentMemory, MemoryStep};
use crate::utils::{folded, new_id};
use crate::{Error, Result};
use reaper::{cause, tilde};
use serde::{Deserialize, Serialize};
use serde_json::Deserializer;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error;
use std::fs::{self, File, TryLockError};
use std::io::{self, BufReader, Read};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const META_PREFIX: &[u8; 8] = br#"{"meta":"#;
pub(crate) const MAX_TITLE_CHARS: usize = 50;
/// Without long paths Windows refuses paths over 260 characters, and packages nest deep in a venv
const MAX_NAME_CHARS: usize = 16;
/// The venv in each project folder, named with a dot, which session ids never have
pub(crate) const VENV: &str = ".venv";
/// Bump it whenever the saved session format changes,
/// as sessions in any other format are neither listed nor opened
const FORMAT: u32 = 8;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SessionMeta {
    format: u32,
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(flatten)]
    pub(crate) settings: Settings,
    created_at: f64,
    pub(crate) updated_at: f64,
}

impl SessionMeta {
    /// The title and the rest are filled in on save
    pub(crate) fn new(id: String, settings: Settings) -> Self {
        let now = now();
        Self {
            format: FORMAT,
            id,
            title: String::new(),
            settings,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub(crate) cwd: PathBuf,
    pub(crate) model: String,
    /// Only servers whose switch differs from `enabled` in agent.toml
    pub(crate) mcp: BTreeMap<String, bool>,
    /// The title the model chose, empty to use the first task or command
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name: String,
}

impl Settings {
    pub(crate) fn mcp_on(&self, server: &McpConfig) -> bool {
        let switch = self.mcp.get(&server.name).copied();
        switch.unwrap_or(server.enabled)
    }

    pub(crate) fn set_mcp(&mut self, server: &McpConfig, on: bool) {
        let name = server.name.clone();
        match on == server.enabled {
            true => self.mcp.remove(&name),
            false => self.mcp.insert(name, on),
        };
    }
}

#[derive(Serialize, Deserialize)]
struct Record<M = SessionMeta, A = AgentMemory> {
    /// Must come first because `read_meta` only reads the start
    meta: M,
    memory: A,
}

pub(crate) struct HistoryStore {
    dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct Lock {
    path: PathBuf,
    _held: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Keep the lock file on Windows because the next holder can't detect a replaced file there
        if cfg!(windows) {
            return;
        }
        // Delete before the lock is released,
        // since the next holder relies on that to see the file was replaced
        fs::remove_file(&self.path).ok();
    }
}

impl HistoryStore {
    pub(crate) fn new(config: &Path) -> Self {
        let dir = config.join("projects");
        Self { dir }
    }

    /// Even when saving fails, `meta` already carries the new title and time
    pub(crate) fn save(&self, memory: &AgentMemory, meta: &mut SessionMeta) -> Result<()> {
        let project = self.project(&meta.settings.cwd);
        let path = session_file(&project, &meta.id)?;

        let mut tasked = None;
        let mut typed = None;
        for step in &memory.steps {
            match step {
                MemoryStep::Task(task) => {
                    let words: Vec<&str> = task.text.split_whitespace().collect();
                    let mut text = words.join(" ");
                    if text.is_empty() {
                        if let Some(file) = task.files.first() {
                            text = file.path.clone();
                        }
                    }
                    tasked = Some(text).filter(|text| !text.is_empty());
                    break;
                }
                MemoryStep::Shell(ran) if typed.is_none() => {
                    let words: Vec<&str> = ran.command.split_whitespace().collect();
                    let command = words.join(" ");
                    typed = Some(format!("! {command}"));
                }
                _ => {}
            }
        }

        let name = &meta.settings.name;
        meta.title = match (name.is_empty(), tasked.or(typed)) {
            (false, _) => name.clone(),
            (true, Some(text)) => text.chars().take(MAX_TITLE_CHARS).collect(),
            (true, None) => "(empty)".to_owned(),
        };
        meta.updated_at = now();

        let record = Record {
            meta: &*meta,
            memory,
        };
        let json = serde_json::to_string(&record).map_err(|e| failed(&path, &e))?;

        // Two processes saving the same session at once each write their own temp file
        let unique = new_id();
        let extension = format!("json.{unique}.tmp");
        let tmp = path.with_extension(extension);
        fs::create_dir_all(&project).map_err(|e| failed(&path, &e))?;
        // On a full disk every step saves again, so broken temp files would pile up unless removed
        let written = fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = written {
            fs::remove_file(&tmp).ok();
            return Err(failed(&path, &e));
        }

        Ok(())
    }

    /// Fails if another process has the session open,
    /// but if locking itself fails it goes unlocked and saving reports the problem
    pub(crate) fn lock(&self, cwd: &Path, id: &str) -> Result<Option<Lock>> {
        let project = self.project(cwd);
        let path = session_file(&project, id)?.with_extension("lock");
        fs::create_dir_all(&project).ok();

        // The file we lock may be one the previous holder just deleted
        loop {
            // On Windows an append-only handle can't be locked
            let opened = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path);
            let Ok(file) = opened else {
                return Ok(None);
            };
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    let message = format!("{id}: open in another process; close it there first");
                    return Err(Error::History(message));
                }
                Err(TryLockError::Error(_)) => return Ok(None),
            }

            // On Unix `dev` and `ino` reveal a replaced file, and Windows can't delete an open file
            #[cfg(unix)]
            let replaced = {
                let held = file.metadata().map(|meta| (meta.dev(), meta.ino()));
                let named = fs::metadata(&path).map(|meta| (meta.dev(), meta.ino()));
                !matches!((held, named), (Ok(held), Ok(named)) if held == named)
            };
            #[cfg(not(unix))]
            let replaced = false;
            if !replaced {
                return Ok(Some(Lock { path, _held: file }));
            }
        }
    }

    /// Fails unless the session is saved for the working directory,
    /// naming the directory when it is saved for another
    pub(crate) fn locate(&self, cwd: &Path, id: &str) -> Result<()> {
        let project = self.project(cwd);
        let path = session_file(&project, id)?;
        if path.exists() {
            return Ok(());
        }

        for other in self.projects()? {
            let path = session_file(&other, id)?;
            let opened = File::open(path).ok();
            if let Some(meta) = opened.and_then(read_meta) {
                let cwd = tilde(&meta.settings.cwd);
                let message = format!("session: {id}: belongs to {cwd}; resume it there");
                return Err(Error::InvalidParams(message));
            }
        }

        Err(Error::SessionNotFound(id.to_owned()))
    }

    pub(crate) fn load(&self, cwd: &Path, id: &str) -> Result<(SessionMeta, AgentMemory)> {
        let project = self.project(cwd);
        let path = session_file(&project, id)?;
        let missing = || Error::SessionNotFound(id.to_owned());
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing()),
            Err(e) => return Err(failed(&path, &e)),
        };
        // A session in another format counts as missing, just as `list` skips it
        if read_meta(text.as_bytes()).is_none() {
            return Err(missing());
        }

        let record: Record = serde_json::from_str(&text).map_err(|e| failed(&path, &e))?;
        Ok((record.meta, record.memory))
    }

    pub(crate) fn list(&self, cwd: Option<&Path>) -> Result<Vec<SessionMeta>> {
        let projects = match cwd {
            Some(cwd) => vec![self.project(cwd)],
            None => self.projects()?,
        };

        let mut found = Vec::new();
        for project in projects {
            let entries = match fs::read_dir(&project) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(failed(&project, &e)),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.extension().is_some_and(|ext| ext == "json") {
                    continue;
                }
                let file = File::open(&path).ok();
                if let Some(meta) = file.and_then(read_meta) {
                    found.push(meta);
                }
            }
        }

        found.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
        Ok(found)
    }

    /// The copies directory sits next to the saved session,
    /// and `list` only reads .json so it never counts as a session
    pub(crate) fn copies(&self, cwd: &Path, id: &str) -> Result<PathBuf> {
        let project = self.project(cwd);
        let path = session_file(&project, id)?;
        Ok(path.with_extension(""))
    }

    /// The folder of the working directory's sessions and venv,
    /// named after its last part and a hash of the whole path, both as `folded` spells them
    pub(crate) fn project(&self, cwd: &Path) -> PathBuf {
        let cwd = PathBuf::from(folded(cwd));
        let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
        let digits: Vec<String> = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let hash = digits.concat();
        let named = cwd.file_name().unwrap_or_default().to_string_lossy();
        let named: String = named.chars().take(MAX_NAME_CHARS).collect();

        let name = format!("{named}-{}", &hash[..16]);
        self.dir.join(name)
    }

    fn projects(&self) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(failed(&self.dir, &e)),
        };

        let mut found = Vec::new();
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                found.push(entry.path());
            }
        }

        Ok(found)
    }
}

/// Ids have no dots, which keeps their files apart from `VENV`
fn session_file(project: &Path, id: &str) -> Result<PathBuf> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if id.is_empty() {
        let message = "session: id: empty".to_owned();
        return Err(Error::InvalidParams(message));
    }
    if !id.chars().all(allowed) {
        let message = format!("session: {id}: invalid id");
        return Err(Error::InvalidParams(message));
    }

    let file = format!("{id}.json");
    Ok(project.join(file))
}

fn failed(path: &Path, error: &dyn error::Error) -> Error {
    let path = tilde(path);
    let reason = cause(error);

    let message = format!("{path}: {reason}");
    Error::History(message)
}

fn now() -> f64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH);
    now.map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn read_meta(reader: impl Read) -> Option<SessionMeta> {
    let mut reader = BufReader::new(reader);

    let mut prefix = [0u8; META_PREFIX.len()];
    reader.read_exact(&mut prefix).ok()?;
    if prefix != *META_PREFIX {
        return None;
    }

    let mut values = Deserializer::from_reader(reader).into_iter();
    let meta: SessionMeta = values.next()?.ok()?;
    (meta.format == FORMAT).then_some(meta)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::config::McpKind;
    use crate::history::*;
    use crate::steps::tests::{asked, one_task};
    use crate::steps::{Attachment, Exit, ShellStep, TaskStep};
    use crate::tests::temp_dir;

    /// Saves it as a new session
    pub(crate) fn saved(
        store: &HistoryStore,
        memory: &AgentMemory,
        id: &str,
        settings: &Settings,
    ) -> SessionMeta {
        let mut meta = SessionMeta::new(id.to_owned(), settings.clone());
        store.save(memory, &mut meta).expect("save");
        meta
    }

    #[test]
    fn mcp_follows_config() {
        let server = |enabled| McpConfig {
            name: "web".to_owned(),
            description: None,
            enabled,
            kind: McpKind::Http {
                url: "http://127.0.0.1:1/mcp".to_owned(),
                headers: BTreeMap::new(),
            },
        };
        let mut settings = Settings::default();
        settings.set_mcp(&server(true), false);
        assert!(!settings.mcp_on(&server(true)));

        // A switch that matches the config isn't recorded, so it follows later config changes
        settings.set_mcp(&server(true), true);
        assert!(settings.mcp.is_empty(), "{:?}", settings.mcp);
        assert!(!settings.mcp_on(&server(false)));
    }

    #[test]
    fn settings_saved() {
        let dir = temp_dir("history");
        let store = HistoryStore::new(&dir);
        let cwd = Path::new("/work");
        let memory = one_task("A task");
        let settings = Settings {
            cwd: cwd.to_owned(),
            model: "tiny".to_owned(),
            mcp: BTreeMap::from([("fake".to_owned(), false)]),
            name: String::new(),
        };

        saved(&store, &memory, "a1", &settings);
        let (meta, _) = store.load(cwd, "a1").expect("load");
        assert_eq!(meta.title, "A task");
        assert_eq!(meta.settings.model, "tiny");
        assert_eq!(meta.settings.mcp.get("fake"), Some(&false));

        let listed = store.list(Some(cwd)).expect("list");
        assert_eq!(listed[0].settings.cwd, cwd);
        let project = store.project(cwd);
        let names = fs::read_dir(&project).expect("dir").flatten();
        let leftover = names.filter(|entry| entry.path().to_string_lossy().ends_with(".tmp"));
        assert_eq!(leftover.count(), 0);

        let named = Settings {
            name: "Clean up the date helpers".to_owned(),
            ..settings.clone()
        };
        saved(&store, &memory, "a1", &named);
        let listed = store.list(Some(cwd)).expect("list");
        let titles: Vec<&str> = listed.iter().map(|meta| meta.title.as_str()).collect();
        assert!(titles.contains(&"Clean up the date helpers"), "{titles:?}");

        let lines = asked("First line\n\n  second line\t");
        let attached = Attachment {
            path: "screenshot.png".to_owned(),
            ..Attachment::default()
        };
        let files = TaskStep {
            text: " \n".to_owned(),
            files: vec![attached],
        };
        for (task, expected) in [(lines, "First line second line"), (files, "screenshot.png")] {
            let memory = AgentMemory {
                steps: vec![MemoryStep::Task(task)],
            };
            let meta = saved(&store, &memory, "d4", &settings);
            assert_eq!(meta.title, expected);
        }

        // Without a task the first command is used
        let typed = |command: &str| {
            let ran = ShellStep {
                command: command.to_owned(),
                shell: "sh".to_owned(),
                output: String::new(),
                end: Exit::Exited(0),
            };
            MemoryStep::Shell(ran)
        };
        let shelled = vec![typed("echo  café\n"), typed("ls")];
        let tasked = vec![typed("ls"), MemoryStep::Task(asked("Count the lines"))];
        for (steps, expected) in [(shelled, "! echo café"), (tasked, "Count the lines")] {
            let memory = AgentMemory { steps };
            let meta = saved(&store, &memory, "d5", &settings);
            assert_eq!(meta.title, expected);
        }

        // A saved session in another format isn't listed and fails to open
        let old = r#"{"meta":{"id":"b2","title":"t","cwd":"/old","created_at":1.0,"updated_at":2.0},"memory":{"steps":[]}}"#;
        let path = project.join("b2.json");
        fs::write(&path, old).expect("write");
        let listed = store.list(Some(cwd)).expect("list");
        assert!(listed.iter().all(|meta| meta.id != "b2"));
        assert!(store.load(cwd, "b2").is_err());
        let older = r#"{"meta":{"format":2,"id":"b3","title":"t","cwd":"/older","created_at":1.0,"updated_at":2.0},"memory":{"steps":[]}}"#;
        fs::write(project.join("b3.json"), older).expect("write");
        let refused = store.load(cwd, "b3").expect_err("format");
        assert_eq!(refused.to_string(), "session: b3: not found");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn other_cwd_refused() {
        let dir = temp_dir("elsewhere");
        let store = HistoryStore::new(&dir);
        let memory = one_task("Fix the menu");
        let here = Path::new("/home/zoë/naïve");
        let settings = Settings {
            cwd: here.to_owned(),
            ..Settings::default()
        };
        saved(&store, &memory, "s1", &settings);

        store.locate(here, "s1").expect("here");
        let other = Path::new("/home/zoë/café");
        let refused = store
            .locate(other, "s1")
            .expect_err("elsewhere")
            .to_string();
        let expected = "session: s1: belongs to /home/zoë/naïve; resume it there";
        assert_eq!(refused, expected);
        let missing = store.locate(other, "s2").expect_err("missing").to_string();
        assert_eq!(missing, "session: s2: not found");
        assert!(!store.project(other).exists());
        assert!(store.list(Some(other)).expect("list").is_empty());
        assert_eq!(store.list(None).expect("list").len(), 1);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_locked() {
        let dir = temp_dir("lock");
        let store = HistoryStore::new(&dir);
        let cwd = Path::new("/work");
        let held = store.lock(cwd, "s1").expect("lock");
        assert!(held.is_some());

        let refused = store.lock(cwd, "s1").expect_err("locked").to_string();
        let expected = "history: s1: open in another process; close it there first";
        assert_eq!(refused, expected);
        for (id, expected) in [
            ("café/../id", "session: café/../id: invalid id"),
            ("", "session: id: empty"),
        ] {
            let refused = store.lock(cwd, id).expect_err("invalid").to_string();
            assert_eq!(refused, expected);
        }

        drop(held);
        let lock = store.project(cwd).join("s1.lock");
        assert_eq!(lock.exists(), cfg!(windows));
        let again = store.lock(cwd, "s1").expect("relock");
        assert!(again.is_some());
        assert!(store.list(Some(cwd)).expect("list").is_empty());

        let file = dir.join("file");
        fs::write(&file, "").expect("write");
        let blocked = HistoryStore::new(&file);
        assert!(blocked.lock(cwd, "s1").expect("unlocked").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn projects_named() {
        let store = HistoryStore::new(Path::new("/config"));
        let named = |cwd: &str| {
            let project = store.project(Path::new(cwd));
            let name = project.file_name().unwrap_or_default();
            name.to_string_lossy().into_owned()
        };

        let name = named("/home/zoë/naïve café");
        assert!(name.starts_with("naïve café-"), "{name}");
        assert_eq!(name.chars().count(), "naïve café-".chars().count() + 16);
        let long = "🐍".repeat(100);
        let path = format!("/home/{long}");
        let name = named(&path);
        assert_eq!(name.chars().count(), 16 + 1 + 16, "{name}");
        assert_ne!(named("/a/work"), named("/b/work"));
        assert_eq!(named("/a/./work/"), named("/a/work"));
        #[cfg(windows)]
        assert_eq!(named(r"C:\Users\Zoë\Café"), named(r"c:\users\zoë\CAFÉ"));
    }
}
