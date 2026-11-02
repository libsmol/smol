# Changelog

Notable changes to smol. Versions follow [semantic versioning](https://semver.org);
while the version is 0.x, a minor release may break things.

Sessions are saved with a format number. A release that changes the format says so
here; sessions saved in an older format are no longer listed and can't be resumed,
but their files stay in `~/.smol/history` untouched.

## 0.6.4 - 2026-11-02

- smol shows an image that an answer puts on a line of its own, from a path or
  a web address, in the conversation: through Kitty's graphics protocol in
  Ghostty and kitty, and as colored half blocks in other terminals and inside
  tmux, screen or Zellij. Its alt text shows behind a spinner while it loads, and alone as
  before when it can't be shown. Clicking it opens the image in the system's
  viewer. Images the code viewed show under their call when expanded.
- `remember()` with the same text in the same scope, project or global,
  returns the id of the memory already saved instead of saving another. A
  batch of memories that failed halfway and ran again used to save its first
  ones twice.
- The system prompt asks to read large or many files in parts or to search
  them with `grep()`, next to printing only what is needed.
- The system prompt asks to put temporary files in `$TMPDIR`, which
  `tempfile` uses. Code that wrote to `/tmp` directly failed with
  `PermissionError` in about one deepseek run in ten.

## 0.6.3 - 2026-11-02

- The system prompt says that packages installed into the venv can be
  imported right away without naming `pip install`, and words the rules on
  starting agents and on memories more directly. Keeping one fact per memory
  and passing `project=False` only when the user says a fact holds in every
  project are now separate rules.
- A call that fails or doesn't run no longer starts its reply with `Error:`:
  the exception and the line that raised it, or why the call didn't run,
  follow the output after a blank line, and the system prompt no longer
  explains the marker. In a deepseek run the model recovered from errors as
  fast as with it.

## 0.6.2 - 2026-11-02

- The system prompt gives `remember()`'s 300-character limit and says that
  `recall()` returns up to 10 memories, which `help()` used to say alone, and
  it asks to search memories by the task's key words. It calls memories
  global or the project's, where it used to say for every folder or for this
  folder. The rules on when to save and forget move from Rules to a section
  of their own under Memories. Besides what the user asks for and their
  preferences and corrections, the model now saves only pitfalls and failed
  approaches in the project that no file records, instead of anything it
  worked out about the folder, which saved descriptions of code that went
  stale. A memory is global only when the user says it holds in every
  project.
- `recall()` fills what its matches leave of its 10 memories with the newest
  ones. A query in other words than a memory's, such as English words for a
  Chinese memory, used to find nothing, and the model went on as if nothing
  was saved.
- The system prompt lists `agent()` and its handle's methods with the other
  functions, gives the rules on when to start agents a section of their own
  under Sub-agents, and says that a single `print()` over 8192 characters is
  cut too.

## 0.6.1 - 2026-11-01

- The system prompt lists the optional parameters of MCP functions by name
  only and says that `help()` gives their types, which takes what tavily adds
  to the prompt from about 1800 to 1200 characters; `help()` on a server or a
  function still shows the full signatures.
- Code that replaced `sys.stdout` with its own stream on fd 1, such as
  `io.TextIOWrapper(open(sys.stdout.fileno(), "wb", 0))`, closed fd 1 once
  the next snippet put the original stream back, and the next file the code
  opened got every later print and program output, while each snippet
  waited 2 more seconds to end. The output now stays with the code.
- Output that happens to hold the bytes that mark the end of a snippet, such
  as `libsmol` printed with `cat`, no longer moves output into the next
  snippet.
- A thread the code started no longer waits forever on a call to an MCP
  function, `agent()` or a memory function that was still running when the
  snippet ended or that it made while an MCP server was being added; the call
  raises an error instead.
- An interrupt no longer kills Python and loses its variables when the code
  had already stopped but its output was still arriving.
- When Python exits while the model writes the next code, for example from a
  thread that called `os._exit`, that code is reported as not run, with what
  Python printed last, instead of as the code that ended Python.
- Editors get the code the model writes when it first appears and then at
  most every 100 ms instead of on every token. Each update carries all of the
  code, so long code used to send hundreds of megabytes and could stall every
  session.
- With `max_tokens` set in `model.toml`, compaction starts early enough to
  leave room for it, and summary requests leave room for it too. A server
  that refuses a request whose input and `max_tokens` together pass the
  context no longer fails every turn once a session grows; the larger
  `max_tokens`, the earlier compaction starts.
- On Linux the code can list `/`, the home folder and any other folder that
  holds a hidden path, where `ls`, `os.listdir` and `git status` failed with
  a permission error. Folders below that are hidden can be listed too, but
  what they hold still can't be read.
- On Linux a symbolic link in a writable folder that holds a hidden path, such
  as a link in the home folder when it is the working directory, no longer
  makes the place it leads to writable.
- `~/.config/git/credentials`, `~/.pgpass`, `~/.vault-token` and
  `~/.terraform.d/credentials.tfrc.json` are hidden from the code too.
- A client that opens a session with a relative `cwd` gets an error, as ACP
  requires an absolute one; such a session used to look for instructions,
  skills and the venv in the wrong place.
- `recall()` with a NUL in the query finds memories instead of failing with
  "unterminated string", and other control characters separate words.
- An MCP function whose tool declares no output schema returns a `str`, as
  its signature says, even when the server also sends structured content;
  it used to return a dict and drop the text, including notes on images
  that weren't kept.
- `libsmol setup` writes `model.toml` aside and renames it into place, so a
  write that fails halfway leaves the old file instead of an empty one.
- When a model error ends a turn, agents still running in the background end
  as "stopped before it answered" instead of "interrupted by the user".
- Copies of images and videos larger than their limit are no longer read,
  and the name of a missing tool the model called is cut to 64 characters,
  in the reply to the model and where it is saved and shown.
- Output and command results drop the payload of DCS, SOS, PM and APC
  sequences, such as kitty and sixel images, and an ESC right before another
  sequence no longer leaves that sequence's bytes in the text.
- An MCP server named like a function that agents `agent()` starts don't
  have, such as `agent` or `remember`, works under the same name in those
  agents, where it used to raise `NameError`.
- When output redrawn with `\r`, such as a progress bar, is so long that its
  middle is dropped, the note on the dropped characters stands where they
  were instead of in the middle of a line.
- An attached or viewed image or video is held to the limit of what it is
  rather than of its extension, so a 15 MB PNG named `.mov` is refused and a
  15 MB MP4 named `.png` is taken as a video. A file with a media extension
  that holds no image or video is held to the 1 MB limit of other files.
- Requests a client sent for a session that then fails to open, such as one
  open in another process, get answers instead of none.
- smol restores the terminal when it is killed with SIGTERM, ends a scrollbar
  drag let go over the completions, ends a frame left unfinished by an error
  before restoring the terminal, and keeps emoji and accents whole in the
  shimmer of a running agent's status.

## 0.6.0 - 2026-11-01

- `remember(text, project=True)`, `recall(query="")` and `forget(id)` keep
  memories across sessions in `~/.smol/memory.db`. A memory belongs to the
  working directory, or with `project=False` to every directory; a session's
  system prompt lists the newest of the latter when it opens and says how many
  the directory has, which `recall()` finds by whole words, Chinese ones too.
  Agents that `agent()` starts can only recall. SQLite and a Chinese
  dictionary make `libsmol` about 3 MB larger.
- A thread stuck writing to a full pipe no longer hangs the end of a snippet,
  which used to end with the worker killed and every variable lost. Pipes the
  code opened are still flushed after each snippet, on threads the snippet
  waits for only briefly.
- `help("name")` describes a function, class or module the code defined under
  a built-in's name instead of the built-in.
- `model.toml` must give `api` as an `http://` or `https://` URL, which used
  to show only when the first request failed.
- A quoted value in a `SKILL.md` front matter followed by a comment no longer
  keeps its quotes.
- Code that contains ```` ``` ```` shows as one block in smol and in editors.
- While MCP servers connect, smol no longer shows a new turn as thinking.
- An agent that `agent()` starts is told its answer goes to the agent that
  gave it the task.

## 0.5.1 - 2026-11-01

- The runtime and the worker module in `~/.smol/runtime` are no longer named
  after the machine's target, so a configuration directory serves one
  operating system and architecture; machines of different kinds that share a
  home folder each need their own `SMOL_CONFIG_DIR`. The runtime is downloaded
  again and each working directory gets a new venv, so packages installed with
  0.5.0 have to be installed again; the 0.5.0 runtime, module and venvs are
  removed a week after their last use.

## 0.5.0 - 2026-11-01

- The code runs in CPython 3.14 instead of RustPython. It is the same pinned
  build of [python-build-standalone](https://github.com/astral-sh/python-build-standalone)
  on every machine, which `libsmol setup`, or else the first session before
  it opens, downloads into `~/.smol/runtime` together with the worker module
  of the `libsmol` version, and a session can't open until that has worked
  once. Runtimes exist for Linux (x86_64 and arm64 with glibc), macOS (Apple
  silicon and Intel) and Windows (x86_64); `libsmol` built for anything else
  can't open a session. Plain Python runs several times faster, and `libsmol`
  is 7 MB instead of 33 MB.
- Each working directory has a venv in `~/.smol/runtime/venvs`, shared by its
  sessions and the agents they start, which the code can write. `pip install` puts
  packages there, C extensions such as numpy and pandas included, and the
  session can import them right away, editable installs too; `sys.executable`
  and the `python` and `pip` on `PATH` are the venv's, and in the sandbox pip
  keeps its cache in the session's temporary folder. A venv goes once its
  directory is gone and no session has used it for a week, and another
  version's runtime or module once no session has used it for a week.
- Functions and classes the code defines can't be sent to other processes,
  which can't import them, so `multiprocessing` and `ProcessPoolExecutor`
  raise an error that says to put them in a `.py` file.
- On Linux and macOS, code that waits on a lock or a queue can now be
  interrupted. A syntax error points at an unclosed bracket instead of the end
  of the line.
- The system prompt says which Python it is, that its venv is kept for the
  folder and can be written, and that installed packages import at once, and
  no longer tells the model to install a skill's packages in a venv in
  `$TMPDIR`.
- On macOS, code could read a hidden path with a folder between it and a place
  the code can write, such as `~/.config/gh` in a session opened in `~` or a
  deeper path in `[deny] read`, by renaming that folder, since Seatbelt rules
  go by path. The folders above a hidden path can no longer be renamed or
  removed.
- On macOS Python starts through `/usr/bin/sandbox-exec` again, so its
  Seatbelt profile holds before Python starts, as Landlock does on Linux,
  rather than from when the worker module loads.

## 0.4.4 - 2026-10-31

- The system prompt says that a task stays open after the answer only while an
  agent started with a name is still running. It said the task goes on after
  the answer, which read as if there were more to do.
- The system prompt names the places `[allow] write` in `agent.toml` opens to
  the code. It said that writing anywhere but the working directory and
  `$TMPDIR` fails, which was not true of them.
- The prompt for compaction asks for where the work stands when the summarized
  part ends. A summary that follows an earlier one rewrites it instead of
  merging the two, what the agent said it did is kept apart from what the
  output showed, each check names what it ran against, long content in a file
  is given by its path, and earlier tasks stay only where they still matter.
  The summary is written in the language of the tasks, and the model is told
  it covers its own work so far. In sessions that compacted often, the model
  read the same files again after every summary instead of going on.
- The system prompt says the answer goes to the user and that no hidden tests
  or graders check the work. Asked to make a project's tests pass, deepseek
  spent most steps guessing what hidden tests might check, building parts of
  the spec no test needed and often working for hundreds of steps.
- `sys.executable` in the code is the `python3` (or `python`) on `PATH`. It
  named libsmol, so code that ran a script or `-m unittest` with it got
  libsmol's usage message.

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
