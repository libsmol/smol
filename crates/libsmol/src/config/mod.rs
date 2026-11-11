use crate::model::{Model, OpenAIModel};
use crate::{Error, Result};
use reaper::{cause, tilde};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Not;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{self, Path, PathBuf};
use std::sync::Arc;
use toml::{Table, Value};

pub(crate) mod setup;

/// Testing compaction shrinks the window a lot, so this only catches 0 and obvious typos
pub(crate) const MIN_CONTEXT: u64 = 1024;

/// Only the first one found in each directory is used
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
const SKILL_FILE: &str = "SKILL.md";
const ROLE_FILE: &str = "roles.toml";
pub(crate) const MODEL_FILE: &str = "model.toml";
const MODEL_EXAMPLE: &str = r#"[model.deepseek]
type = "openai-completions"
api = "https://api.deepseek.com/v1"
api_key = "sk-..."
models = [{ id = "deepseek-chat", context = 65536 }]"#;

pub(crate) fn dir() -> PathBuf {
    match env::var_os("SMOL_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = env::home_dir().unwrap_or_default();
            home.join(".smol")
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelFile {
    /// The toml crate has `preserve_order` on, which keeps the order of the file
    #[serde(default)]
    model: Table,
}

/// One `[model.<name>]` table, as `models` reads it and `save_model` writes it
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, expecting = "a table")]
struct Provider {
    #[serde(rename = "type")]
    kind: Kind,
    api: Option<String>,
    api_key: Option<String>,
    models: Vec<ModelSpec>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, expecting = "a table")]
pub(crate) struct ModelSpec {
    pub(crate) id: String,
    pub(crate) effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) efforts: Vec<String>,
    pub(crate) context: Option<u64>,
    /// DeepSeek counts reasoning toward it, and a small value cuts the reply off mid-reasoning
    pub(crate) max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) image: bool,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) video: bool,
}

#[derive(Deserialize, Serialize)]
enum Kind {
    #[serde(rename = "openai-completions")]
    OpenAI,
}

/// Model names are `provider/id`
pub(crate) struct Models(Vec<(String, Provider)>);

impl Models {
    pub(crate) fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (prefix, provider) in &self.0 {
            for model in &provider.models {
                let name = format!("{prefix}/{}", model.id);
                names.push(name);
            }
        }
        names
    }

    fn find(&self, name: &str) -> Option<(&Provider, &ModelSpec)> {
        for (prefix, provider) in &self.0 {
            let rest = name.strip_prefix(prefix.as_str());
            let Some(id) = rest.and_then(|rest| rest.strip_prefix('/')) else {
                continue;
            };
            if let Some(model) = provider.models.iter().find(|model| model.id == id) {
                return Some((provider, model));
            }
        }
        None
    }

    /// `effort` replaces the one model.toml gives
    pub(crate) fn make(&self, name: &str, effort: Option<&str>) -> Result<Box<dyn Model>> {
        let Some((provider, model)) = self.find(name) else {
            let names = self.names().join(", ");
            let message = format!("model: {name}: not in model.toml (models: {names})");
            return Err(Error::InvalidParams(message));
        };

        let mut spec = model.clone();
        if let Some(effort) = effort {
            spec.effort = Some(effort.to_owned());
        }

        let api = provider.api.clone();
        let key = provider.api_key.clone();
        let built = match provider.kind {
            Kind::OpenAI => OpenAIModel::new(&spec, api, key)?,
        };
        Ok(Box::new(built))
    }

    /// The one model.toml gives
    pub(crate) fn effort(&self, name: &str) -> Option<&str> {
        let (_, spec) = self.find(name)?;
        spec.effort.as_deref()
    }

    /// The efforts a session can switch among, only its own without a list
    pub(crate) fn offered(&self, name: &str) -> Vec<String> {
        let Some((_, spec)) = self.find(name) else {
            return Vec::new();
        };

        match spec.efforts.is_empty() {
            true => spec.effort.iter().cloned().collect(),
            false => spec.efforts.clone(),
        }
    }

    pub(crate) fn check_effort(&self, name: &str, effort: &str) -> Result<()> {
        let offered = self.offered(name);
        if offered.iter().any(|known| known == effort) {
            return Ok(());
        }

        let message = match offered.is_empty() {
            true => format!("effort: {effort}: not offered by {name}"),
            false => {
                let efforts = offered.join(", ");
                format!("effort: {effort}: not offered by {name} (efforts: {efforts})")
            }
        };
        Err(Error::InvalidParams(message))
    }
}

pub(crate) fn models(dir: &Path) -> Result<Models> {
    let path = dir.join(MODEL_FILE);
    let invalid = invalid(&path);
    let text = match read_text(&path) {
        Ok(Some(text)) => text,
        Ok(None) => {
            let shown = tilde(&path);
            let message = format!(
                "{shown}: not found; run `libsmol setup` or create it, for example:\n{MODEL_EXAMPLE}"
            );
            return Err(Error::Unconfigured(message));
        }
        Err(reason) => return Err(invalid(reason)),
    };

    let file: ModelFile = parsed(&text).map_err(&invalid)?;

    let mut providers = Vec::new();
    for (name, value) in file.model {
        let provider = match value.try_into::<Provider>() {
            Ok(provider) => provider,
            Err(e) => {
                let message = without_value(e.message());
                let problem = format!("[model.{name}]: {message}");
                return Err(invalid(problem));
            }
        };

        if let Some(api) = &provider.api {
            if let Err(problem) = parse_url(api) {
                let problem = format!("[model.{name}]: api: {problem}");
                return Err(invalid(problem));
            }
        }
        if provider.models.is_empty() {
            let problem = format!("[model.{name}]: no models");
            return Err(invalid(problem));
        }
        for (index, model) in provider.models.iter().enumerate() {
            let id = &model.id;
            let earlier = &provider.models[..index];
            if earlier.iter().any(|known| known.id == *id) {
                let problem = format!("[model.{name}]: {id}: duplicate");
                return Err(invalid(problem));
            }
            if model.context.is_some_and(|context| context < MIN_CONTEXT) {
                let problem = format!("[model.{name}]: {id}: context below {MIN_CONTEXT}");
                return Err(invalid(problem));
            }
            for (index, effort) in model.efforts.iter().enumerate() {
                if model.efforts[..index].contains(effort) {
                    let problem = format!("[model.{name}]: {id}: efforts: {effort}: duplicate");
                    return Err(invalid(problem));
                }
            }
            let effort = model.effort.as_ref();
            let unlisted = effort.is_none_or(|effort| !model.efforts.contains(effort));
            if !model.efforts.is_empty() && unlisted {
                let efforts = model.efforts.join(", ");
                let problem = format!("[model.{name}]: {id}: effort: not one of {efforts}");
                return Err(invalid(problem));
            }
        }
        providers.push((name, provider));
    }

    if providers.is_empty() {
        return Err(invalid("no models".to_owned()));
    }

    Ok(Models(providers))
}

pub(crate) fn save_model(
    dir: &Path,
    name: &str,
    api: &str,
    key: &str,
    spec: &ModelSpec,
) -> Result<()> {
    let quoted = |text: &str| Value::String(text.to_owned()).to_string();
    let bare = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let plain = !name.is_empty() && name.chars().all(bare);
    let name = match plain {
        true => name.to_owned(),
        false => quoted(name),
    };
    let key = match key.is_empty() {
        true => "none",
        false => key,
    };
    let provider = Provider {
        kind: Kind::OpenAI,
        api: Some(api.to_owned()),
        api_key: Some(key.to_owned()),
        models: vec![spec.clone()],
    };
    let fields = Table::try_from(&provider).expect("serialize");

    // Each field on a line of its own keeps `models` an inline array, as the README writes it
    let mut lines = vec![format!("[model.{name}]")];
    for (field, value) in fields {
        lines.push(format!("{field} = {value}"));
    }
    let text = lines.join("\n") + "\n";

    let path = dir.join(MODEL_FILE);
    let invalid = invalid(&path);
    let unwritable = |e: io::Error| {
        let reason = cause(&e);
        invalid(reason)
    };
    fs::create_dir_all(dir).map_err(unwritable)?;

    // Renaming over a link would leave a plain file where the link was
    let target = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    // A write straight into it that failed halfway would leave no models
    let written = target.with_extension("toml.new");
    // A stale one would keep its own mode
    fs::remove_file(&written).ok();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // The file holds the key in plain text
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&written).map_err(unwritable)?;
    let saved = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = saved {
        // It holds part of the key
        fs::remove_file(&written).ok();
        return Err(unwritable(e));
    }
    fs::rename(&written, &target).map_err(unwritable)?;

    Ok(())
}

/// The global file comes first, then each directory from the root down to `project`,
/// and unreadable files are skipped and returned as problems
pub(crate) fn instructions(dir: &Path, project: &Path) -> (Vec<String>, Vec<Error>) {
    let mut dirs = vec![dir];
    let mut above: Vec<&Path> = project.ancestors().collect();
    above.reverse();
    // The config directory may also be one of the project's ancestors
    above.retain(|above| *above != dir);
    dirs.extend(above);

    let mut found = Vec::new();
    let mut skipped = Vec::new();
    for dir in dirs {
        let mut paths = INSTRUCTION_FILES.iter().map(|name| dir.join(name));
        let Some(path) = paths.find(|path| path.is_file()) else {
            continue;
        };

        let text = match read_text(&path) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(reason) => {
                let error = skip(&path, &reason);
                skipped.push(error);
                continue;
            }
        };

        let text = text.trim();
        if !text.is_empty() {
            found.push(text.to_owned());
        }
    }

    (found, skipped)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    /// An absolute path, which the model passes to `open()` as is
    pub(crate) path: PathBuf,
}

/// `name` in the config directory, then in each level's .smol from the root down to `project`
fn layered(dir: &Path, project: &Path, name: &str) -> Vec<PathBuf> {
    let mut paths = vec![dir.join(name)];
    let mut above: Vec<&Path> = project.ancestors().collect();
    above.reverse();
    for layer in above {
        let path = layer.join(".smol").join(name);
        // The config directory may itself be the .smol of one of the levels
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Global skills live under the config directory's skills
/// and project ones under each level's .smol/skills, and the deepest wins on a name clash
pub(crate) fn skills(dir: &Path, project: &Path) -> (Vec<Skill>, Vec<Error>) {
    let roots = layered(dir, project, "skills");

    let mut found = BTreeMap::new();
    let mut skipped = Vec::new();
    for root in roots {
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                let reason = unreadable(&e);
                let error = skip(&root, &reason);
                skipped.push(error);
                continue;
            }
        };
        let mut folders: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        folders.sort();

        for folder in folders {
            let path = folder.join(SKILL_FILE);
            if !path.is_file() {
                continue;
            }

            let text = match read_text(&path) {
                Ok(Some(text)) => text,
                Ok(None) => continue,
                Err(reason) => {
                    let error = skip(&path, &reason);
                    skipped.push(error);
                    continue;
                }
            };

            let mut fields = front_matter(&text).unwrap_or_default();
            let description = fields.remove("description").filter(|text| !text.is_empty());
            let Some(description) = description else {
                let error = skip(&path, "no description in its front matter");
                skipped.push(error);
                continue;
            };
            let named = fields.remove("name").filter(|name| !name.is_empty());
            let dir_name = folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let name = named.or(dir_name).unwrap_or_default();

            let path = path::absolute(&path).unwrap_or(path);
            let skill = Skill {
                name: name.clone(),
                description,
                path,
            };
            found.insert(name, skill);
        }
    }

    let found = found.into_values().collect();
    (found, skipped)
}

#[derive(Clone)]
pub(crate) struct Role {
    pub(crate) name: String,
    pub(crate) description: String,
    /// The session's model when unset
    pub(crate) model: Option<Arc<dyn Model>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, expecting = "a table")]
struct RoleSpec {
    description: String,
    model: Option<String>,
}

/// Global roles live in the config directory's roles.toml
/// and project ones in each level's .smol/roles.toml, and the deepest wins on a name clash
pub(crate) fn roles(dir: &Path, project: &Path, models: &Models) -> (Vec<Role>, Vec<Error>) {
    let paths = layered(dir, project, ROLE_FILE);

    let mut found = BTreeMap::new();
    let mut skipped = Vec::new();
    for path in paths {
        let text = match read_text(&path) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(reason) => {
                let error = skip(&path, &reason);
                skipped.push(error);
                continue;
            }
        };
        let table: Table = match parsed(&text) {
            Ok(table) => table,
            Err(problem) => {
                let error = skip(&path, &problem);
                skipped.push(error);
                continue;
            }
        };

        for (name, value) in table {
            let spec = match value.try_into::<RoleSpec>() {
                Ok(spec) => spec,
                Err(e) => {
                    let message = without_value(e.message());
                    let problem = format!("[{name}]: {message}");
                    let error = skip(&path, &problem);
                    skipped.push(error);
                    continue;
                }
            };
            let description = spec.description.trim();
            if description.is_empty() {
                let problem = format!("[{name}]: description is empty");
                let error = skip(&path, &problem);
                skipped.push(error);
                continue;
            }

            let model = match spec.model.as_deref().map(|model| models.make(model, None)) {
                None => None,
                Some(Ok(built)) => Some(Arc::from(built)),
                Some(Err(e)) => {
                    let problem = format!("[{name}]: {e}");
                    let error = skip(&path, &problem);
                    skipped.push(error);
                    continue;
                }
            };
            let role = Role {
                name: name.clone(),
                description: description.to_owned(),
                model,
            };
            found.insert(name, role);
        }
    }

    let found = found.into_values().collect();
    (found, skipped)
}

/// The YAML between the opening pair of --- lines,
/// taking only top-level string fields and joining multiline ones into one line
fn front_matter(text: &str) -> Option<BTreeMap<String, String>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }

    let mut fields = BTreeMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in lines {
        let ended = matches!(line.trim_end(), "---" | "...");
        let continued = line.starts_with([' ', '\t']) || line.trim().is_empty();
        if continued && !ended {
            if let Some((_, parts)) = &mut current {
                parts.push(line.trim());
            }
            continue;
        }

        if let Some((key, parts)) = current.take() {
            let value = scalar(&parts);
            fields.insert(key, value);
        }
        if ended {
            return Some(fields);
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_owned();
            current = Some((key, vec![value.trim()]));
        }
    }

    None
}

/// Block scalars (> or |) and indented continuation lines are both joined into one line
fn scalar(parts: &[&str]) -> String {
    let (first, rest) = parts.split_first().unwrap_or((&"", &[]));
    let block = first.starts_with(['>', '|']);
    let mut words = Vec::new();
    if !block {
        words.push(*first);
    }
    words.extend(rest);
    words.retain(|word| !word.is_empty());
    let joined = words.join(" ");
    // A # inside a block scalar isn't a comment
    if block {
        return joined;
    }

    // A quoted value ends at its closing quote, after which a comment may follow
    if let Some(rest) = joined.strip_prefix('"') {
        let mut escaped = false;
        let mut end = None;
        for (at, c) in rest.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => {
                    end = Some(at);
                    break;
                }
                _ => {}
            }
        }
        if let Some(end) = end {
            let quoted = &rest[..end];
            return quoted.replace("\\\"", "\"").replace("\\\\", "\\");
        }
    }
    if let Some(rest) = joined.strip_prefix('\'') {
        let mut chars = rest.char_indices().peekable();
        let mut end = None;
        while let Some((at, c)) = chars.next() {
            if c != '\'' {
                continue;
            }
            // Two quotes stand for one inside the value
            if chars.next_if(|(_, next)| *next == '\'').is_some() {
                continue;
            }
            end = Some(at);
            break;
        }
        if let Some(end) = end {
            let quoted = &rest[..end];
            return quoted.replace("''", "'");
        }
    }

    // In an unquoted value whatever follows # is a comment
    let (plain, _) = joined.split_once(" #").unwrap_or((&joined, ""));
    plain.trim().to_owned()
}

/// `None` when it doesn't exist, without the BOM editors on Windows often write
fn read_text(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let text = text.trim_start_matches('\u{feff}');
            Ok(Some(text.to_owned()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(unreadable(&e)),
    }
}

fn unreadable(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::InvalidData => "not UTF-8 text".to_owned(),
        _ => cause(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFile {
    #[serde(default)]
    mcp: BTreeMap<String, Value>,
    #[serde(default)]
    allow: Allow,
    #[serde(default)]
    deny: Deny,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, expecting = "a table")]
struct Allow {
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, expecting = "a table")]
struct Deny {
    #[serde(default)]
    read: Vec<String>,
}

/// How agent.toml widens or narrows what the sandbox lets the code reach
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Access {
    /// Names of more environment variables to pass on
    pub(crate) env: Vec<String>,
    pub(crate) write: Vec<PathBuf>,
    /// More places the code can neither read nor write
    pub(crate) hidden: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    deny_unknown_fields,
    expecting = "a table"
)]
enum McpSpec {
    Stdio {
        #[serde(default = "enabled")]
        enabled: bool,
        description: Option<String>,
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        #[serde(default = "enabled")]
        enabled: bool,
        description: Option<String>,
        url: String,
        transport: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct McpConfig {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) kind: McpKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum McpKind {
    Stdio {
        command: PathBuf,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// An `[mcp.*]` entry that can't be used
#[derive(Debug)]
pub(crate) struct Skipped {
    pub(crate) name: String,
    pub(crate) problem: Error,
}

/// MCP entries that can't be used are skipped and returned with their problems
pub(crate) fn agent(dir: &Path) -> Result<(Vec<McpConfig>, Access, Vec<Skipped>)> {
    let path = dir.join("agent.toml");
    let invalid = invalid(&path);
    let text = match read_text(&path) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok((Vec::new(), Access::default(), Vec::new())),
        Err(reason) => return Err(invalid(reason)),
    };

    let file: AgentFile = parsed(&text).map_err(&invalid)?;

    let mut servers = Vec::new();
    let mut skipped = Vec::new();
    for (name, value) in file.mcp {
        let spec = match value.try_into::<McpSpec>() {
            Ok(spec) => spec,
            Err(e) => {
                let message = without_value(e.message());
                let problem = format!("[mcp.{name}]: {message}");
                let problem = skip(&path, &problem);
                skipped.push(Skipped { name, problem });
                continue;
            }
        };

        let server = match spec {
            McpSpec::Stdio {
                enabled,
                description,
                command,
                args,
                env,
            } => {
                let kind = McpKind::Stdio { command, args, env };
                McpConfig {
                    name,
                    description,
                    enabled,
                    kind,
                }
            }
            McpSpec::Http {
                enabled,
                description,
                url,
                transport,
                headers,
            } => {
                let transport = transport.as_deref().unwrap_or("streamable-http");
                if transport != "streamable-http" {
                    let problem = format!(
                        "[mcp.{name}]: unsupported transport {transport} (expected streamable-http)"
                    );
                    let problem = skip(&path, &problem);
                    skipped.push(Skipped { name, problem });
                    continue;
                }

                // The URL may carry a secret, so it is never shown
                if let Err(problem) = parse_url(&url) {
                    let problem = format!("[mcp.{name}]: url: {problem}");
                    let problem = skip(&path, &problem);
                    skipped.push(Skipped { name, problem });
                    continue;
                }

                let kind = McpKind::Http { url, headers };
                McpConfig {
                    name,
                    description,
                    enabled,
                    kind,
                }
            }
        };
        servers.push(server);
    }

    let home = env::home_dir().unwrap_or_default();
    let absolute = |key: &str, texts: Vec<String>| {
        let mut paths = Vec::new();
        for text in texts {
            let path = match text.strip_prefix('~') {
                Some(rest) if rest.is_empty() || rest.starts_with(path::is_separator) => {
                    let rest = rest.trim_start_matches(path::is_separator);
                    home.join(rest)
                }
                _ => PathBuf::from(&text),
            };
            if !path.is_absolute() {
                let problem = format!("{key}: {text}: not an absolute path");
                return Err(invalid(problem));
            }
            paths.push(path);
        }
        Ok(paths)
    };
    let write = absolute("[allow]: write", file.allow.write)?;
    let hidden = absolute("[deny]: read", file.deny.read)?;
    let access = Access {
        env: file.allow.env,
        write,
        hidden,
    };

    Ok((servers, access, skipped))
}

pub(crate) fn parse_url(url: &str) -> Result<Url, String> {
    match Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(parsed),
        Ok(_) => Err("expected http:// or https://".to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

fn invalid(path: &Path) -> impl Fn(String) -> Error {
    let shown = tilde(path);
    move |problem| {
        let message = format!("{shown}: {problem}");
        Error::Config(message)
    }
}

fn skip(path: &Path, problem: &str) -> Error {
    let invalid = invalid(path);
    let problem = format!("{problem}, skipped");
    invalid(problem)
}

fn parsed<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    let e = match toml::from_str(text) {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };

    let message = without_value(e.message());
    let Some(span) = e.span() else {
        return Err(message);
    };
    let head = &text[..span.start];
    let line = head.matches('\n').count() + 1;
    let start = head.rfind('\n').map_or(0, |at| at + 1);
    let before = &text[start..span.start];
    // Values may be secrets and are never quoted
    if let Some((key, _)) = before.split_once('=') {
        let key = key.trim();
        return Err(format!("line {line}: {key}: {message}"));
    }

    let rest = &text[span.start..];
    let after = rest.lines().next().unwrap_or_default();
    let spot = &text[span];
    let quoted = format!("`{spot}`");
    // A line without = may continue a value
    let named = before.trim_start().starts_with('[') || after.contains('=');
    let shown = named && !spot.is_empty() && !spot.contains('\n') && !message.contains(&quoted);
    let problem = match shown {
        true => format!("line {line}: {spot}: {message}"),
        false => format!("line {line}: {message}"),
    };
    Err(problem)
}

/// Serde quotes the value it got, which may be a secret
fn without_value(message: &str) -> String {
    for lead in ["invalid type: ", "invalid value: "] {
        let Some(rest) = message.strip_prefix(lead) else {
            continue;
        };
        let Some(at) = rest.rfind(", expected ") else {
            continue;
        };

        let got = &rest[..at];
        let kind = got.split(['`', '"']).next().unwrap_or(got).trim_end();
        let expected = &rest[at..];
        return format!("{lead}{kind}{expected}");
    }

    message.to_owned()
}

#[cfg(test)]
mod tests {
    use crate::config::{
        agent, front_matter, instructions, models, roles, save_model, scalar, skills, Access,
        McpConfig, McpKind, ModelSpec, Skill, MODEL_EXAMPLE,
    };
    use crate::model::MediaKind;
    use crate::tests::temp_dir;
    use crate::Error;
    use reaper::tilde;
    use std::collections::BTreeMap;
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::{Path, PathBuf};

    fn written(text: &str) -> (PathBuf, String) {
        let dir = temp_dir("config");
        fs::create_dir_all(&dir).expect("create");
        let path = dir.join("model.toml");
        fs::write(&path, text).expect("write");
        let shown = tilde(&path);
        (dir, shown)
    }

    #[test]
    fn providers_listed() {
        let text = r#"
[model."über"]
type = "openai-completions"
api = "http://127.0.0.1:1/v1"
models = [
    { id = "m1" },
    { id = "piñata-2", effort = "high", efforts = ["low", "high"], context = 8000, max_tokens = 4096, image = true, video = true },
]

[model.slow]
type = "openai-completions"
models = [{ id = "m3", effort = "max" }]
"#;
        let (dir, _) = written(text);
        let read = models(&dir).expect("load");
        assert_eq!(read.names(), ["über/m1", "über/piñata-2", "slow/m3"]);
        let built = read.make("über/piñata-2", Some("low")).expect("make");
        assert_eq!(built.context_window(), Some(8000));
        assert!(built.takes(MediaKind::Image) && built.takes(MediaKind::Video));

        let missing = read.make("über", None).err().expect("missing").to_string();
        let names = "über/m1, über/piñata-2, slow/m3";
        let expected = format!("model: über: not in model.toml (models: {names})");
        assert_eq!(missing, expected);

        // Without a list a model offers only its own effort, and without one none
        assert_eq!(read.offered("über/piñata-2"), ["low", "high"]);
        assert_eq!(read.offered("slow/m3"), ["max"]);
        assert!(read.offered("über/m1").is_empty());
        assert_eq!(read.effort("über/piñata-2"), Some("high"));
        assert_eq!(read.effort("über/m1"), None);
        read.check_effort("über/piñata-2", "low").expect("offered");
        let refused = |name: &str, effort: &str| {
            let checked = read.check_effort(name, effort);
            checked.expect_err("refused").to_string()
        };
        let expected = "effort: max: not offered by über/piñata-2 (efforts: low, high)";
        assert_eq!(refused("über/piñata-2", "max"), expected);
        assert_eq!(
            refused("über/m1", "low"),
            "effort: low: not offered by über/m1"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_files_rejected() {
        let cases = [
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\nid = \"m1\"\n",
                "[model.café]: unknown field `id`, expected one of `type`, `api`, `api_key`, `models`",
            ),
            (
                "[model.\"naïve\"]\ntype = \"openai-completions\"\nmodels = []\n",
                "[model.naïve]: no models",
            ),
            (
                "[model.kimi]\ntype = \"openai-completions\"\nmodels = [{ id = \"k3\" }, { id = \"k3\" }]\n",
                "[model.kimi]: k3: duplicate",
            ),
            (
                "[model.\"jalapeño\"]\ntype = \"openai-completions\"\nmodels = [{ id = \"m\", context = 0 }]\n",
                "[model.jalapeño]: m: context below 1024",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\nmodels = [{ id = \"m\", efforts = [\"low\", \"high\"] }]\n",
                "[model.café]: m: effort: not one of low, high",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\nmodels = [{ id = \"m\", effort = \"xhigh\", efforts = [\"low\", \"high\"] }]\n",
                "[model.café]: m: effort: not one of low, high",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\nmodels = [{ id = \"m\", effort = \"low\", efforts = [\"low\", \"high\", \"low\"] }]\n",
                "[model.café]: m: efforts: low: duplicate",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\napi = \"api.café.com/v1\"\nmodels = [{ id = \"m\" }]\n",
                "[model.café]: api: relative URL without a base",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\napi = \"ftp://café.com/v1\"\nmodels = [{ id = \"m\" }]\n",
                "[model.café]: api: expected http:// or https://",
            ),
            (
                "[model.kimi]\ntype = \"openai-completions\"\nmodels = [{ id = \"k3\" }]\n\n[model.kimi]\n",
                "line 5: kimi: duplicate key",
            ),
            (
                "[models.\"über\"]\ntype = \"openai-completions\"\n",
                "line 1: unknown field `models`, expected `model`",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\napi_key = sk-café123\nmodels = [{ id = \"m\" }]\n",
                "line 3: api_key: string values must be quoted, expected literal string",
            ),
            (
                "[model.\"café\"]\ntype = \"openai-completions\"\napi_key = 4242\nmodels = [{ id = \"m\" }]\n",
                "[model.café]: invalid type: integer, expected a string",
            ),
            (
                "[model]\n\"café\" = 5\n",
                "[model.café]: invalid type: integer, expected a table",
            ),
            (
                "[model.\"naïve\"]\ntype = \"openai-completions\"\nmodels = [5]\n",
                "[model.naïve]: invalid type: integer, expected a table",
            ),
        ];

        for (text, problem) in cases {
            let (dir, shown) = written(text);
            let error = models(&dir).err().expect("error").to_string();
            assert_eq!(error, format!("config: {shown}: {problem}"));
            fs::remove_dir_all(&dir).ok();
        }

        let (dir, shown) = written("");
        fs::write(dir.join("model.toml"), b"\xff\xfe").expect("write");
        let error = models(&dir).err().expect("error").to_string();
        assert_eq!(error, format!("config: {shown}: not UTF-8 text"));
        fs::remove_dir_all(&dir).ok();

        let (dir, shown) = written("");
        fs::remove_file(dir.join("model.toml")).expect("remove");
        let error = models(&dir).err().expect("error");
        assert!(matches!(error, Error::Unconfigured(_)), "{error:?}");
        let problem =
            format!("not found; run `libsmol setup` or create it, for example:\n{MODEL_EXAMPLE}");
        assert_eq!(error.to_string(), format!("config: {shown}: {problem}"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saved_model_loads() {
        let dir = temp_dir("saved-model");
        let path = dir.join("model.toml");
        let tricky = r#"sk-"café"\🐍"#;
        let spec = ModelSpec {
            id: "piñata-2".to_owned(),
            effort: Some("high".to_owned()),
            context: Some(8192),
            image: true,
            ..ModelSpec::default()
        };
        let api = "http://127.0.0.1:1/v1";
        save_model(&dir, "über café.v2", api, tricky, &spec).expect("save");

        let read = models(&dir).expect("load");
        assert_eq!(read.names(), ["über café.v2/piñata-2"]);
        let built = read.make("über café.v2/piñata-2", None).expect("make");
        assert_eq!(built.context_window(), Some(8192));
        assert!(built.takes(MediaKind::Image) && !built.takes(MediaKind::Video));
        let text = fs::read_to_string(&path).expect("read");
        let parsed: toml::Table = toml::from_str(&text).expect("parse");
        let key = parsed["model"]["über café.v2"]["api_key"].as_str();
        assert_eq!(key, Some(tricky));

        #[cfg(unix)]
        fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("chmod");
        let spec = ModelSpec {
            id: "qwen3:32b".to_owned(),
            ..ModelSpec::default()
        };
        save_model(&dir, "local-1", api, "", &spec).expect("save");

        let text = fs::read_to_string(&path).expect("read");
        let expected = "[model.local-1]\ntype = \"openai-completions\"\napi = \"http://127.0.0.1:1/v1\"\napi_key = \"none\"\nmodels = [{ id = \"qwen3:32b\" }]\n";
        assert_eq!(text, expected);
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let read = models(&dir).expect("load");
        let built = read.make("local-1/qwen3:32b", None).expect("make");
        assert_eq!(built.context_window(), None);

        // A link a dotfile manager made stays, and the file it leads to is rewritten
        #[cfg(unix)]
        {
            let real = dir.join("dotfiles/model.toml");
            fs::create_dir_all(dir.join("dotfiles")).expect("dir");
            fs::rename(&path, &real).expect("move");
            symlink(&real, &path).expect("link");
            save_model(&dir, "local-1", api, "", &spec).expect("save");
            let linked = fs::symlink_metadata(&path).expect("link").file_type();
            assert!(linked.is_symlink());
            assert_eq!(fs::read_to_string(&real).expect("read"), expected);
            assert!(!dir.join("dotfiles/model.toml.new").exists());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn instructions_found() {
        let dir = temp_dir("instructions");
        let global = dir.join("config");
        let outer = dir.join("outer");
        let project = outer.join("résumé");
        // A directory with the same name doesn't count
        fs::create_dir_all(project.join("AGENTS.md")).expect("create");
        fs::create_dir_all(&global).expect("create");
        fs::write(global.join("AGENTS.md"), "\u{feff}Global rules\n").expect("write");
        fs::write(dir.join("AGENTS.md"), " \n").expect("write");
        fs::write(
            dir.join("CLAUDE.md"),
            "An empty AGENTS.md still claims this level",
        )
        .expect("write");
        fs::write(outer.join("AGENTS.md"), "Team rules for the café\n").expect("write");
        fs::write(
            project.join("CLAUDE.md"),
            "Project rules\n\nSecond paragraph\n",
        )
        .expect("write");

        let expected = [
            "Global rules",
            "Team rules for the café",
            "Project rules\n\nSecond paragraph",
        ];
        let written = [
            expected[0],
            expected[1],
            expected[2],
            "An empty AGENTS.md still claims this level",
        ];
        let read = |global: &Path, project: &Path| {
            let (mut found, _) = instructions(global, project);
            // Ignore anything above the temp directory
            found.retain(|text| written.contains(&text.as_str()));
            found
        };

        assert_eq!(read(&global, &project), expected);
        assert_eq!(read(&global, &global), expected[..1]);
        assert_eq!(read(&project, &project), [expected[2], expected[1]]);

        // An unreadable file is skipped and the levels below are still read
        fs::write(global.join("AGENTS.md"), b"\xff\xfe").expect("write");
        assert_eq!(read(&global, &project), expected[1..]);
        let (_, skipped) = instructions(&global, &global);
        let skipped: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        let shown = tilde(&global.join("AGENTS.md"));
        let problem = format!("config: {shown}: not UTF-8 text, skipped");
        assert_eq!(skipped, [problem]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skills_found() {
        let dir = temp_dir("skills");
        let global = dir.join("config");
        let outer = dir.join("outer");
        let project = outer.join("résumé");
        let write = |folder: PathBuf, text: &str| {
            fs::create_dir_all(&folder).expect("create");
            fs::write(folder.join("SKILL.md"), text).expect("write");
            folder.join("SKILL.md")
        };
        write(
            global.join("skills/pdf"),
            "---\nname: pdf\ndescription: Global PDF\n---\nBody",
        );
        let table = write(
            global.join("skills/spreadsheet"),
            "\u{feff}---\ndescription: >\n  Read and write spreadsheets,\n  # not a comment\n---\n",
        );
        let pdf = write(
            outer.join(".smol/skills/docs"),
            "---\nname: pdf\ndescription: 'The project''s PDF'\n---\n",
        );
        write(
            outer.join(".smol/skills/weekly"),
            "---\ndescription: The café's weekly menu\n---\n",
        );
        let weekly = write(
            project.join(".smol/skills/weekly"),
            "---\ndescription: Write the weekly report # comment\nlicense: none\n---\n",
        );
        // Built part by part because the skipped skill is reported with Windows separators
        let empty = write(
            project.join(".smol").join("skills").join("empty"),
            "---\nname: empty\n---\nNo description",
        );
        fs::create_dir_all(project.join(".smol/skills/not-a-skill")).expect("create");

        let skill = |name: &str, description: &str, path: &PathBuf| Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            path: path.clone(),
        };
        let expected = vec![
            skill("pdf", "The project's PDF", &pdf),
            skill(
                "spreadsheet",
                "Read and write spreadsheets, # not a comment",
                &table,
            ),
            skill("weekly", "Write the weekly report", &weekly),
        ];

        let (mut found, skipped) = skills(&global, &project);
        // Ignore anything above the temp directory
        found.retain(|skill| skill.path.starts_with(&dir));
        assert_eq!(found, expected);
        let skipped: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        let shown = tilde(&empty);
        let problem = format!("config: {shown}: no description in its front matter, skipped");
        assert_eq!(skipped, [problem.as_str()]);

        // When the config directory is the project's .smol it is read only once
        let (_, skipped) = skills(&project.join(".smol"), &project);
        let skipped: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(skipped, [problem]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn front_matter_parsed() {
        let cases: [(&[&str], &str); 7] = [
            (&[r#""The café # menu" # comment"#], "The café # menu"),
            (&[r#""Say \"naïve\" \\ twice""#], r#"Say "naïve" \ twice"#),
            (&["'It''s # résumé' # comment"], "It's # résumé"),
            (&["\"Unclosed # comment"], "\"Unclosed"),
            (&["Plain café # comment"], "Plain café"),
            (&[">", "Read the", "# résumé"], "Read the # résumé"),
            (&["Read the", "", "naïve notes"], "Read the naïve notes"),
        ];
        for (parts, expected) in cases {
            assert_eq!(scalar(parts), expected, "{parts:?}");
        }

        let text = "---\nname: café\ndescription: Plain\n  continued\n...\nname: late\n";
        let fields = front_matter(text).expect("front matter");
        let pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [("description", "Plain continued"), ("name", "café")]
        );
        assert_eq!(front_matter("name: café\n---\n"), None);
        assert_eq!(front_matter("---\nname: café\n"), None);
    }

    #[test]
    fn roles_found() {
        let text = "[model.mock]\ntype = \"openai-completions\"\napi = \"http://127.0.0.1:1/v1\"\nmodels = [{ id = \"m1\", context = 8000 }]\n";
        let (global, _) = written(text);
        let read = models(&global).expect("load");
        let outer = global.join("outer");
        let project = outer.join("résumé");
        let write = |dir: PathBuf, text: &str| {
            fs::create_dir_all(&dir).expect("create");
            let path = dir.join("roles.toml");
            fs::write(&path, text).expect("write");
            tilde(&path)
        };
        let top = write(
            global.clone(),
            "stray = \"x\"\n\n[reviewer]\ndescription = \"Reviews the café's code\"\n\n[tester]\ndescription = \"Writes tests\"\nmodel = \"mock/m1\"\n",
        );
        let middle = write(
            outer.join(".smol"),
            "[reviewer]\ndescription = \"  Reviews naïve code \"\n\n[writer]\ndescription = \"\"\n\n[guesser]\ndescription = \"Guesses\"\nmodel = \"mock/m9\"\n\n[typo]\ndescripton = \"x\"\n",
        );
        let broken = write(project.join(".smol"), "[résumé\n");

        let (found, skipped) = roles(&global, &project, &read);

        let mut listed = Vec::new();
        for role in &found {
            let window = role.model.as_ref().and_then(|model| model.context_window());
            listed.push((role.name.as_str(), role.description.as_str(), window));
        }
        let expected = [
            ("reviewer", "Reviews naïve code", None),
            ("tester", "Writes tests", Some(8000)),
        ];
        assert_eq!(listed, expected);
        let skipped: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        let expected = [
            format!("config: {top}: [stray]: invalid type: string, expected a table, skipped"),
            format!("config: {middle}: [writer]: description is empty, skipped"),
            format!("config: {middle}: [guesser]: model: mock/m9: not in model.toml (models: mock/m1), skipped"),
            format!("config: {middle}: [typo]: unknown field `descripton`, expected `description` or `model`, skipped"),
            format!("config: {broken}: line 1: unclosed table, expected `]`, skipped"),
        ];
        assert_eq!(skipped, expected);

        // When the config directory is the project's .smol it is read only once
        let (found, skipped) = roles(&outer.join(".smol"), &outer, &read);
        assert_eq!(found.len(), 1);
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        fs::remove_dir_all(&global).ok();
    }

    #[test]
    fn servers_read() {
        let dir = temp_dir("mcp-config");
        fs::create_dir_all(&dir).expect("create");
        let (servers, access, _) = agent(&dir).expect("missing");
        assert!(servers.is_empty());
        assert_eq!(access, Access::default());

        let text = "[mcp.\"workspace\"]\ntype = \"stdio\"\ncommand = \"desktop-mcp\"\nargs = [\"--quiet\"]\nenv = { LANG = \"en_US.UTF-8\" }\n\n\
            [mcp.web]\ntype = \"http\"\nurl = \"http://127.0.0.1:1/mcp\"\nenabled = false\ndescription = \"Search the web\"\n";
        fs::write(dir.join("agent.toml"), text).expect("write");
        let (read, _, skipped) = agent(&dir).expect("servers");
        assert!(skipped.is_empty(), "{skipped:?}");

        let http = McpKind::Http {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            headers: BTreeMap::new(),
        };
        let web = McpConfig {
            name: "web".to_owned(),
            description: Some("Search the web".to_owned()),
            enabled: false,
            kind: http,
        };
        let env = BTreeMap::from([("LANG".to_owned(), "en_US.UTF-8".to_owned())]);
        let stdio = McpKind::Stdio {
            command: PathBuf::from("desktop-mcp"),
            args: vec!["--quiet".to_owned()],
            env,
        };
        let desktop = McpConfig {
            name: "workspace".to_owned(),
            description: None,
            enabled: true,
            kind: stdio,
        };
        assert_eq!(read, [web, desktop]);

        // An entry that can't be used is skipped and the others are still read
        let shown = tilde(&dir.join("agent.toml"));
        let kept = "\n[mcp.kept]\ntype = \"stdio\"\ncommand = \"kept-mcp\"\n";
        let cases = [
            (
                "café",
                "[mcp.\"café\"]\ntype = \"sse\"\n",
                "unknown variant `sse`, expected `stdio` or `http`",
            ),
            (
                "café",
                "[mcp]\n\"café\" = 1\n",
                "invalid type: integer, expected a table",
            ),
            (
                "workspace",
                "[mcp.\"workspace\"]\ntype = \"stdio\"\ncommand = \"x\"\nurl = \"http://127.0.0.1:1/mcp\"\n",
                "unknown field `url`, expected one of `enabled`, `description`, `command`, `args`, `env`",
            ),
            (
                "web",
                "[mcp.\"web\"]\ntype = \"http\"\nurl = \"localhost:3000/mcp\"\n",
                "url: expected http:// or https://",
            ),
            (
                "web",
                "[mcp.\"web\"]\ntype = \"http\"\nurl = \"café/mcp\"\n",
                "url: relative URL without a base",
            ),
            (
                "web",
                "[mcp.web]\ntype = \"http\"\nurl = \"https://example.com/mcp\"\nheaders = \"Authorization: Bearer naïve\"\n",
                "invalid type: string, expected a map",
            ),
        ];
        for (name, text, problem) in cases {
            let text = format!("{text}{kept}");
            fs::write(dir.join("agent.toml"), text).expect("write");
            let (servers, _, skipped) = agent(&dir).expect("skipped");
            let names: Vec<&str> = servers.iter().map(|server| server.name.as_str()).collect();
            assert_eq!(names, ["kept"]);
            let mut said = Vec::new();
            for entry in skipped {
                said.push((entry.name, entry.problem.to_string()));
            }
            let problem = format!("config: {shown}: [mcp.{name}]: {problem}, skipped");
            assert_eq!(said, [(name.to_owned(), problem)]);
        }

        let cases = [
            (
                "[sandbox]\nnet = true\n",
                "line 1: unknown field `sandbox`, expected one of `mcp`, `allow`, `deny`",
            ),
            (
                "[mcp.\"workspace\"]\ntype = \"stdio\"\ncommand = \"x\"\nargs = [\n  \"--quiet\",\n  sk-naïve,\n]\n",
                "line 6: string values must be quoted, expected literal string",
            ),
        ];
        for (text, problem) in cases {
            fs::write(dir.join("agent.toml"), text).expect("write");
            let error = agent(&dir).expect_err("error").to_string();
            assert_eq!(error, format!("config: {shown}: {problem}"));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn access_read() {
        let dir = temp_dir("access-config");
        fs::create_dir_all(&dir).expect("create");
        let home = env::home_dir().expect("home");
        #[cfg(unix)]
        let absolute = "/srv/café";
        #[cfg(windows)]
        let absolute = r"C:\srv\café";
        let text = format!(
            "[allow]\nenv = [\"GITHUB_TOKEN\"]\nwrite = [\"~/.npm\", '{absolute}']\n\n[deny]\nread = [\"~\"]\n"
        );
        fs::write(dir.join("agent.toml"), text).expect("write");
        let (_, access, _) = agent(&dir).expect("access");

        let expected = Access {
            env: vec!["GITHUB_TOKEN".to_owned()],
            write: vec![home.join(".npm"), PathBuf::from(absolute)],
            hidden: vec![home.clone()],
        };
        assert_eq!(access, expected);

        let shown = tilde(&dir.join("agent.toml"));
        let cases = [
            (
                "[deny]\nread = [\"résumé\"]\n",
                "[deny]: read: résumé: not an absolute path",
            ),
            (
                "[allow]\nwrite = [\"~naïve/notes\"]\n",
                "[allow]: write: ~naïve/notes: not an absolute path",
            ),
            (
                "[allow]\nnet = true\n",
                "line 2: unknown field `net`, expected `env` or `write`",
            ),
            (
                "allow = 5\n",
                "line 1: allow: invalid type: integer, expected a table",
            ),
        ];
        for (text, problem) in cases {
            fs::write(dir.join("agent.toml"), text).expect("write");
            let error = agent(&dir).expect_err("error").to_string();
            assert_eq!(error, format!("config: {shown}: {problem}"));
        }
        fs::remove_dir_all(&dir).ok();
    }
}
