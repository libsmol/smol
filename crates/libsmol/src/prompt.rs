use crate::agent::{Place, Status};
use crate::config::{Role, Skill};
use crate::memory::{
    exit_code, AgentStep, Call, CallError, Ending, Exit, MemoryStep, ShellStep, StepError,
    TaskStep, MAX_OBSERVATION_CHARS,
};
use crate::model::{Media, Part};
use crate::utils::truncated;
use crate::worker::host::Added;
use crate::worker::Function;
use chrono::Utc;
use std::env::consts::OS;
use std::path::Path;

pub(crate) const TOOL: &str = "eval";
/// Mentioning tags makes models write tags into the arguments
pub(crate) const TOOL_DOC: &str = "Runs Python code.";

const UPDATE: &str = "Task update";

const RULES: &str = "- Never make up data or functions; check unfamiliar ones with help() first.
- Do the task instead of describing it.
- Report only results you obtained, and state what failed or is uncertain.";

const ANSWER: &str = "without a tool call. This ends the task. Answer as soon as you have the result; if no code is needed, answer immediately.";
const UPDATES: &str = "The user may send a \"Task update:\" while you work; it adds to or changes the current task. Text you write in a reply that runs code is shown to the user.";
const HANDED_BACK: &str = "Only your answer is returned to the agent that assigned this task, so include everything the task asks for.";
/// Errors of `agent()` and its handles go unlabeled, as the worker labels them with the function
/// the code called
pub(crate) const NESTED: &str = "not available to agents started by agent()";
pub(crate) const NO_AGENT: &str = "no such agent";
/// Also what the user sees for an agent that is aborted before it ends
pub(crate) const UNFINISHED: &str = "stopped before it answered";
pub(crate) const STOPPED_BY_CODE: &str = "stopped by stop()";
/// The worker puts it in the docs of `Agent.wait` and `Agent.stop`
pub(crate) const DONE: &str = "Agent done";

pub(crate) fn unknown_role(name: &str, roles: &[Role]) -> String {
    let names: Vec<&str> = roles.iter().map(|role| role.name.as_str()).collect();
    let names = names.join(", ");
    format!("role: {name}: unknown (roles: {names})")
}

pub(crate) fn name_running(name: &str) -> String {
    format!("name: {name}: already running")
}

/// What `Agent.status()` returns
pub(crate) fn agent_status(status: &Status, end: &Ending) -> String {
    match end {
        Ending::Answer(_) => return "answered".to_owned(),
        Ending::Error(error) => return format!("failed: {error}"),
        Ending::Interrupted {} => return STOPPED.to_owned(),
        Ending::Stopped {} => return STOPPED_BY_CODE.to_owned(),
        Ending::Unfinished {} => return UNFINISHED.to_owned(),
        Ending::Running {} => {}
    }

    let Some(since) = status.since else {
        return "waiting for a free slot".to_owned();
    };
    let seconds = since.elapsed().as_secs();
    let step = status.steps + 1;
    let running = format!("running for {seconds}s, step {step}: {}", status.doing);
    match status.preview.is_empty() {
        true => running,
        false => format!("{running}\n{}", status.preview),
    }
}

/// What an agent started by `agent()` is doing, which the user sees as well
pub(crate) const STARTING: &str = "starting";
pub(crate) const THINKING: &str = "thinking";
pub(crate) const WRITING: &str = "writing";
pub(crate) const RUNNING: &str = "running";
pub(crate) const COMPACTING: &str = "compacting";
pub(crate) const PYTHON_RESTARTED: &str = "python restarted";

pub(crate) fn retrying(reason: &str) -> String {
    format!("retrying: {reason}")
}

pub(crate) fn done(agents: &[String]) -> String {
    let names = agents.join("\n");
    format!("{DONE}:\n{names}")
}

/// None leaves view() out of the prompt
pub(crate) fn view_summary(image: bool, video: bool) -> Option<&'static str> {
    match (image, video) {
        (true, true) => Some("attaches an image or video file to this call's result"),
        (true, false) => Some("attaches an image file to this call's result"),
        (false, true) => Some("attaches a video file to this call's result"),
        (false, false) => None,
    }
}

pub(crate) fn system(
    functions: &[Function],
    servers: &[&Added],
    skills: &[Skill],
    cwd: &Path,
    sandboxed: bool,
    instructions: &[String],
    place: &Place,
) -> String {
    let mut lines = Vec::new();
    for function in functions {
        let line = format!("- {}  # {}", function.header, function.summary);
        lines.push(line);
    }
    let functions = lines.join("\n");

    let mut listed = String::new();
    for added in servers {
        let name = &added.name;
        let heading = format!("## MCP `{name}`");
        let mut parts = vec![heading];
        let description = added.server.description.trim();
        if !description.is_empty() {
            parts.push(description.to_owned());
        }
        let mut signatures = Vec::new();
        for header in &added.headers {
            let line = format!("- {header}");
            signatures.push(line);
        }
        if !signatures.is_empty() {
            parts.push(signatures.join("\n"));
        }

        let section = parts.join("\n\n");
        listed.push_str("\n\n");
        listed.push_str(&section);
    }

    let mut entries = Vec::new();
    for skill in skills {
        let name = &skill.name;
        let description = &skill.description;
        let path = skill.path.display();
        let entry = format!("## Skill `{name}`\n\n{description}\n\n- {path}");
        entries.push(entry);
    }
    // The sandbox gives Windows code TEMP instead of TMPDIR
    let temp = match cfg!(windows) {
        true => "%TEMP%",
        false => "$TMPDIR",
    };
    let skills = match entries.is_empty() {
        true => String::new(),
        false => {
            let entries = entries.join("\n\n");
            format!("\n\n# Skills\n\nWhen a task matches a skill below, read and follow its SKILL.md; paths in it are relative to its folder. Install packages its scripts need in a venv in {temp}.\n\n{entries}")
        }
    };

    let (reply, note, hand_off, roles) = match place {
        Place::Main { roles, .. } => {
            let hand_off = format!("\n\nagent() gives a task to a new agent:
- Pass independent parts of a task as a list to run them in parallel.
- Give it long side work where only the result matters, so its steps stay out of this conversation.
- With a name, it runs in the background: the task goes on after your answer until it ends, and an \"{DONE}:\" message tells you when.");
            ("reply in Markdown", UPDATES, hand_off, roles.as_slice())
        }
        Place::Delegated(_) => ("reply", HANDED_BACK, String::new(), &[][..]),
    };
    let mut entries = Vec::new();
    for role in roles {
        let entry = format!("- `{}`: {}", role.name, role.description);
        entries.push(entry);
    }
    let roles = match entries.is_empty() {
        true => String::new(),
        false => {
            let entries = entries.join("\n");
            format!("\n\n# Roles\n\nTo give a task to an agent in one of these roles, call agent(task, role=role). For a list of tasks, role is one role for all of them or a list with one role per task:\n\n{entries}")
        }
    };
    let role = match place.role() {
        Some(role) => format!(
            "\n\n# Role\n\nYour role is `{}`: {}",
            role.name, role.description
        ),
        None => String::new(),
    };

    let mut files = Vec::new();
    for text in instructions {
        let file = format!("<instructions>\n{text}\n</instructions>");
        files.push(file);
    }
    // No precedence is stated, so instructions from a cloned repo cannot override the rules above
    let instructions = match files.is_empty() {
        true => String::new(),
        false => {
            let files = files.join("\n\n");
            format!("\n\n# Instructions\n\nFollow these instructions:\n\n{files}")
        }
    };

    let cwd = cwd.display();
    let sandbox = match sandboxed {
        true => format!("\n- Your code runs in a sandbox: writing outside {cwd} and $TMPDIR, or reading some private files, fails with PermissionError. $TMPDIR is removed when the session ends.\n- If a tool fails because it cannot write its cache, rerun it with the cache in $TMPDIR."),
        false => String::new(),
    };
    let date = Utc::now().format("%Y-%m-%d");
    format!(
        "You complete tasks by running Python code with the {TOOL} tool. Each reply does one of two things:
- Run code: call {TOOL}. A reply may contain several calls; they run in order, and once one fails, the rest are skipped.
- Answer: {reply} {ANSWER}

{note}{hand_off}{role}

# Running code

All calls share one Python session: variables, functions and imports persist across calls.

A call returns what its code printed, plus the value of every top-level expression that is not None (strings verbatim, other values as repr). Output longer than {MAX_OBSERVATION_CHARS} characters is cut in the middle; this cut does not change the values in your variables. If the code fails, the result ends with \"Error:\" followed by the exception and the line that raised it.

Write each call as a complete program:
- Do as much as possible with loops, conditions, the standard library and the functions below.
- Print only what you need to see.
- Reuse the data, functions and imports the session already holds; reload data only if its source changed.
- Put code you will run again in a function.

# Functions

These functions need no import. Use them instead of writing your own. Create new files with open().

## Built-in

{functions}{listed}{skills}{roles}

# Rules

{RULES}

# Environment

- Your code runs on the user's computer ({OS}) in {cwd}; the files and programs a task mentions are there.{sandbox}
- Today is {date} (UTC).{instructions}",
    )
}

pub(crate) fn task(step: &TaskStep) -> Vec<Part> {
    request("New task", step)
}

pub(crate) fn update(step: &TaskStep) -> Vec<Part> {
    request(UPDATE, step)
}

fn request(label: &str, step: &TaskStep) -> Vec<Part> {
    let task = &step.text;
    let mut text = format!("{label}:\n{task}");
    if !step.files.is_empty() {
        text.push_str("\n\nAttached files:");
    }

    let mut parts = vec![Part::Text(text)];
    for file in &step.files {
        let path = &file.path;
        if let Some(media) = &file.media {
            let attached = Part::Media(path.clone(), media.clone());
            parts.push(attached);
            continue;
        }

        let (tag, key) = match (file.resource, path.ends_with('/')) {
            (true, _) => ("resource", "uri"),
            (false, true) => ("directory", "path"),
            (false, false) => ("file", "path"),
        };
        let lines = match file.lines {
            Some(lines) => format!(" lines=\"{}\"", span(lines)),
            None => String::new(),
        };
        let content = file.content.trim_end_matches('\n');
        let block = format!("<{tag} {key}=\"{path}\"{lines}>\n{content}\n</{tag}>");
        parts.push(Part::Text(block));
    }

    parts
}

fn span((first, last): (u32, u32)) -> String {
    match first == last {
        true => first.to_string(),
        false => format!("{first}-{last}"),
    }
}

pub(crate) fn result(printed: Option<&str>, viewed: &[(String, Media)]) -> Vec<Part> {
    let mut parts = Vec::new();
    if let Some(printed) = printed {
        let printed = printed.to_owned();
        parts.push(Part::Text(printed));
    }
    for (label, media) in viewed {
        let viewed = Part::Media(label.clone(), media.clone());
        parts.push(viewed);
    }

    parts
}

pub(crate) const NO_OUTPUT: &str = "(no output)";

pub(crate) fn cut(count: usize) -> String {
    format!("[{count} characters cut]")
}

/// Cut-off replies are never sent back, so the model cannot see the last reply this mentions
const CUT: &str = "Your last reply was discarded: it hit the output length limit, so nothing ran. Write less per reply and split long work across several replies.";
const NO_CODE: &str = "not run: no Python code in the code argument";
pub(crate) const STOPPED: &str = "interrupted by the user";
const UNRUN: &str = "interrupted by the user before the code ran";
const SKIPPED: &str = "not run: an earlier call failed";
const UNREPLIED: &str = "The user interrupted you before you replied.";

/// A step with calls reports this in their replies instead
pub(crate) fn error(step: &AgentStep) -> Option<String> {
    let text = explained(step)?;
    Some(format!("Error:\n{text}"))
}

fn explained(step: &AgentStep) -> Option<String> {
    let error = step.error.as_ref()?;
    let silent = step.text.trim().is_empty() && step.calls.is_empty();
    let text = match error {
        StepError::Cut => CUT,
        StepError::Interrupted if silent => UNREPLIED,
        StepError::Interrupted => STOPPED,
        StepError::Model(_) => return Some(error.to_string()),
    };
    Some(text.to_owned())
}

/// A call that never ran still needs a reason
pub(crate) fn call_error(step: &AgentStep, call: &Call) -> Option<String> {
    let text = call_explained(step, call)?;
    Some(format!("Error:\n{text}"))
}

fn call_explained(step: &AgentStep, call: &Call) -> Option<String> {
    let interrupted = step.error == Some(StepError::Interrupted);
    let text = match (&call.error, &call.result) {
        (Some(CallError::NoTool(name)), _) => {
            let text =
                format!("not run: there is no tool named \"{name}\"; the only tool is {TOOL}");
            return Some(text);
        }
        (Some(CallError::NoCode), _) => NO_CODE,
        (Some(CallError::Interrupted), Some(_)) => STOPPED,
        (Some(CallError::Interrupted), None) => UNRUN,
        (Some(CallError::Execution(text)), _) => text,
        (None, Some(_)) => return None,
        (None, None) if interrupted => UNRUN,
        (None, None) => SKIPPED,
    };
    Some(text.to_owned())
}

pub(crate) fn shell(step: &ShellStep) -> String {
    let ShellStep {
        command,
        shell,
        output,
        end,
    } = step;
    let ran = format!("The user ran this {shell} command in the working directory:\n$ {command}");
    let output = match output.trim_end() {
        "" => "Output: none".to_owned(),
        output => format!("Output (stdout and stderr):\n{output}"),
    };
    let end = match end {
        Exit::Exited(code) => {
            let code = exit_code(*code);
            format!("Exit code: {code}")
        }
        Exit::Signaled(signal) => format!("Signal {signal} ended it."),
        Exit::Interrupted => "The user interrupted it before it exited.".to_owned(),
        Exit::Unstarted(reason) => return format!("{ran}\nIt did not start: {reason}"),
    };
    format!("{ran}\n{output}\n{end}")
}

/// `updates` follow `task` and are left out without one
pub(crate) fn summarized(summary: &str, task: Option<&str>, updates: &[&str]) -> String {
    let mut text = format!(
        "Earlier messages were replaced by this summary:\n\n<summary>\n{summary}\n</summary>"
    );
    let Some(task) = task else {
        return text;
    };

    let current = format!("\n\nCurrent task:\n{task}");
    text.push_str(&current);
    for update in updates {
        let update = format!("\n\n{UPDATE}:\n{update}");
        text.push_str(&update);
    }
    text
}

pub(crate) fn unseen(label: &str, media: &Media) -> String {
    let named = named(label, media);
    let word = media.kind.word();
    format!("The {named} is not shown: the current model does not take {word}s.")
}

pub(crate) fn missing(label: &str, media: &Media) -> String {
    let named = named(label, media);
    format!("The {named} is not shown: its saved copy is gone.")
}

pub(crate) fn dropped(label: &str, media: &Media) -> String {
    let named = named(label, media);
    format!("The {named} is not shown: only the latest images and videos are.")
}

/// Viewed and attached media go by path, and media from MCP by the function called
fn named(label: &str, media: &Media) -> String {
    let word = media.kind.word();
    match media.from_call {
        true => format!("{word} from {label}"),
        false => format!("{word} {label}"),
    }
}

/// The media itself comes right after the tag
pub(crate) fn media_tag(label: &str, media: &Media) -> String {
    let tag = media.kind.word();
    let source = match media.from_call {
        true => "from",
        false => "path",
    };
    format!("<{tag} {source}=\"{label}\"/>")
}

/// `reason` is the lowercase reason `Worker` reports
pub(crate) fn python(reason: &str) -> String {
    format!("Python {reason}")
}

pub(crate) fn exited(end: Option<&Exit>) -> String {
    match end {
        Some(Exit::Exited(code)) => {
            let code = exit_code(*code);
            format!("exited with code {code}")
        }
        Some(Exit::Signaled(signal)) => format!("was ended by signal {signal}"),
        _ => "exited".to_owned(),
    }
}

/// `said` is the output or error from before the worker came up
pub(crate) fn unstarted(said: &str) -> String {
    match said {
        "" => "did not start".to_owned(),
        said => format!("did not start: {said}"),
    }
}

/// Errors from MCP functions, which the Python side prefixes with `server.function:`
pub(crate) const NO_SERVER: &str = "server disconnected";
pub(crate) const SILENT_ERROR: &str = "the tool reported an error without a message";

pub(crate) fn unshown(what: &str, mime: &str, size: usize) -> String {
    format!("[{what}: {mime}, {size} bytes, not shown]")
}

pub(crate) fn unkept(label: &str, reason: &str) -> String {
    format!("[image from {label}: {reason}]")
}

pub(crate) fn uncopied(reason: &str) -> String {
    format!("not copied: {reason}")
}

pub(crate) fn linked(uri: &str) -> String {
    format!("[resource link: {uri}]")
}

pub(crate) fn too_large(size: u64) -> String {
    format!("{size} bytes, too large to attach; read the parts you need in code")
}

pub(crate) fn undecoded(size: u64) -> String {
    format!("{size} bytes, not UTF-8 text; if it is text in another encoding, decode it in code")
}

pub(crate) fn unserializable(key: &str, kind: &str) -> String {
    format!("{key}: {kind} is not JSON-serializable")
}

pub(crate) fn failure(label: &str, object: &str, reason: &str) -> String {
    format!("{label}: {object}: {reason}")
}

pub(crate) fn http_status(code: u16, excerpt: &str) -> String {
    match excerpt {
        "" => format!("http {code}"),
        excerpt => format!("http {code}: {excerpt}"),
    }
}

pub(crate) fn timed_out(seconds: u64) -> String {
    format!("received no data for {seconds}s")
}

pub(crate) fn media_too_large(word: &str, size: u64, limit: u64) -> String {
    format!("{size} bytes, over the {limit}-byte limit for an attached {word}")
}

pub(crate) const IRREGULAR: &str = "not a regular file";

pub(crate) fn too_many(limit: usize) -> String {
    format!("grep: more than {limit} MiB of matches, narrow the pattern or the path")
}

pub(crate) fn unfetchable(url: &str, mime: &str) -> String {
    format!("fetch: {url}: {mime}, only HTML and text pages can be fetched")
}

/// Matches the reason reqwest gives for too many redirects
pub(crate) const TOO_MANY_REDIRECTS: &str = "too many redirects";

pub(crate) fn cut_at(limit: u64) -> String {
    format!("[cut at {limit} MiB]")
}

pub(crate) fn no_connection(host: &str, reason: &str) -> String {
    format!("cannot connect to {host}: {reason}")
}

pub(crate) fn unviewable(path: &str, images: &str, videos: &str) -> String {
    format!("view: {path}: not an image ({images}) or video ({videos})")
}

pub(crate) const RESTARTED: &str =
    "Python was restarted: variables, functions and imports from earlier calls are gone.";

pub(crate) const NAMER: &str = "Reply with a short title for a conversation that starts with the message below: a few words in its language, without quotes or ending punctuation. Do not answer the message.";

pub(crate) const SUMMARIZER: &str = "You summarize an AI agent's work so it can continue after its earlier messages are removed. In the conversation, [Agent] is the agent's message, [Code] the Python code it ran, [Observation] what the code printed and [Error] what failed. Write only the summary, in the language of the tasks; do not continue the work or answer the tasks.";

const SUMMARY_FORMAT: &str = "Use exactly this format with every section (\"(none)\" if empty), in short bullets with exact paths, names and values:

## Tasks
- [each task the agent was given, with its task updates and its answer if one was given]
## Done
- [what the agent has done for the last task]
## Found
- [facts needed to continue: exact values, paths, line numbers, names, URLs, error messages]
## Failed
- [what failed and why, so it is not repeated]
## Left
1. [what is left to do for the last task, in order]";

/// Attachments, printed output, errors and command output are each cut to `max` characters
pub(crate) fn transcript(steps: &[&MemoryStep], max: usize) -> String {
    let mut entries = Vec::new();
    for step in steps {
        let (label, task) = match step {
            MemoryStep::Task(task) => ("Task", task),
            MemoryStep::Update(task) => (UPDATE, task),
            MemoryStep::Action(step) => {
                let shown = !step.dropped();
                let text = step.text.trim();
                if shown && !text.is_empty() {
                    let said = format!("[Agent]: {text}");
                    entries.push(said);
                }
                let calls = match shown {
                    true => step.calls.as_slice(),
                    false => &[],
                };
                for call in calls {
                    if !call.unread() {
                        let code = format!("[Code]: {}", call.code);
                        entries.push(code);
                    }
                    let result = call.result.as_ref();
                    let printed = result.and_then(|result| result.printed.as_deref());
                    if let Some(printed) = printed {
                        let printed = truncated(printed, max);
                        let observed = format!("[Observation]: {printed}");
                        entries.push(observed);
                    }
                    for (label, media) in result.map(|result| &result.media).into_iter().flatten() {
                        let viewed = placeholder(label, media);
                        entries.push(viewed);
                    }
                    if let Some(error) = call_explained(step, call) {
                        let error = truncated(&error, max);
                        let error = format!("[Error]: {error}");
                        entries.push(error);
                    }
                }
                let error = explained(step).filter(|_| calls.is_empty());
                if let Some(error) = error {
                    let error = truncated(&error, max);
                    let error = format!("[Error]: {error}");
                    entries.push(error);
                }
                continue;
            }
            MemoryStep::Shell(ran) => {
                let text = shell(ran);
                let text = truncated(&text, max);
                let entry = format!("[Shell]: {text}");
                entries.push(entry);
                continue;
            }
            MemoryStep::Done { agents } => {
                let names = agents.join(", ");
                let entry = format!("[{DONE}]: {names}");
                entries.push(entry);
                continue;
            }
            MemoryStep::Summary(_) | MemoryStep::Restarted => continue,
        };

        let text = &task.text;
        let asked = format!("[{label}]: {text}");
        entries.push(asked);
        for file in &task.files {
            let path = &file.path;
            let attached = match &file.media {
                Some(media) => placeholder(path, media),
                None => {
                    let content = file.content.trim_end_matches('\n');
                    let content = truncated(content, max);
                    let lines = match file.lines {
                        Some(lines) => format!(", lines {}", span(lines)),
                        None => String::new(),
                    };
                    format!("[Attached {path}{lines}]: {content}")
                }
            };
            entries.push(attached);
        }
    }

    entries.join("\n\n")
}

fn placeholder(label: &str, media: &Media) -> String {
    let named = named(label, media);
    format!("[Attached {named}]")
}

pub(crate) fn summary(transcript: &str, previous: Option<&str>, trailing: bool) -> String {
    let conversation = format!("<conversation>\n{transcript}\n</conversation>");
    let mut parts = match previous {
        Some(previous) => {
            let previous = format!("<previous-summary>\n{previous}\n</previous-summary>");
            let combine = "The previous summary covers the work before this conversation. Merge both into one summary, keeping what is still needed; where they disagree, the conversation is newer.";
            vec![previous, conversation, combine.to_owned()]
        }
        None => vec![conversation],
    };

    if trailing {
        let note = "The conversation above leaves out its latest messages; they are kept word for word after your summary.";
        parts.push(note.to_owned());
    }

    parts.push(SUMMARY_FORMAT.to_owned());
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use crate::agent::tests::main_place;
    use crate::agent::Place;
    use crate::config::{Role, Skill};
    use crate::memory::{
        AgentStep, Call, CallError, CodeResult, Exit, MemoryStep, ShellStep, StepError,
    };
    use crate::model::tests::png;
    use crate::model::Media;
    use crate::prompt::{
        call_error, error, media_tag, shell, system, transcript, unseen, view_summary,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn system_sections() {
        let instructions = ["Reply in French".to_owned(), "Indent with tabs".to_owned()];
        let skills = [Skill {
            name: "weekly".to_owned(),
            description: "Write the weekly report".to_owned(),
            path: PathBuf::from("/home/zoë/.smol/skills/weekly/SKILL.md"),
        }];
        let cwd = Path::new("/home/zoë/work");

        let main = main_place(Vec::new());
        let text = system(&[], &[], &skills, cwd, false, &instructions, &main);
        let place =
            ") in /home/zoë/work; the files and programs a task mentions are there.\n- Today";
        assert!(text.contains(place), "{text}");
        let temp = match cfg!(windows) {
            true => "%TEMP%",
            false => "$TMPDIR",
        };
        let skill = format!(
            "\n\n# Skills\n\n\
             When a task matches a skill below, read and follow its SKILL.md; paths in it are relative to its folder. Install packages its scripts need in a venv in {temp}.\n\n\
             ## Skill `weekly`\n\nWrite the weekly report\n\n- /home/zoë/.smol/skills/weekly/SKILL.md\n\n# Rules\n\n"
        );
        assert!(text.contains(&skill), "{text}");
        let files = " (UTC).\n\n# Instructions\n\n\
                     Follow these instructions:\n\n\
                     <instructions>\nReply in French\n</instructions>\n\n\
                     <instructions>\nIndent with tabs\n</instructions>";
        assert!(text.ends_with(files), "{text}");

        let plain = system(&[], &[], &[], cwd, true, &[], &main);
        assert!(!plain.contains("<instructions"), "{plain}");
        assert!(!plain.contains("# Skills"), "{plain}");
        assert!(!plain.contains("# Roles"), "{plain}");
        let sandbox = "are there.\n- Your code runs in a sandbox: writing outside /home/zoë/work and $TMPDIR, ";
        assert!(plain.contains(sandbox), "{plain}");
    }

    #[test]
    fn roles_described() {
        let role = |name: &str, description: &str| Role {
            name: name.to_owned(),
            description: description.to_owned(),
            model: None,
        };
        let reviewer = role("reviewer", "Reviews the café's code");
        let roles = vec![reviewer.clone(), role("tester", "Writes naïve tests")];
        let cwd = Path::new("/home/zoë/work");

        let place = main_place(roles);
        let main = system(&[], &[], &[], cwd, false, &[], &place);
        let listed = "\n\n# Roles\n\n\
                      To give a task to an agent in one of these roles, call agent(task, role=role). \
                      For a list of tasks, role is one role for all of them or a list with one role per task:\n\n\
                      - `reviewer`: Reviews the café's code\n\
                      - `tester`: Writes naïve tests\n\n# Rules\n\n";
        assert!(main.contains(listed), "{main}");
        assert!(!main.contains("# Role\n"), "{main}");

        let played = Place::Delegated(Some(reviewer));
        let helper = system(&[], &[], &[], cwd, false, &[], &played);
        let own = "include everything the task asks for.\n\n# Role\n\n\
                   Your role is `reviewer`: Reviews the café's code\n\n# Running code";
        assert!(helper.contains(own), "{helper}");
        assert!(!helper.contains("# Roles"), "{helper}");
        let plain = system(&[], &[], &[], cwd, false, &[], &Place::Delegated(None));
        assert!(!plain.contains("# Role"), "{plain}");
    }

    #[test]
    fn view_summaries() {
        let both = Some("attaches an image or video file to this call's result");
        assert_eq!(view_summary(true, true), both);
        let images = Some("attaches an image file to this call's result");
        assert_eq!(view_summary(true, false), images);
        let videos = Some("attaches a video file to this call's result");
        assert_eq!(view_summary(false, true), videos);
        assert_eq!(view_summary(false, false), None);
    }

    /// The model cannot tell from the record how far it got before the interrupt
    #[test]
    fn interrupt_explained() {
        let stopped = |text: &str, calls: Vec<Call>| AgentStep {
            text: text.to_owned(),
            calls,
            error: Some(StepError::Interrupted),
            ..AgentStep::default()
        };
        let unreplied = "The user interrupted you before you replied.";
        let told = format!("Error:\n{unreplied}");
        assert_eq!(error(&stopped("", Vec::new())), Some(told));
        let plain = "Error:\ninterrupted by the user";
        let prose = stopped("Roses are red", Vec::new());
        assert_eq!(error(&prose).as_deref(), Some(plain));

        let written = Call {
            code: "open('half.txt', 'w')".to_owned(),
            ..Call::default()
        };
        let ran = Call {
            result: Some(CodeResult::default()),
            error: Some(CallError::Interrupted),
            ..written.clone()
        };
        let step = stopped("First, a note.", vec![ran.clone(), written.clone()]);
        assert_eq!(call_error(&step, &ran).as_deref(), Some(plain));
        let unrun = call_error(&step, &written);
        let expected = "Error:\ninterrupted by the user before the code ran";
        assert_eq!(unrun.as_deref(), Some(expected));
        let failed = AgentStep {
            error: None,
            ..step
        };
        let skipped = call_error(&failed, &written);
        let expected = "Error:\nnot run: an earlier call failed";
        assert_eq!(skipped.as_deref(), Some(expected));

        let step = MemoryStep::Action(Box::new(stopped("", Vec::new())));
        let text = transcript(&[&step], usize::MAX);
        assert_eq!(text, format!("[Error]: {unreplied}"));
    }

    #[test]
    fn shell_described() {
        let ran = |output: &str, end: Exit| ShellStep {
            command: "grep order log.txt".to_owned(),
            shell: "fish".to_owned(),
            output: output.to_owned(),
            end,
        };
        let head =
            "The user ran this fish command in the working directory:\n$ grep order log.txt\n";

        let found = shell(&ran("order 1\norder 2\n", Exit::Exited(0)));
        let expected = format!("{head}Output (stdout and stderr):\norder 1\norder 2\nExit code: 0");
        assert_eq!(found, expected);
        let none = shell(&ran("", Exit::Exited(1)));
        assert_eq!(none, format!("{head}Output: none\nExit code: 1"));
        let killed = shell(&ran("", Exit::Signaled(9)));
        assert_eq!(killed, format!("{head}Output: none\nSignal 9 ended it."));
        #[cfg(windows)]
        {
            let crashed = shell(&ran("", Exit::Exited(0xC0000005_u32 as i32)));
            assert_eq!(
                crashed,
                format!("{head}Output: none\nExit code: 0xC0000005")
            );
        }
        let stopped = shell(&ran("", Exit::Interrupted));
        let expected = format!("{head}Output: none\nThe user interrupted it before it exited.");
        assert_eq!(stopped, expected);
        let missing = shell(&ran("", Exit::Unstarted("fish not found".to_owned())));
        assert_eq!(missing, format!("{head}It did not start: fish not found"));

        // Summaries see the same text
        let step = MemoryStep::Shell(ran("", Exit::Exited(1)));
        let text = transcript(&[&step], usize::MAX);
        assert_eq!(text, format!("[Shell]: {head}Output: none\nExit code: 1"));
    }

    #[test]
    fn called_media() {
        let called = Media {
            from_call: true,
            ..png("a.png")
        };
        let tag = media_tag("screen.grab", &called);
        assert_eq!(tag, "<image from=\"screen.grab\"/>");
        let note = unseen("screen.grab", &called);
        let expected =
            "The image from screen.grab is not shown: the current model does not take images.";
        assert_eq!(note, expected);

        let result = CodeResult {
            printed: Some("Captured".to_owned()),
            media: vec![("screen.grab".to_owned(), called)],
            ..CodeResult::default()
        };
        let call = Call {
            code: "screen.grab()".to_owned(),
            result: Some(result),
            ..Call::default()
        };
        let step = AgentStep {
            calls: vec![call],
            ..AgentStep::default()
        };
        let step = MemoryStep::Action(Box::new(step));
        let transcript = transcript(&[&step], 100);
        let placeholder =
            "[Code]: screen.grab()\n\n[Observation]: Captured\n\n[Attached image from screen.grab]";
        assert_eq!(transcript, placeholder);
    }
}
