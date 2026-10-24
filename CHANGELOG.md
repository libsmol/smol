# Changelog

Notable changes to smol. Versions follow [semantic versioning](https://semver.org);
while the version is 0.x, a minor release may break things.

Sessions are saved with a format number. A release that changes the format says so
here; sessions saved in an older format are no longer listed and can't be resumed,
but their files stay in `~/.smol/history` untouched.

## Unreleased

- When `model.toml` is missing, `smol` runs `libsmol setup`, which asks for an
  OpenAI-compatible endpoint, its key and a model picked from the endpoint's list,
  with the context window filled in when the list reports it, and writes the file.
  Other ACP clients that support terminal authentication offer it too.

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
