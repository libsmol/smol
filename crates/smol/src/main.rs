use anyhow::{bail, Result};
use clap::builder::NonEmptyStringValueParser;
use clap::{Parser, Subcommand};
use std::env;
use std::env::consts::EXE_SUFFIX;
use std::io::{self, IsTerminal};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod chat;
mod client;
mod conversation;
mod tui;

const AGENT: &str = "libsmol";

#[derive(Parser)]
#[command(
    name = "smol",
    version,
    about,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    mode: Option<Mode>,
    /// first task, optional
    prompt: Option<String>,
    /// resume a stored session by id, or `latest`
    #[arg(long, global = true, value_parser = NonEmptyStringValueParser::new())]
    resume: Option<String>,
    /// model to use, as the agent names it; defaults to the agent's choice
    #[arg(long, global = true)]
    model: Option<String>,
}

#[derive(Subcommand)]
enum Mode {
    /// show smol in a browser too, at an address with a token made for this run
    Web {
        /// address to listen on
        #[arg(long, default_value = "127.0.0.1:8421")]
        listen: SocketAddr,
        /// token to use on every run instead, at least 16 letters, digits or `-._~`
        #[arg(long, value_parser = tui::web::fixed_token)]
        token: Option<String>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("smol: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    if cli.mode.is_none() && !terminal {
        bail!("terminal: stdin and stdout: not both a terminal");
    }

    let installed = format!("{AGENT}{EXE_SUFFIX}");
    let exe = env::current_exe().ok();
    let beside = exe.as_ref().map(|exe| exe.with_file_name(installed));
    // Returns the name unchanged when nothing is found
    let searched = reaper::lookup(Path::new(AGENT));
    let program = match beside {
        Some(beside) if beside.is_file() => beside,
        _ if searched != Path::new(AGENT) => searched,
        _ => {
            let smol = exe.unwrap_or_else(|| PathBuf::from("smol"));
            let smol = smol.display();
            bail!("agent: {AGENT}: not found next to {smol} or in PATH");
        }
    };

    let command = [program.display().to_string()];
    let resume = cli.resume.as_deref();
    match cli.mode {
        Some(Mode::Web { listen, token }) => {
            tui::web::run(&command, cli.model, resume, listen, token, terminal)
        }
        None => tui::run(&command, cli.model, resume, cli.prompt),
    }
}

#[cfg(test)]
mod tests {
    use crate::client::Notice;
    use crate::conversation::Conversation;
    use agent_client_protocol_schema::v1::{
        ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
    };
    use serde_json::json;

    const FAKE_AGENT: &str = r#"
model() {
    printf '{"id":"model","name":"Model","category":"model","type":"select","currentValue":"%s","options":[{"value":"m1","name":"m1"},{"value":"m2-café","name":"m2-café"}]}' "$1"
}
effort() {
    printf '{"id":"effort","name":"Effort","category":"thought_level","type":"select","currentValue":"%s","options":[{"value":"low","name":"low"},{"value":"high","name":"high"}]}' "$1"
}
n=0
while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\).*/\1/p')
    case $line in
    *'"method":"initialize"'*)
        result='{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"close":{},"list":{}}}}' ;;
    *'"method":"session/list"'*'"cwd"'*)
        result='{"sessions":[{"sessionId":"s1","cwd":"/","title":"old session"}]}' ;;
    *'"method":"session/list"'*)
        result='{"sessions":[]}' ;;
    *'"method":"session/new"'*)
        n=$((n + 1))
        result="{\"sessionId\":\"s$n\",\"configOptions\":[$(model m1),$(effort high)]}" ;;
    *'"method":"session/load"'*'"sessionId":"s1"'*)
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old task"}}}}'
        result="{\"configOptions\":[$(model m1),$(effort high)]}" ;;
    *'"method":"session/load"'*)
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"unknown session"}}\n' "$id"
        continue ;;
    *'"method":"session/set_config_option"'*'"value":"m2-café"'*)
        result="{\"configOptions\":[$(model m2-café)]}" ;;
    *'"method":"session/set_config_option"'*'"value":"m1"'*)
        result="{\"configOptions\":[$(model m1),$(effort high)]}" ;;
    *'"method":"session/set_config_option"'*'"value":"low"'*)
        result="{\"configOptions\":[$(model m1),$(effort low)]}" ;;
    *'"method":"session/set_config_option"'*)
        result='{"configOptions":[]}' ;;
    *'"method":"session/prompt"'*'"text":"wait"'*)
        asked=$id
        continue ;;
    *'"method":"session/cancel"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$asked"
        continue ;;
    *'"method":"session/prompt"'*'"text":"queue"'*)
        asked=$id
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"method":"_smol/queued","params":{"sessionId":"s1"}}'
        continue ;;
    *'"result":{"prompts":[]}'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"cancelled"}}\n' "$asked"
        continue ;;
    *'"result":{"prompts":[[{'*'"text":"update"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$asked"
        continue ;;
    *'"method":"session/prompt"'*'"text":"huge"'*)
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"task: too large"}}\n' "$id"
        continue ;;
    *'"method":"session/prompt"'*'"text":"exit"'*)
        echo goodbye >&2
        exit 0 ;;
    *'"method":"_smol/shell"'*)
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"call_9","title":"ls docs","kind":"execute","status":"in_progress","_meta":{"_smol/shell":true}}}}'
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_9","status":"failed","content":[{"type":"content","content":{"type":"text","text":"alpha\nbeta"}}],"_meta":{"error":"exit 2"}}}}'
        result='{"stopReason":"end_turn"}' ;;
    *'"method":"_smol/memories"'*)
        result='{"memories":[{"id":12,"text":"Tests run with pytest","project":true},{"id":99,"text":"Prefers résumé over CV\nand naïve tests","project":false}]}' ;;
    *'"method":"_smol/forget"'*'"id":99'*)
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32002,"message":"memory: 99: not found"}}\n' "$id"
        continue ;;
    *'"method":"_smol/forget"'*)
        result='{}' ;;
    *'"method":"session/prompt"'*)
        result='{"stopReason":"end_turn"}' ;;
    *)
        continue ;;
    esac
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

    pub(crate) fn shell(script: &str) -> Vec<String> {
        let command = ["sh", "-c", script].map(str::to_owned);
        command.to_vec()
    }

    pub(crate) fn fake() -> Vec<String> {
        shell(FAKE_AGENT)
    }

    pub(crate) fn chunk(text: &str) -> ContentChunk {
        let block = ContentBlock::from(text);
        ContentChunk::new(block)
    }

    pub(crate) fn content(text: &str) -> ToolCallContent {
        let block = ContentBlock::from(text);
        ToolCallContent::from(block)
    }

    pub(crate) fn message(text: &str) -> SessionUpdate {
        let chunk = chunk(text);
        SessionUpdate::AgentMessageChunk(chunk)
    }

    pub(crate) fn end_step(talk: &mut Conversation, input: u64, output: u64) {
        let meta = json!({
            "inputTokens": input,
            "outputTokens": output,
            "decodeDuration": 0.5,
            "duration": 0.25,
        });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(input + output, 100).meta(meta);
        let ended = talk.update(&SessionUpdate::UsageUpdate(usage));
        assert!(ended);
    }

    pub(crate) fn told(text: &str) -> Notice {
        Notice {
            text: text.to_owned(),
            retry_in: None,
            step: false,
            unsaved: false,
        }
    }

    pub(crate) fn retrying(text: &str) -> Notice {
        Notice {
            retry_in: Some(2.0),
            ..told(text)
        }
    }

    /// Starts a call that starts `count` agents, the first of them named
    pub(crate) fn started(talk: &mut Conversation, count: usize) {
        let code = ToolCall::new("call_1", "eval").status(ToolCallStatus::InProgress);
        talk.update(&SessionUpdate::ToolCall(code));
        for n in 1..=count {
            let marked = json!({ "_smol/agent": true, "parent": "call_1" });
            let task = format!("Count the cafés in menu {n}");
            let name = (n == 1).then_some("cafés");
            let asked = json!({ "task": task, "role": null, "name": name });
            let id = format!("call_1.{n}");
            let opened = ToolCall::new(id, format!("agent: {task}"))
                .raw_input(asked)
                .meta(marked.as_object().cloned());
            talk.update(&SessionUpdate::ToolCall(opened));
        }
    }

    pub(crate) fn moved(talk: &mut Conversation, n: usize, status: ToolCallStatus) {
        let fields = ToolCallUpdateFields::new().status(status);
        let update = ToolCallUpdate::new(format!("call_1.{n}"), fields);
        talk.update(&SessionUpdate::ToolCallUpdate(update));
    }
}
