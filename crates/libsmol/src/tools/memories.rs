use crate::prompt::{self, EMPTY_MEMORY, NESTED, NO_MEMORY};
use crate::utils::{folded, Reply};
use crate::worker::MemoryCall;
use jieba_rs::Jieba;
use reaper::{cause, tilde};
use rusqlite::{params, Connection, Row, TransactionBehavior};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::task;
use tracing::info;

/// In the config directory, which the sandbox hides from the code
const DB: &str = "memory.db";
/// The worker puts it in the docs of `remember()`
pub const MAX_MEMORY_CHARS: usize = 300;
/// SQLite takes integers as `i64`, and the worker puts it in the docs of `recall()`
pub const MAX_RECALLED: i64 = 10;
const MAX_LISTED: i64 = 10;
const BUSY_WAIT: Duration = Duration::from_secs(5);
/// Made in one write transaction, since sessions of several libsmol may open a new database at once
const SCHEMA: &str = "BEGIN IMMEDIATE;
-- Without AUTOINCREMENT a forgotten newest id comes back, and an id the code still holds deletes the next memory
CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    folder BLOB
);
CREATE VIRTUAL TABLE IF NOT EXISTS terms USING fts5(words, tokenize = 'unicode61 remove_diacritics 2');
PRAGMA user_version = 1;
COMMIT;";

static JIEBA: LazyLock<Jieba> = LazyLock::new(Jieba::new);

/// What the code of one working directory sees: its own memories and the global ones
#[derive(Clone)]
pub(crate) struct Memories {
    db: PathBuf,
    /// The working directory as `folded` spells it
    folder: Vec<u8>,
    /// False for the workers of agents that `agent()` starts, which only recall
    writable: bool,
}

/// What the system prompt shows of them
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Listed {
    /// The global memories up to `MAX_LISTED`, newest first
    pub(crate) newest: Vec<(i64, String)>,
    /// How many global memories there are
    pub(crate) global: usize,
    /// How many the working directory has
    pub(crate) here: usize,
}

impl Memories {
    pub(crate) fn new(config: &Path, cwd: &Path) -> Self {
        let folder = folded(cwd);
        Self {
            db: config.join(DB),
            folder: folder.as_encoded_bytes().to_vec(),
            writable: true,
        }
    }

    pub(crate) fn read_only(&self) -> Self {
        Self {
            writable: false,
            ..self.clone()
        }
    }

    /// Replies as `MemoryCall` documents
    pub(crate) async fn answer(&self, call: MemoryCall) -> Reply {
        match call {
            MemoryCall::Recall { query } => {
                let found = self.recall(&query).await?;
                Ok(json!(found))
            }
            // The worker leaves out what a read-only one can't call, but the code can still ask
            _ if !self.writable => Err(NESTED.to_owned()),
            MemoryCall::Remember { text, project } => {
                let id = self.remember(&text, project).await?;
                Ok(Value::from(id))
            }
            MemoryCall::Forget { id } => {
                self.forget(id).await?;
                Ok(Value::Null)
            }
        }
    }

    pub(crate) async fn listed(&self) -> Result<Listed, String> {
        let memories = self.clone();
        let listing = move || {
            let connection = opened(&memories.db)?;
            let mut newest = connection.prepare(
                "SELECT id, text FROM memories WHERE folder IS NULL ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = newest.query_map([MAX_LISTED], pair)?;
            let newest = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let (global, here): (i64, i64) = connection.query_row(
                "SELECT count(*) FILTER (WHERE folder IS NULL), count(*) FILTER (WHERE folder = ?1) \
                 FROM memories",
                [&memories.folder],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let global = global as usize;
            let here = here as usize;
            Ok(Listed {
                newest,
                global,
                here,
            })
        };
        self.run(listing).await
    }

    /// `project` keeps it to the working directory, and otherwise every directory sees it
    async fn remember(&self, text: &str, project: bool) -> Result<i64, String> {
        let text = text.trim().to_owned();
        let count = text.chars().count();
        if count == 0 {
            return Err(EMPTY_MEMORY.to_owned());
        }
        if count > MAX_MEMORY_CHARS {
            return Err(prompt::long_memory(count));
        }

        let folder = match project {
            true => Some(self.folder.clone()),
            false => None,
        };
        let db = self.db.clone();
        let saving = move || {
            // The first cut loads the dictionary, which must stay off the runtime's thread
            let terms = terms(&text);
            let mut connection = opened(&db)?;
            let saved = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            saved.execute(
                "INSERT INTO memories (text, folder) VALUES (?1, ?2)",
                params![text, folder],
            )?;
            let id = saved.last_insert_rowid();
            saved.execute(
                "INSERT INTO terms (rowid, words) VALUES (?1, ?2)",
                params![id, terms],
            )?;
            saved.commit()?;
            Ok(id)
        };
        let id = self.run(saving).await?;

        let scope = match project {
            true => "project",
            false => "global",
        };
        info!("memory: saved [id={id}, scope={scope}]");
        Ok(id)
    }

    /// The best matches first, or the newest memories when `query` has no words
    async fn recall(&self, query: &str) -> Result<Vec<(i64, String)>, String> {
        let memories = self.clone();
        let query = query.to_owned();
        let searching = move || {
            let matched = matched(&query);
            let connection = opened(&memories.db)?;
            if matched.is_empty() {
                let mut newest = connection.prepare(
                    "SELECT id, text FROM memories WHERE folder = ?1 OR folder IS NULL \
                     ORDER BY id DESC LIMIT ?2",
                )?;
                let rows = newest.query_map(params![memories.folder, MAX_RECALLED], pair)?;
                return rows.collect();
            }

            let mut best = connection.prepare(
                "SELECT memories.id, memories.text FROM terms JOIN memories ON memories.id = terms.rowid \
                 WHERE terms MATCH ?1 AND (memories.folder = ?2 OR memories.folder IS NULL) \
                 ORDER BY bm25(terms), memories.id DESC LIMIT ?3",
            )?;
            let rows = best.query_map(params![matched, memories.folder, MAX_RECALLED], pair)?;
            rows.collect()
        };
        self.run(searching).await
    }

    /// Only a memory the working directory sees
    async fn forget(&self, id: i64) -> Result<(), String> {
        let memories = self.clone();
        let forgetting = move || {
            let mut connection = opened(&memories.db)?;
            let gone = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let deleted = gone.execute(
                "DELETE FROM memories WHERE id = ?1 AND (folder = ?2 OR folder IS NULL)",
                params![id, memories.folder],
            )?;
            // The id may be another directory's memory, whose terms stay
            if deleted > 0 {
                gone.execute("DELETE FROM terms WHERE rowid = ?1", [id])?;
            }
            gone.commit()?;
            Ok(deleted)
        };
        let deleted = self.run(forgetting).await?;
        if deleted == 0 {
            return Err(NO_MEMORY.to_owned());
        }

        info!("memory: forgotten [id={id}]");
        Ok(())
    }

    /// SQLite blocks, and tokio runs every session on one thread
    async fn run<T, F>(&self, work: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce() -> rusqlite::Result<T> + Send + 'static,
    {
        let done = task::spawn_blocking(work).await.expect("memories");
        let shown = tilde(&self.db);
        let failed = |e: rusqlite::Error| {
            let reason = cause(&e);
            format!("memory: {shown}: {reason}")
        };
        done.map_err(failed)
    }
}

fn opened(db: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(db)?;
    connection.busy_timeout(BUSY_WAIT)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        connection.execute_batch(SCHEMA)?;
    }
    Ok(connection)
}

fn pair(row: &Row<'_>) -> rusqlite::Result<(i64, String)> {
    Ok((row.get(0)?, row.get(1)?))
}

/// The words of a stored text separated by spaces, with Chinese cut for search, which also gives the shorter words inside long ones
fn terms(text: &str) -> String {
    let mut words = Vec::new();
    for (chinese, part) in parts(text) {
        if !chinese {
            words.push(part);
            continue;
        }
        // Without HMM a word missing from the dictionary is cut alike wherever it appears
        let tokens = JIEBA.cut_for_search(part, false);
        words.extend(tokens.iter().map(|token| token.word));
    }
    words.join(" ")
}

/// An FTS5 query for any word of `query`, each quoted to keep FTS5 syntax out
fn matched(query: &str) -> String {
    let mut words = Vec::new();
    for (chinese, part) in parts(query) {
        if !chinese {
            words.extend(part.split_whitespace());
            continue;
        }
        let tokens = JIEBA.cut(part, false);
        words.extend(tokens.iter().map(|token| token.word));
    }

    let mut quoted = Vec::new();
    for word in words {
        // A query of punctuation alone lists the newest instead of matching nothing
        if !word.chars().any(char::is_alphanumeric) {
            continue;
        }
        let escaped = word.replace('"', "\"\"");
        quoted.push(format!("\"{escaped}\""));
    }
    quoted.join(" OR ")
}

/// Runs of Chinese characters and of everything else, since jieba splits other letters such as é one by one
fn parts(text: &str) -> Vec<(bool, &str)> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut current = None;
    for (at, c) in text.char_indices() {
        // The CJK ideograph blocks jieba cuts
        let chinese = matches!(
            c,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{20000}'..='\u{2FA1F}'
        );
        if current == Some(chinese) {
            continue;
        }
        if let Some(kind) = current {
            parts.push((kind, &text[start..at]));
        }
        start = at;
        current = Some(chinese);
    }
    if let Some(kind) = current {
        parts.push((kind, &text[start..]));
    }
    parts
}

#[cfg(test)]
mod tests {
    use crate::prompt::{self, EMPTY_MEMORY, NESTED, NO_MEMORY};
    use crate::tests::temp_dir;
    use crate::tools::memories::{matched, terms, Listed, Memories};
    use crate::utils::block_on;
    use crate::worker::MemoryCall;
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    fn opened(config: &Path, cwd: &str) -> Memories {
        fs::create_dir_all(config).expect("config");
        Memories::new(config, Path::new(cwd))
    }

    fn words(text: &str) -> Vec<String> {
        let cut = terms(text);
        cut.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn chinese_cut() {
        let cut = words("中华人民共和国成立了");
        for word in ["中华", "人民", "共和国", "中华人民共和国"] {
            assert!(cut.contains(&word.to_owned()), "{cut:?}");
        }
        assert_eq!(words("用pnpm安装依赖"), ["用", "pnpm", "安装", "依赖"]);
        assert_eq!(
            terms("The café: nautilus_trader"),
            "The café: nautilus_trader"
        );
    }

    #[test]
    fn query_quoted() {
        assert_eq!(matched("安装 pnpm"), "\"安装\" OR \"pnpm\"");
        assert_eq!(matched("中华人民共和国"), "\"中华人民共和国\"");
        let syntax = "\"say\" OR \"\"\"hi\"\"\" OR \"AND\" OR \"NEAR(a\"";
        assert_eq!(matched("say \"hi\" AND * NEAR(a"), syntax);
        assert_eq!(matched("。！ ? "), "");
    }

    #[test]
    fn recall_ranked() {
        let dir = temp_dir("recalled");
        let here = opened(&dir, "/home/zoë/café");
        let there = opened(&dir, "/home/zoë/naïve");
        let saving = async {
            let mut ids = Vec::new();
            for text in [
                "这个项目用 pnpm 安装依赖",
                "安全装置每周检查一次",
                "中华人民共和国成立于1949年",
                "The café on the corner opens at nine",
                "nautilus_trader needs Python 3.12 or newer",
            ] {
                let id = here.remember(text, true).await.expect("remember");
                ids.push(id);
            }
            let theirs = there.remember("用 npm 安装依赖", true).await;
            let shared = there.remember("回复用中文", false).await;
            (ids, theirs.expect("remember"), shared.expect("remember"))
        };
        let (ids, theirs, shared) = block_on(saving);
        let found = |query: &str| {
            let recalling = here.recall(query);
            let recalled = block_on(recalling).expect("recall");
            recalled.into_iter().map(|(id, _)| id).collect::<Vec<_>>()
        };

        assert_eq!(found("安装"), [ids[0]]);
        assert_eq!(found("共和国"), [ids[2]]);
        assert_eq!(found("CAFE"), [ids[3]]);
        assert_eq!(found("nautilus"), [ids[4]]);
        assert_eq!(found("pnpm 依赖 检查")[0], ids[0]);
        assert_eq!(found("中文"), [shared]);
        let newest = [shared, ids[4], ids[3], ids[2], ids[1], ids[0]];
        assert_eq!(found(""), newest);
        assert!(!found("npm 安装依赖").contains(&theirs));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memories_listed() {
        let dir = temp_dir("listed");
        let here = opened(&dir, "/home/zoë/café");
        let elsewhere = opened(&dir, "/home/zoë/naïve");
        let saving = async {
            let empty = here.listed().await.expect("listed");
            let first = here.remember("Prefers résumé over CV", false).await;
            let second = elsewhere.remember("Replies in French", false).await;
            here.remember("Tests run with pytest", true).await.ok();
            elsewhere.remember("Builds with make", true).await.ok();
            let listed = here.listed().await.expect("listed");
            (
                empty,
                first.expect("remember"),
                second.expect("remember"),
                listed,
            )
        };
        let (empty, first, second, listed) = block_on(saving);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(empty, Listed::default());
        let newest = vec![
            (second, "Replies in French".to_owned()),
            (first, "Prefers résumé over CV".to_owned()),
        ];
        let expected = Listed {
            newest,
            global: 2,
            here: 1,
        };
        assert_eq!(listed, expected);
    }

    #[test]
    fn forget_scoped() {
        let dir = temp_dir("forgotten");
        let here = opened(&dir, "/home/zoë/café");
        let there = opened(&dir, "/home/zoë/naïve");
        let forgetting = async {
            let own = here
                .remember("Deploys on Fridays", true)
                .await
                .expect("remember");
            let shared = there
                .remember("Writes naïve tests", false)
                .await
                .expect("remember");
            let theirs = there.remember("Uses tabs", true).await.expect("remember");
            let refused = here.forget(theirs).await;
            let forgotten = [here.forget(own).await, here.forget(shared).await];
            let again = here.forget(own).await;
            let left = there.recall("").await.expect("recall");
            let searched = here.recall("Deploys").await.expect("recall");
            (refused, forgotten, again, left, searched, theirs)
        };
        let (refused, forgotten, again, left, searched, theirs) = block_on(forgetting);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(refused, Err(NO_MEMORY.to_owned()));
        assert_eq!(forgotten, [Ok(()), Ok(())]);
        assert_eq!(again, Err(NO_MEMORY.to_owned()));
        assert_eq!(left, [(theirs, "Uses tabs".to_owned())]);
        assert_eq!(searched, []);
    }

    /// An id the code still holds never names a memory saved later
    #[test]
    fn ids_never_reused() {
        let dir = temp_dir("reused");
        let here = opened(&dir, "/home/zoë/café");
        let saving = async {
            let first = here.remember("Uses pnpm", true).await.expect("remember");
            here.forget(first).await.expect("forget");
            let second = here.remember("Uses naïve mocks", true).await;
            (first, second.expect("remember"))
        };
        let (first, second) = block_on(saving);
        fs::remove_dir_all(&dir).ok();

        assert!(second > first, "{first} {second}");
    }

    #[test]
    fn read_only_recalls() {
        let dir = temp_dir("read-only");
        let here = opened(&dir, "/home/zoë/café");
        let reading = here.read_only();
        let answering = async {
            let remember = MemoryCall::Remember {
                text: "Prefers naïve tests".to_owned(),
                project: true,
            };
            let saved = here.answer(remember).await.expect("remember");
            let forget = MemoryCall::Forget { id: 1 };
            let refused = reading.answer(forget).await;
            let recall = MemoryCall::Recall {
                query: String::new(),
            };
            let found = reading.answer(recall).await;
            (saved, refused, found)
        };
        let (saved, refused, found) = block_on(answering);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(refused, Err(NESTED.to_owned()));
        assert_eq!(found, Ok(json!([[saved, "Prefers naïve tests"]])));
    }

    #[test]
    fn remember_checked() {
        let dir = temp_dir("checked");
        let here = opened(&dir, "/home/zoë/café");
        let longest = "é".repeat(300);
        let longer = "é".repeat(301);
        let checking = async {
            let blank = here.remember(" \n ", true).await;
            let kept = here.remember(&longest, true).await;
            let refused = here.remember(&longer, true).await;
            (blank, kept, refused)
        };
        let (blank, kept, refused) = block_on(checking);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(blank, Err(EMPTY_MEMORY.to_owned()));
        assert!(kept.is_ok(), "{kept:?}");
        assert_eq!(refused, Err(prompt::long_memory(301)));
    }

    /// Sessions of several libsmol write the same database at once
    #[test]
    fn writes_concurrent() {
        let dir = temp_dir("concurrent");
        let first = opened(&dir, "/home/zoë/café");
        let second = opened(&dir, "/home/zoë/naïve");
        let writing = async {
            tokio::join!(
                first.remember("Uses pnpm", true),
                second.remember("Uses cargo", true),
                first.remember("Prefers résumé", false),
                second.remember("Ships on Mondays", false),
            )
        };
        let (a, b, c, d) = block_on(writing);
        let listed = block_on(first.listed()).expect("listed");
        fs::remove_dir_all(&dir).ok();

        let mut ids = vec![a.expect("a"), b.expect("b"), c.expect("c"), d.expect("d")];
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        assert_eq!((listed.newest.len(), listed.global, listed.here), (2, 2, 1));
    }
}
