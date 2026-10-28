use crate::model::{Model, ModelSpec, OpenAIModel};
use crate::{Error, Result};
use reaper::{cause, tilde};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
#[cfg(unix)]
use std::fs::Permissions;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{self, Path, PathBuf};
use std::sync::Arc;
use toml::Value;

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
    model: toml::Table,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    #[serde(rename = "type")]
    kind: Kind,
    api: Option<String>,
    api_key: Option<String>,
    models: Vec<ModelSpec>,
}

#[derive(Deserialize)]
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

    pub(crate) fn make(&self, name: &str) -> Result<Box<dyn Model>> {
        for (prefix, provider) in &self.0 {
            let rest = name.strip_prefix(prefix.as_str());
            let Some(id) = rest.and_then(|rest| rest.strip_prefix('/')) else {
                continue;
            };
            let Some(model) = provider.models.iter().find(|model| model.id == id) else {
                continue;
            };

            let api = provider.api.clone();
            let key = provider.api_key.clone();
            let built = match provider.kind {
                Kind::OpenAI => OpenAIModel::new(model, api, key)?,
            };
            return Ok(Box::new(built));
        }

        let names = self.names().join(", ");
        let message = format!("model: {name}: not in model.toml (models: {names})");
        Err(Error::InvalidParams(message))
    }
}

pub(crate) fn models(dir: &Path) -> Result<Models> {
    let path = dir.join(MODEL_FILE);
    let invalid = invalid(&path);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let shown = tilde(&path);
            let message = format!(
                "{shown}: not found; run `libsmol setup` or create it, for example:\n{MODEL_EXAMPLE}"
            );
            return Err(Error::Unconfigured(message));
        }
        Err(e) => {
            let reason = cause(&e);
            return Err(invalid(reason));
        }
    };

    let file: ModelFile = parsed(&text).map_err(&invalid)?;

    let mut providers = Vec::new();
    for (name, value) in file.model {
        let provider = match value.try_into::<Provider>() {
            Ok(provider) => provider,
            Err(e) => {
                let message = e.message();
                let problem = format!("[model.{name}]: {message}");
                return Err(invalid(problem));
            }
        };

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
    let api = quoted(api);
    let key = quoted(key);
    let entry = Value::try_from(spec).expect("serialize");

    let lines = [
        format!("[model.{name}]"),
        "type = \"openai-completions\"".to_owned(),
        format!("api = {api}"),
        format!("api_key = {key}"),
        format!("models = [{entry}]"),
    ];
    let text = lines.join("\n") + "\n";

    let path = dir.join(MODEL_FILE);
    let invalid = invalid(&path);
    let unwritable = |e: io::Error| {
        let reason = cause(&e);
        invalid(reason)
    };
    fs::create_dir_all(dir).map_err(unwritable)?;

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // The file holds the key in plain text
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).map_err(unwritable)?;
    // The mode only applies to a new file
    #[cfg(unix)]
    {
        let private = Permissions::from_mode(0o600);
        file.set_permissions(private).map_err(unwritable)?;
    }
    file.write_all(text.as_bytes()).map_err(unwritable)?;

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

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                let invalid = invalid(&path);
                let reason = unreadable(&e);
                let error = invalid(reason);
                skipped.push(error);
                continue;
            }
        };

        // Editors on Windows often write a BOM at the start
        let text = text.trim_start_matches('\u{feff}').trim();
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
                let invalid = invalid(&root);
                let reason = unreadable(&e);
                let error = invalid(reason);
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
            let invalid = invalid(&path);
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    let reason = unreadable(&e);
                    let error = invalid(reason);
                    skipped.push(error);
                    continue;
                }
            };

            let text = text.trim_start_matches('\u{feff}');
            let mut fields = front_matter(text).unwrap_or_default();
            let description = fields.remove("description").filter(|text| !text.is_empty());
            let Some(description) = description else {
                let problem = "no description in its front matter, skipped".to_owned();
                let error = invalid(problem);
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
#[serde(deny_unknown_fields)]
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
        let invalid = invalid(&path);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                let reason = unreadable(&e);
                skipped.push(invalid(reason));
                continue;
            }
        };
        let table: toml::Table = match parsed(&text) {
            Ok(table) => table,
            Err(problem) => {
                let problem = format!("{problem}, skipped");
                skipped.push(invalid(problem));
                continue;
            }
        };

        for (name, value) in table {
            let spec = match value.try_into::<RoleSpec>() {
                Ok(spec) => spec,
                Err(e) => {
                    let message = e.message();
                    let problem = format!("[{name}]: {message}, skipped");
                    skipped.push(invalid(problem));
                    continue;
                }
            };
            let description = spec.description.trim();
            if description.is_empty() {
                let problem = format!("[{name}]: description is empty, skipped");
                skipped.push(invalid(problem));
                continue;
            }

            let model = match spec.model.as_deref().map(|model| models.make(model)) {
                None => None,
                Some(Ok(built)) => Some(Arc::from(built)),
                Some(Err(e)) => {
                    let problem = format!("[{name}]: {e}, skipped");
                    skipped.push(invalid(problem));
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
            fields.insert(key, scalar(&parts));
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

    if let Some(quoted) = joined
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
    {
        return quoted.replace("\\\"", "\"").replace("\\\\", "\\");
    }
    if let Some(quoted) = joined
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
    {
        return quoted.replace("''", "'");
    }
    // In an unquoted value whatever follows # is a comment
    let (plain, _) = joined.split_once(" #").unwrap_or((&joined, ""));
    plain.trim().to_owned()
}

fn unreadable(e: &io::Error) -> String {
    let reason = match e.kind() {
        io::ErrorKind::InvalidData => "not UTF-8 text".to_owned(),
        _ => cause(e),
    };
    format!("{reason}, skipped")
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
#[serde(deny_unknown_fields)]
struct Allow {
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
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

pub(crate) fn agent(dir: &Path) -> Result<(Vec<McpConfig>, Access)> {
    let path = dir.join("agent.toml");
    let invalid = invalid(&path);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Access::default()));
        }
        Err(e) => {
            let reason = cause(&e);
            return Err(invalid(reason));
        }
    };

    let file: AgentFile = parsed(&text).map_err(&invalid)?;

    let mut servers = Vec::new();
    for (name, value) in file.mcp {
        let spec = match value.try_into::<McpSpec>() {
            Ok(spec) => spec,
            Err(e) => {
                let message = e.message();
                let problem = format!("[mcp.{name}]: {message}");
                return Err(invalid(problem));
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
                    return Err(invalid(problem));
                }

                // The URL may carry a secret, so it is never shown
                if let Err(problem) = parse_url(&url) {
                    let problem = format!("[mcp.{name}]: url: {problem}");
                    return Err(invalid(problem));
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

    Ok((servers, access))
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

fn parsed<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    let e = match toml::from_str(text) {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };

    let message = e.message();
    let problem = match e.span() {
        Some(span) => {
            let line = text[..span.start].matches('\n').count() + 1;
            let spot = &text[span];
            let quoted = format!("`{spot}`");
            match spot.is_empty() || spot.contains('\n') || message.contains(&quoted) {
                true => format!("line {line}: {message}"),
                false => format!("line {line}: {spot}: {message}"),
            }
        }
        None => message.to_owned(),
    };
    Err(problem)
}

#[cfg(test)]
mod tests {
    use crate::config::{
        agent, instructions, models, roles, save_model, skills, Access, McpConfig, McpKind, Skill,
        MODEL_EXAMPLE,
    };
    use crate::model::{MediaKind, ModelSpec};
    use crate::tests::temp_dir;
    use crate::Error;
    use reaper::tilde;
    use std::collections::BTreeMap;
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
    { id = "piñata-2", effort = "high", context = 8000, max_tokens = 4096, image = true, video = true },
]

[model.slow]
type = "openai-completions"
models = [{ id = "m3" }]
"#;
        let (dir, _) = written(text);
        let read = models(&dir).expect("load");
        assert_eq!(read.names(), ["über/m1", "über/piñata-2", "slow/m3"]);
        let built = read.make("über/piñata-2").expect("make");
        assert_eq!(built.context_window(), Some(8000));
        assert!(built.takes(MediaKind::Image) && built.takes(MediaKind::Video));

        let missing = read.make("über").err().expect("missing").to_string();
        let names = "über/m1, über/piñata-2, slow/m3";
        let expected = format!("model: über: not in model.toml (models: {names})");
        assert_eq!(missing, expected);

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
                "[model.odd]\ntype = \"anthropic\"\nmodels = [{ id = \"m1\" }]\n",
                "[model.odd]: unknown variant `anthropic`, expected `openai-completions`",
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
                "[model.kimi]\ntype = \"openai-completions\"\nmodels = [{ id = \"k3\" }]\n\n[model.kimi]\n",
                "line 5: kimi: duplicate key",
            ),
            (
                "[models.\"über\"]\ntype = \"openai-completions\"\n",
                "line 1: unknown field `models`, expected `model`",
            ),
        ];

        for (text, problem) in cases {
            let (dir, shown) = written(text);
            let error = models(&dir).err().expect("error").to_string();
            assert_eq!(error, format!("config: {shown}: {problem}"));
            fs::remove_dir_all(&dir).ok();
        }

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
        let built = read.make("über café.v2/piñata-2").expect("make");
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
        let built = read.make("local-1/qwen3:32b").expect("make");
        assert_eq!(built.context_window(), None);
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
        write(
            global.clone(),
            "[reviewer]\ndescription = \"Reviews the café's code\"\n\n[tester]\ndescription = \"Writes tests\"\nmodel = \"mock/m1\"\n",
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
        let (servers, access) = agent(&dir).expect("missing");
        assert!(servers.is_empty());
        assert_eq!(access, Access::default());

        let text = "[mcp.\"workspace\"]\ntype = \"stdio\"\ncommand = \"desktop-mcp\"\nargs = [\"--quiet\"]\nenv = { LANG = \"en_US.UTF-8\" }\n\n\
            [mcp.web]\ntype = \"http\"\nurl = \"http://127.0.0.1:1/mcp\"\nenabled = false\ndescription = \"Search the web\"\n";
        fs::write(dir.join("agent.toml"), text).expect("write");
        let (read, _) = agent(&dir).expect("servers");
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

        let shown = tilde(&dir.join("agent.toml"));
        let cases = [
            (
                "[mcp.\"café\"]\ntype = \"sse\"\n",
                "[mcp.café]: unknown variant `sse`, expected `stdio` or `http`",
            ),
            (
                "[mcp.\"workspace\"]\ntype = \"stdio\"\nargs = []\n",
                "[mcp.workspace]: missing field `command`",
            ),
            (
                "[mcp.\"workspace\"]\ntype = \"stdio\"\ncommand = \"x\"\nurl = \"http://127.0.0.1:1/mcp\"\n",
                "[mcp.workspace]: unknown field `url`, expected one of `enabled`, `description`, `command`, `args`, `env`",
            ),
            (
                "[mcp.\"web\"]\ntype = \"http\"\nurl = \"localhost:3000/mcp\"\n",
                "[mcp.web]: url: expected http:// or https://",
            ),
            (
                "[mcp.\"web\"]\ntype = \"http\"\nurl = \"café/mcp\"\n",
                "[mcp.web]: url: relative URL without a base",
            ),
            (
                "[sandbox]\nnet = true\n",
                "line 1: unknown field `sandbox`, expected one of `mcp`, `allow`, `deny`",
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
        let (_, access) = agent(&dir).expect("access");
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
        ];
        for (text, problem) in cases {
            fs::write(dir.join("agent.toml"), text).expect("write");
            let error = agent(&dir).expect_err("error").to_string();
            assert_eq!(error, format!("config: {shown}: {problem}"));
        }
        fs::remove_dir_all(&dir).ok();
    }
}
