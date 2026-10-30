# Changelog

Notable changes to smol. Versions follow [semantic versioning](https://semver.org);
while the version is 0.x, a minor release may break things.

Sessions are saved with a format number. A release that changes the format says so
here; sessions saved in an older format are no longer listed and can't be resumed,
but their files stay in `~/.smol/history` untouched.

## Unreleased

- The system prompt says that a task stays open after the answer only while an
  agent started with a name is still running. It said the task goes on after
  the answer, which read as if there were more to do.
- The system prompt names the places `[allow] write` in `agent.toml` opens to
  the code. It said that writing anywhere but the working directory and
  `$TMPDIR` fails, which was not true of them.

## 0.4.3 - 2026-10-30

- The system prompt says an agent `agent()` starts has the same environment as
  the one that starts it, and that a task asking for agents has each of its
  parts done by an agent once the parts are clear. In a session that asked for
  agents, the model drew every part itself first.
- Code could get libsmol, which runs outside the sandbox, to read a hidden file
  and send it to the model, by writing a fake `view()` notice to its own
  connection. libsmol now takes only a file name in the copies folder from that
  notice and reads copies only as plain files, never through a link.
- Code in the sandbox can connect only to Unix sockets in the places it can
  write, so no longer to ssh-agent, gpg-agent, Docker or D-Bus, and can't
  signal programs outside the sandbox. On Linux this needs kernel 7.1 for
  sockets with a path and 6.12 for signals and abstract sockets; older kernels
  keep the rest of the sandbox and the log says what stays open.

## 0.4.2 - 2026-10-29

- On macOS the Python process applies its Seatbelt profile itself with
  `sandbox_init` when it starts, instead of being started through
  `/usr/bin/sandbox-exec`. The profile is the same. If applying it fails, Python
  doesn't start and the session says why, rather than running unconfined.

## 0.4.1 - 2026-10-29

- The system prompt is laid out as How to reply, Running code, Functions,
  Sub-agents, Environment and Rules, with code and literal text in backticks.
  Functions are listed as Python signatures with their descriptions as comments,
  and `help()` shows the same descriptions. Sub-agents has a table of when to
  call `agent()` and says that a single task that fails raises `RuntimeError`.
- The prompt for compaction explains every label in the conversation it
  summarizes, not just four of them.

## 0.4.0 - 2026-10-29

Sessions are saved in format 8, so sessions from 0.3.1 and earlier are no longer
listed.

- `agent(task)` gives a task to a new agent, with its own Python session and the
  same files, functions, skills, instructions and MCP servers, and returns its
  answer. A list of tasks runs up to 8 agents at a time and returns the answers in
  order, with a `RuntimeError` in place of a task that failed. Each agent shows as
  its own call, its tokens are counted apart from the session's, and everything it
  did is saved with the session. Agents it starts can't start agents themselves.
- smol groups those agents under the call that started them. One at work shows
  the last lines of what it writes or prints and what it is doing, in a color of
  its own; one that ended takes a single line, and ctrl+o shows the start of its
  answer.
- Roles in `~/.smol/roles.toml`, and in `.smol/roles.toml` down to the working
  directory, each give `agent(task, role="name")` an agent with a description
  and optionally a model of its own; a list of tasks takes one role or a list
  with one per task. The system prompt lists them, and smol shows an agent's
  role in front of its task.
- `agent(task, name="...")` starts the agent in the background and returns an
  `Agent` handle at once, so the model can answer while it works; a list of
  tasks takes one name per task. `Agent.get(name)` finds it again, also after
  Python restarts, and the handle's `wait(timeout)`, `status()` and `stop()`
  return its answer, say what it is doing and stop it. The turn stays open until
  every background agent has ended. When one ends before code took its answer,
  the model gets an "Agent done:" message, and what you type meanwhile reaches
  it as a task update. While the rows of a call's running agents are scrolled
  out of view, smol pins them below the conversation.
- Only the latest 8 to 15 images and videos go with each request, and older
  ones are replaced by a note. A session that had viewed 47 images failed with
  HTTP 413 because the request grew too large.
- The system prompt is arranged in short sections and lists. A task update now
  adds to or changes the current task, and the model is told that text in a
  reply that runs code is shown to you. `view()` is described by the media the
  current model takes.
- For ACP clients: agents arrive as tool calls marked `_smol/agent`, with the
  call that started them in `parent` and their task, role and name in
  `rawInput`. A client that declared `_smol/queued` sends `_smol/enqueued` when
  it queues a message during a turn, which reaches a turn waiting on background
  agents.

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
