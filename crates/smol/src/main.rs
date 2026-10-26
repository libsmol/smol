use anyhow::{bail, Result};
use clap::builder::NonEmptyStringValueParser;
use clap::Parser;
use std::env;
use std::env::consts::EXE_SUFFIX;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod chat;
mod client;
mod tui;

const AGENT: &str = "libsmol";

#[derive(Parser, Debug)]
#[command(name = "smol", version, about)]
struct Cli {
    /// first task, optional
    prompt: Option<String>,
    /// resume a stored session by id, or `latest`
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    resume: Option<String>,
    /// model to use, as the agent names it; defaults to the agent's choice
    #[arg(long)]
    model: Option<String>,
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
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("terminal: stdin and stdout: not both a terminal");
    }

    let installed = format!("{AGENT}{EXE_SUFFIX}");
    let exe = env::current_exe().ok();
    let beside = exe.as_ref().map(|exe| exe.with_file_name(installed));
    // Returns the name unchanged when nothing is found
    let searched = reaper::program(Path::new(AGENT));
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
    tui::run(&command, cli.model, resume, cli.prompt)
}

#[cfg(test)]
mod tests {
    use crate::Cli;
    use clap::error::ErrorKind;
    use clap::Parser;

    const FAKE_AGENT: &str = r#"
model() {
    printf '{"id":"model","name":"Model","category":"model","type":"select","currentValue":"%s","options":[{"value":"m1","name":"m1"},{"value":"m2-café","name":"m2-café"}]}' "$1"
}
n=0
while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case $line in
    *'"method":"initialize"'*)
        result='{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"close":{},"list":{}}}}' ;;
    *'"method":"session/list"'*'"cwd"'*)
        result='{"sessions":[{"sessionId":"s1","cwd":"/","title":"old session"}]}' ;;
    *'"method":"session/list"'*)
        result='{"sessions":[]}' ;;
    *'"method":"session/new"'*)
        n=$((n + 1))
        result="{\"sessionId\":\"s$n\",\"configOptions\":[$(model m1)]}" ;;
    *'"method":"session/load"'*'"sessionId":"s1"'*)
        printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old task"}}}}'
        result="{\"configOptions\":[$(model m1)]}" ;;
    *'"method":"session/load"'*)
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"unknown session"}}\n' "$id"
        continue ;;
    *'"method":"session/set_config_option"'*'"value":"m2-café"'*)
        result="{\"configOptions\":[$(model m2-café)]}" ;;
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

    #[test]
    fn version_shown() {
        let shown = Cli::try_parse_from(["smol", "--version"]).expect_err("version");
        assert_eq!(shown.kind(), ErrorKind::DisplayVersion);
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(shown.to_string(), format!("smol {version}\n"));
    }

    #[test]
    fn empty_resume_refused() {
        let empty = Cli::try_parse_from(["smol", "--resume", ""]);
        assert!(empty.is_err());

        let named = Cli::try_parse_from(["smol", "--resume", "old session"]).expect("parse");
        assert_eq!(named.resume.as_deref(), Some("old session"));
    }
}
