# Changelog

Notable changes to smol. Versions follow [semantic versioning](https://semver.org);
while the version is 0.x, a minor release may break things.

Sessions are saved with a format number. A release that changes the format says so
here; sessions saved in an older format are no longer listed and can't be resumed,
but their files stay in `~/.smol/history` untouched.

## 0.3.1 - 2026-10-27

- Packages a skill's scripts need now go in a venv in the session's temporary
  folder. The system prompt used to say to make it in the skill's folder, which the
  sandbox keeps read-only outside the working directory, so creating it failed.
- The system prompt is shorter: the rules about fixing failed calls and assuming
  UTF-8 and the list repeating what the built-in functions do are gone.

## 0.3.0 - 2026-10-26

Sessions are saved in format 7, so sessions from 0.2.0 and earlier are no longer
listed.

- `:goal` is gone, along with its checker, its status and `:goal resume` and
  `:goal clear`. The `_smol/goal` notification and the `_smol/clear_goal` method are
  removed, and a `_smol/goal` flag on a prompt is ignored.

## 0.2.0 - 2026-10-26

- Python and the programs it starts run in a sandbox. On Linux (with Landlock) and
  macOS (with `sandbox-exec`) they can write only in the working directory and a
  temporary folder of their own, and can't read common credential locations such as
  `~/.ssh` or the smol configuration. On every platform they get only a few of your
  environment variables. `[allow]` and `[deny]` in `agent.toml` widen or narrow it;
  see Sandbox in the README.

## 0.1.1 - 2026-10-24

- When `model.toml` is missing, `smol` runs `libsmol setup`, which asks for an
  OpenAI-compatible endpoint, its key and a model picked from the endpoint's list,
  with the context window filled in when the list reports it, and writes the file.
  Other ACP clients that support terminal authentication offer it too.
- `smol --version` and `libsmol --version`.
- Each code call is titled with its code, so Zed shows the Python under "Run Command"
  instead of `eval`.
- Code added in Zed with "Add to Agent Thread" reaches the model as selected, with its
  file and lines marked, instead of as the whole file. libsmol now takes context
  embedded in prompts, so Zed's other `@` mentions work too, and a linked file with a
  line range is attached as just those lines.
- A session that can't open because of `model.toml` or `agent.toml` is now logged
  like other failures to open.

## 0.1.0 - 2026-10-24

The first public release: the `smol` terminal UI and the `libsmol` ACP agent.
Sessions are saved in format 6.

- The model works through a single `eval` tool that runs Python (RustPython, built
  in) in a session that keeps its variables, functions and imports, with `ls`,
  `grep`, `edit`, `fetch`, `view` and `help` ready to use.
- Interrupting stops code in place and keeps the session's variables. Code that
  can't be interrupted gets 3 seconds before Python restarts, and the model is told.
- Any OpenAI-compatible chat completions endpoint, configured in `model.toml`, with
  an output limit, reasoning effort, image and video input, and compaction of older
  steps before the context window fills.
- MCP servers over stdio and streamable HTTP, configured in `agent.toml`, show up
  as Python objects and can be turned on and off with `:mcp`.
- `:goal` keeps working until a checker agrees the goal is met, or stops with what
  only the user can provide.
- `!` runs a shell command in the session directory, text typed during a turn
  reaches the model as a task update, and sessions can be resumed.
- `AGENTS.md` (or `CLAUDE.md`) instructions and skills are read from the
  configuration directory and from each directory down to the working directory.
- `libsmol` speaks standard ACP, so Zed and other ACP clients can drive it.
- A daily log in `~/.smol/logs`, filtered with `SMOL_LOG`.
- Release archives for Linux, macOS and Windows, with install scripts that
  download and verify them.
- Packages on crates.io: `libsmol` (the agent), `libsmol-tui` (the `smol`
  terminal UI) and `libsmol-reaper` (the process groups both use).
