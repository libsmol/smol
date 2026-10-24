# smol

A CodeMode agent for your terminal and any ACP client. The model gets one tool,
`eval`, and does everything by writing Python in a session that keeps its
variables, functions and imports from step to step.

Inspired by [smolagents](https://github.com/huggingface/smolagents).

A real run with DeepSeek on smol's own source:

![smol counting the lines of its own source](https://raw.githubusercontent.com/libsmol/smol/main/docs/demo.gif)

## Why smol

- **CodeMode, natively.** Acting means writing code. One call can walk a
  directory, read a hundred files, call several tools, filter what comes back
  and print only what matters, where a tool-per-action agent would need dozens
  of round trips. Intermediate data stays in Python variables instead of
  passing through the context, so long tasks take far fewer turns and tokens.
- **Orchestration in code.** MCP servers show up as Python objects, so the
  model chains, batches, loops over and retries their tools with ordinary
  code, and keeps the results around for the next step.
- **One tool.** The model sees a single `eval(code)` tool. There is no menu of
  tools to choose from or route between; its attention goes to the task.
- **Python.** The language models write most fluently, with a standard library
  that covers files, data, text, HTTP and more.
- **A tiny prompt.** The system prompt is under 1,000 tokens: how the session
  works, the built-in functions and a few rules. No persona, no imposed
  workflow, nothing that holds a capable model back.
- **No permission prompts.** It never stops to ask before running code,
  writing a file or calling a program. See the warning below.
- **Standard ACP.** `libsmol` speaks the
  [Agent Client Protocol](https://agentclientprotocol.com), so Zed or any other
  ACP client can drive it; `smol` is just one such client. The few extras that
  ACP has no message for (goals, `!` commands, queued updates) use `_smol/`
  extensions, which other clients ignore as the protocol requires.

> [!WARNING]
> **There is no sandbox and no permission prompt.** The model's code runs on
> your machine, as you, in the session's working directory, with access to
> everything you can access: your files, your network, your credentials and
> the programs on your `PATH`. Use it in directories and on machines where
> that is acceptable, and never give it tasks that need accounts or secrets
> you would not hand to the model.

## Install

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/libsmol/smol/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/libsmol/smol/main/install.ps1 | iex
```

Both download the latest release from GitHub, check it against the release's
`SHA256SUMS` and put `smol` and `libsmol` in `~/.local/bin` (on Windows
that folder is also added to your `PATH`). Set `SMOL_VERSION` (for example
`v0.1.0`) to install a given release and `SMOL_INSTALL_DIR` to install
somewhere else. Running the script again upgrades.

Releases are built for Linux (x86_64, arm64), macOS (Apple silicon, Intel)
and Windows (x86_64).

With a recent stable Rust you can also install from crates.io, where the
agent is `libsmol` and the terminal UI is `libsmol-tui` (its program is still
`smol`). Keep `--locked`, which builds with the dependency versions the release
was tested with:

```sh
cargo install --locked libsmol libsmol-tui
```

To build from a checkout, run `./build.sh` (`build.ps1` on Windows).

## Configure a model

Configuration lives in `~/.smol` (or the directory in `SMOL_CONFIG_DIR`). It is
read each time a session opens, so `:new` picks up changes.

When `~/.smol/model.toml` doesn't exist, `smol` first runs `libsmol setup`. It
asks for the base URL of an OpenAI-compatible endpoint, its API key and a model,
picked from the endpoint's model list or typed in, and writes the file. The
context window is filled in when the list reports it. Run `libsmol setup` again
to replace it. Other ACP clients that support terminal authentication offer the
same setup; with the rest, run it in a terminal.

Base URLs of some common providers:

| Provider | Base URL |
| --- | --- |
| OpenAI | `https://api.openai.com/v1` |
| DeepSeek | `https://api.deepseek.com/v1` |
| OpenRouter | `https://openrouter.ai/api/v1` |
| Moonshot (Kimi) | `https://api.moonshot.ai/v1`, in China `https://api.moonshot.cn/v1` |
| Alibaba Cloud Model Studio (Qwen) | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| Zhipu (GLM) | `https://open.bigmodel.cn/api/paas/v4`, Z.ai `https://api.z.ai/api/paas/v4` |
| SiliconFlow | `https://api.siliconflow.com/v1`, in China `https://api.siliconflow.cn/v1` |
| Ollama (local) | `http://localhost:11434/v1` |

`~/.smol/model.toml` lists the models. Each `[model.<provider>]` table is one
OpenAI-compatible chat completions endpoint:

```toml
[model.deepseek]
type = "openai-completions"
api = "https://api.deepseek.com/v1"
api_key = "sk-..."
models = [{ id = "deepseek-chat", context = 65536 }]

[model.local]
type = "openai-completions"
api = "http://localhost:11434/v1"
api_key = "none"
models = [{ id = "qwen3:32b", context = 32768 }]
```

Models are named `<provider>/<id>`, for example `deepseek/deepseek-chat`; the
first one is the default.

| Field | Meaning |
| --- | --- |
| `type` | Only `openai-completions` for now. |
| `api` | Base URL. Without it the model goes to `https://api.openai.com/v1` and `OPENAI_API_KEY` is used when `api_key` is missing. |
| `api_key` | Sent as a bearer token. `"none"` or empty sends no key. |
| `id` | The model name the endpoint expects. |
| `context` | Context window in tokens. With it, older steps are summarized before the window fills; without it, nothing is compacted. |
| `max_tokens` | Output limit per reply. Some providers count reasoning in it, so a small value can cut a reply off mid-thought. |
| `effort` | Sent as `reasoning_effort`. |
| `image`, `video` | `true` if the model accepts images or videos. Then `view()` and attached media are sent to it; otherwise it gets a note that the media is not shown. |

Keys are stored in plain text, so keep `model.toml` readable only by you.

## Use it

Run `smol` in the directory you want to work in, then type a task.

- `Esc` interrupts the running step. Typing while a task runs queues the text;
  it reaches the model as a task update after the current step.
- A line starting with `!` runs a shell command in the session directory
  (`$SHELL`, or `cmd` on Windows). The model sees the command and its output
  with your next message.
- Paths written as `@path` are attached to the message.
- `ctrl+o` expands long output, `ctrl+d` quits.

| Command | What it does |
| --- | --- |
| `:goal <text>` | Keep working until a checker agrees the goal is met, or decides it needs something only you can give. `:goal` shows it, `:goal resume` continues, `:goal clear` drops it. |
| `:new` | Start a new session (re-reads the configuration). |
| `:resume <session>` | Resume an earlier session. `:resume` lists them. |
| `:model <name>` | Switch the model. `:model` lists them. |
| `:mcp <on\|off> <server>` | Turn an MCP server on (again) or off. `:mcp` lists them. |
| `:quit` | Quit. |

`smol "<task>"` starts with a task, `smol --resume latest` resumes the last
session and `smol --model <name>` picks the model. Sessions are saved as JSON
under `~/.smol/history`, with copies of the images and videos the model looked
at.

### What the code can use

Besides the Python standard library, every session has these functions:

| Function | |
| --- | --- |
| `ls(path=".", recursive=False)` | List a directory; recursive listing skips `.git` and `.gitignore`d paths. |
| `grep(pattern, path=".", glob=None)` | Search files with a Rust regex, skipping `.gitignore`d and binary files. |
| `edit(path, old, new)` | Replace text that appears exactly once in a file. |
| `fetch(url)` | Get a web page as Markdown. |
| `view(path)` | Show an image or video to the model (offered only when the model takes media). |
| `help(name)` | Print a function's signature and docs, including code defined in earlier steps. |

MCP tools appear as Python objects named after their server, for example
`github.search_issues(query="...")`.

## MCP servers

Add servers to `~/.smol/agent.toml`:

```toml
[mcp.github]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "..." }

[mcp.docs]
type = "http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer ..." }
enabled = false
```

| Field | Meaning |
| --- | --- |
| `type` | `stdio` (start a program) or `http` (streamable HTTP). |
| `command`, `args`, `env` | For `stdio`. |
| `url`, `headers` | For `http`. |
| `description` | Added to the prompt next to the server's functions. |
| `enabled` | `false` to keep it off until `:mcp on <server>`. |

## Instructions and skills

When a session opens, `AGENTS.md` (or `CLAUDE.md`) is read from the
configuration directory and from every directory from the filesystem root down
to the working directory, and added to the system prompt.

Skills are folders with a `SKILL.md` whose front matter has a `description`
(and optionally a `name`):

```markdown
---
name: weekly-report
description: Write the weekly report from the git log and the issue tracker.
---
...
```

They are read from `~/.smol/skills/<skill>/SKILL.md` and from
`.smol/skills/<skill>/SKILL.md` in each directory down to the working
directory; a deeper one wins over one with the same name. Only the name,
description and path go into the prompt; the model reads the file when it
needs it.

## Zed and other ACP clients

`libsmol` takes no arguments and speaks ACP on stdin and stdout, so any
client that can launch a custom agent can use it. In Zed, add it to
`agent_servers` in your settings (use the full path if Zed does not find it on
its `PATH`):

```json
{
  "agent_servers": {
    "smol": {
      "type": "custom",
      "command": "libsmol",
      "args": []
    }
  }
}
```

It supports loading, listing and closing sessions, a model option and an option
per MCP server, and it reads the same `~/.smol` configuration as `smol`. Code
added with **Add to Agent Thread** reaches the model as Zed shows it, unsaved
edits included, with its file and lines marked; Zed's other `@` mentions, such
as diagnostics, are passed on as well.

## Logs

`libsmol` writes a log to `~/.smol/logs/libsmol.<date>.log`, one file
per day (UTC) with the last 5 kept. It records the program starting and
exiting, sessions opening and closing, each step's tool calls, tokens and
duration, retries, interrupts, compaction, goal verdicts, Python restarts, MCP
connections, failures and crashes. It does not record your prompts, the
model's replies, code, program output, shell commands or keys; an error message
may quote the first words of a reply the model got wrong. Times are UTC, and
lines from a session carry its id:

```
2026-10-23 08:12:03  INFO {id=19a0b7c3d2e-4f1a2b3c}: session: opened [cwd=~/work/shop, model=deepseek/deepseek-chat]
2026-10-23 08:12:09  INFO {id=19a0b7c3d2e-4f1a2b3c}: step: done [calls=1, tokens=3121/86, elapsed=2.4s]
```

Please attach it when you report a bug.

`SMOL_LOG` sets what is logged, in
[`RUST_LOG` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):
the default is `warn,libsmol=info`, `SMOL_LOG=warn` keeps only problems,
`SMOL_LOG=off` turns the log off.

## Limitations

- **The Python is [RustPython](https://github.com/RustPython/RustPython)**,
  built into `libsmol`. Pure-Python code and the standard library work, but
  C extensions such as numpy or pandas cannot be imported. Code can still run
  a system Python through `subprocess` when one is installed.
- **Interrupting** stops code in place, and the session keeps its variables.
  Code blocked where an interrupt cannot reach it (for example `os.system`,
  waiting on a lock, or most blocking calls on Windows) gets 3 seconds, then
  the Python process is restarted and its variables are lost. The model is
  told when that happens.
- **Only OpenAI-compatible chat completions endpoints** are supported for now.
- **The prompts have mostly been tuned with DeepSeek models.** Other models
  should work, but expect rough edges.
- **0.x releases can change the session format.** Sessions saved in an older
  format are not listed and cannot be resumed; see the
  [changelog](CHANGELOG.md).

## Acknowledgements

smol owes its core idea, and half its name, to Hugging Face's
[smolagents](https://github.com/huggingface/smolagents), whose `CodeAgent`
showed how much better models do when they act by writing code instead of
filling in JSON tool calls. That approach goes back to the CodeAct paper,
[Executable Code Actions Elicit Better LLM Agents](https://arxiv.org/abs/2402.01030).

smolagents is a Python library for building agents. smol is a ready-made agent
for the terminal and any ACP client, shipped as a single binary with its own
embedded Python. It is an independent project, not affiliated with Hugging Face.

## License

[MIT](LICENSE). Release archives also include `THIRD-PARTY-LICENSES.html` with
the licenses of the libraries built into the programs.
