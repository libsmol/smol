import builtins as _builtins
import inspect as _inspect
import io as _io
import os as _os
import re as _re
import shutil as _shutil
# Importing it installs the SIGINT handler the agent relies on to interrupt code
import signal as _signal
import sys as _sys
import types as _types
import weakref as _weakref

_FILENAME = "<smolcode>"
_BOOTSTRAP = (lambda: None).__code__.co_filename
# pickle finds functions and classes defined in the code by module name
_MAIN = _types.ModuleType("__main__")
_sys.modules["__main__"] = _MAIN
_STATE = vars(_MAIN)

_REAL_OPEN = _io.open
_REAL_PRINT = print
# Flushed after each snippet, which lets other programs read files left open
_OPEN_FILES = _weakref.WeakSet()

# RustPython's own line buffering drops errors from its flush, an interrupt among them
class _LineStream(_io.TextIOWrapper):
    def write(self, text):
        written = super().write(text)
        if "\n" in text or "\r" in text:
            self.flush()
        return written

def _stream(fd):
    buffered = _REAL_OPEN(fd, "wb", closefd=False)
    # Windows would write \r\n, and every \r would count toward cut output
    return _LineStream(buffered, encoding="utf-8", errors="backslashreplace", newline="\n")

_STDOUT, _STDERR = _stream(1), _stream(2)
# Only prints to this stream are capped, not ones captured by redirect_stdout
_SHOWN = None

_MAX_PRINT_CHARS = 8192

def _open(file, mode="r", buffering=-1, encoding=None, errors=None, newline=None, closefd=True, opener=None):
    opened = _REAL_OPEN(file, mode, buffering, encoding, errors, newline, closefd, opener)
    _OPEN_FILES.add(opened)
    return opened

def _print(*args, sep=" ", end="\n", file=None, flush=False, limit=_MAX_PRINT_CHARS):
    target = _sys.stdout if file is None else file
    if target is not _SHOWN or limit is None:
        return _REAL_PRINT(*args, sep=sep, end=end, file=file, flush=flush)

    separator = " " if sep is None else sep
    texts = [str(arg) for arg in args]
    text = separator.join(texts)
    if len(text) > limit:
        cut = len(text) - limit
        marker = f"\n[{cut} characters cut; print(..., limit=None) skips this cut]\n"
        head = text[: limit - limit // 2]
        tail = text[len(text) - limit // 2 :]
        text = head + marker + tail
    _REAL_PRINT(text, end=end, file=file, flush=flush)

_open.__name__ = _open.__qualname__ = "open"
_open.__doc__ = _REAL_OPEN.__doc__
_print.__name__ = _print.__qualname__ = "print"

_builtins.open = _io.open = _open
_builtins.print = _print
_sys.stdout, _sys.stderr = _STDOUT, _STDERR
_sys.__stdout__, _sys.__stderr__ = _STDOUT, _STDERR
# RustPython names libsmol here, which cannot run Python scripts
_sys.executable = _sys._base_executable = _shutil.which("python3") or _shutil.which("python") or ""

def _bound(signature, header, args, kwargs):
    try:
        return signature.bind(*args, **kwargs)
    except TypeError as exc:
        raise TypeError(f"{header}: {exc}") from None

def _builtin(function, header=None):
    signature = _inspect.signature(function)
    if header is None:
        shown = signature
        # Methods are listed as Class.method without self or cls
        if "." in function.__qualname__:
            parameters = list(signature.parameters.values())[1:]
            shown = signature.replace(parameters=parameters)
        header = f"{function.__qualname__}{shown}"

    def builtin(*args, **kwargs):
        bound = _bound(signature, header, args, kwargs)
        return function(*bound.args, **bound.kwargs)

    builtin.__name__ = function.__name__
    builtin.__qualname__ = function.__qualname__
    builtin.__signature__ = signature
    # The agent sets these values too, so docs name them in braces
    builtin.__doc__ = function.__doc__.format(agents=_MAX_AGENTS, done=_DONE)
    builtin._smol_header = header
    return builtin

def _str(function, name, value):
    if isinstance(value, str):
        return value
    kind = type(value).__name__
    raise TypeError(f"{function}: {name} must be str, not {kind}")

def _path(function, name, value):
    if isinstance(value, (str, _os.PathLike)):
        return _os.fsdecode(value)
    kind = type(value).__name__
    raise TypeError(f"{function}: {name} must be str or a path, not {kind}")

@_builtin
def edit(path: str | _os.PathLike, old: str, new: str) -> None:
    """Replaces old with new in a text file.
    Raises ValueError and leaves the file unchanged unless old appears exactly once."""
    path = _path("edit", "path", path)
    old = _str("edit", "old", old)
    new = _str("edit", "new", new)
    if not old:
        raise ValueError(f"edit: {path}: old text is empty")

    with _REAL_OPEN(path, "rb") as file:
        data = file.read()
    try:
        text = data.decode()
    except UnicodeDecodeError:
        raise ValueError(f"edit: {path}: not UTF-8 text") from None

    # open() reads every newline as \n but each one in the file may be \r\n
    parts = [old] if "\r" in old else old.split("\n")
    escaped = [_re.escape(part) for part in parts]
    # A leading newline must not match a second time inside \r\n
    joined = "(?:\r\n|(?<!\r)\n)".join(escaped)
    pattern = _re.compile(joined)

    # Overlapping matches count too
    count, first = 0, None
    found = pattern.search(text)
    while found:
        count += 1
        first = first or found
        after = found.start() + 1
        found = pattern.search(text, after)
    if count == 0:
        raise ValueError(f"edit: {path}: old text not found")
    if count > 1:
        raise ValueError(f"edit: {path}: old text found {count} times; include more surrounding text so it appears once")

    # The new text takes the replaced text's line endings, or the whole file's when it has none
    replaced = first.group()
    if "\n" in replaced:
        crlf = "\r\n" in replaced
    else:
        crlf = "\n" in text and text.count("\r\n") == text.count("\n")
    if crlf:
        new = new.replace("\r\n", "\n").replace("\n", "\r\n")
    # Encoding must fail before the file is truncated
    edited = (text[:first.start()] + new + text[first.end():]).encode()
    with _REAL_OPEN(path, "wb") as file:
        file.write(edited)

@_builtin
def ls(path: str | _os.PathLike = ".", recursive: bool = False) -> list[str]:
    """Sorted paths relative to path: ls('src') returns 'a.py', not 'src/a.py'.
    Directories end with /. recursive=True skips .gitignore'd paths.

    Lists directory path as sorted paths relative to it, directories ending with /; recursive=True lists everything below it, skipping .git and .gitignore'd paths and not entering symbolic links."""
    path = "." if path is None else _path("ls", "path", path)
    recursive = bool(recursive)
    return _ls(path, recursive)

@_builtin
def grep(pattern: str, path: str | _os.PathLike = ".", glob: str | None = None) -> list[tuple[str, int, str]]:
    """(path, line number from 1, line) for each matching line.
    pattern is a Rust regex without lookaround or backreferences.
    Skips .gitignore'd and binary files.

    Searches a file, or every file below a directory, for a Rust regex.
    - Its syntax is that of Python re, without lookaround or backreferences; (?i) ignores case.
    - Below a directory it skips .git, symbolic links, .gitignore'd and binary files.
    - Returns the matching lines; paths work with open() and lines are counted from 1.

    Args:
        glob: Only search files whose names match this glob
    """
    pattern = _str("grep", "pattern", pattern)
    path = "." if path is None else _path("grep", "path", path)
    glob = None if glob is None else _str("grep", "glob", glob)
    return _grep(pattern, path, glob)

@_builtin
def fetch(url: str) -> str:
    """The page as Markdown.

    Returns a web page: HTML as Markdown, other text as it is; binary content raises an error."""
    url = _str("fetch", "url", url)
    return _fetch(url)

@_builtin
def view(path: str | _os.PathLike) -> None:
    """Attaches an image or video file to this call's result.

    Attaches a PNG, JPEG, GIF or WebP image, or an MP4, WebM or MOV video, to this call's result."""
    path = _path("view", "path", path)
    # The code may have changed directory but the label is relative to the session directory
    absolute = _os.path.abspath(path)
    _view(path, absolute)

def _delegated(function, method, params):
    try:
        return _agent(method, params)
    except (TypeError, ValueError) as exc:
        raise type(exc)(f"{function}: {exc}") from None

def _handed(function, result):
    error = result.get("error")
    if error is None:
        return result["answer"]
    return RuntimeError(f"{function}: {error}")

def _texts(function, name, value, count=None):
    texts = [value] if isinstance(value, str) else value
    if not isinstance(texts, list):
        kind = type(value).__name__
        raise TypeError(f"{function}: {name} must be str or a list of str, not {kind}")
    for item in texts:
        if not isinstance(item, str):
            kind = type(item).__name__
            raise TypeError(f"{function}: {name} must be str or a list of str, not a list holding {kind}")
    if count is not None and len(texts) != count:
        raise ValueError(f"{function}: {name}: needs one {name} per task ({count}), not {len(texts)}")
    return texts

_FIELDS = ".name, .task and .role" if _AGENTS == "roles" else ".name and .task"

class Agent:
    __doc__ = f"The handle of an agent started with a name.\n{_FIELDS} hold what it was started with."

    _smol_header = "class Agent"

    def __init__(self, id, name, task, role):
        self._id = id
        self.name = name
        self.task = task
        self.role = role

    def __repr__(self):
        return f"<Agent {self.name}>"

    def get(cls, name: str) -> Agent | None:
        """The handle of the agent last started with this name, also after Python restarts.

        Returns the handle of the agent last started with this name in this session, also after Python restarts, or None if there is none."""
        name = _str("Agent.get", "name", name)
        found = _delegated("Agent.get", "find", {"name": name})
        if found is None:
            return None
        return cls(**found)

    def wait(self, timeout: float | None = None) -> str | None:
        """Its answer once it ends, or None if it is still running after timeout seconds.
        Raises RuntimeError if it failed or was stopped.

        Waits until the agent ends and returns its answer.
        - With timeout, returns None if it is still running after that many seconds; timeout=None waits until it ends.
        - Raises RuntimeError if the agent failed or was stopped.
        - Once the code has its result, no "{done}:" message is sent for it."""
        if timeout is not None:
            if isinstance(timeout, bool) or not isinstance(timeout, (int, float)):
                kind = type(timeout).__name__
                raise TypeError(f"Agent.wait: timeout must be a number or None, not {kind}")
            if not timeout >= 0:
                raise ValueError(f"Agent.wait: timeout must be 0 or more, not {timeout}")
            timeout = float(timeout)
        result = _delegated("Agent.wait", "wait", {"id": self._id, "timeout": timeout})
        if result is None:
            return None
        answer = _handed("Agent.wait", result)
        if isinstance(answer, RuntimeError):
            raise answer
        return answer

    def status(self) -> str:
        """What it is doing now and the end of what it is writing or printing, or how it ended.

        Returns how long it has run and what it is doing now, followed by the last lines it is writing or printing; once it has ended, returns how it ended."""
        return _delegated("Agent.status", "status", {"id": self._id})

    def stop(self) -> None:
        """Stops it and returns once it has stopped.

        Stops the agent and returns once it has stopped; no "{done}:" message is sent for it."""
        _delegated("Agent.stop", "stop", {"id": self._id})

# Wrapped once the class exists, which their annotations name
for _method in [Agent.get, Agent.wait, Agent.status, Agent.stop]:
    setattr(Agent, _method.__name__, _builtin(_method))
Agent.get = classmethod(Agent.get)

def agent(task: str | list[str], role: str | list[str] | None = None, *, name: str | list[str] | None = None) -> str | list[str | RuntimeError] | Agent | list[Agent]:
    """A new agent does each task and returns its answer, or with name its handle at once.
    A list of tasks takes a list of names, one per task.

    Gives each task to a new agent.
    - Without name: returns its answer, or raises RuntimeError if the agent fails. A list of tasks runs up to {agents} agents at a time and returns their answers in order, with the exception in place of each task that failed.
    - With name: the agent runs in the background and agent() returns its Agent handle at once. A list of tasks takes one name per task and returns a list of handles. A name cannot be given again while its agent is running.

    The agent has its own Python session and the same files, built-in functions, MCP servers, skills and instructions as you, but not your variables or this conversation, so the task must state everything it needs. Agents started this way cannot call agent()."""
    single = isinstance(task, str)
    tasks = _texts("agent", "task", task)
    if not tasks or not all(item.strip() for item in tasks):
        raise ValueError("agent: task is empty")
    roles = [role] * len(tasks) if role is None or isinstance(role, str) else _texts("agent", "role", role, len(tasks))

    if name is None:
        asked = [{"task": task, "role": role, "name": None} for task, role in zip(tasks, roles)]
        results = _delegated("agent", "ask", {"agents": asked})
        answers = [_handed("agent", result) for result in results]
        if not single:
            return answers
        if isinstance(answers[0], RuntimeError):
            raise answers[0]
        return answers[0]

    names = _texts("agent", "name", name, len(tasks))
    if not all(item.strip() for item in names):
        raise ValueError("agent: name is empty")
    for at, item in enumerate(names):
        if item in names[:at]:
            raise ValueError(f"agent: name: {item}: given twice")
    asked = [{"task": task, "role": role, "name": name} for task, role, name in zip(tasks, roles, names)]
    handles = _delegated("agent", "start", {"agents": asked})
    handles = [Agent(**handle) for handle in handles]
    return handles[0] if single else handles

# Without roles the parameter is left out, so passing one fails like any unknown argument
if _AGENTS != "roles":
    _signature = _inspect.signature(agent)
    _parameters = [_signature.parameters["task"], _signature.parameters["name"]]
    agent.__signature__ = _signature.replace(parameters=_parameters)
agent = _builtin(agent)

@_builtin
def help(request: object) -> None:
    """Prints the signature and docs of a function, class, module or MCP server.
    For functions and classes you defined, also prints their code.

    Prints the signature and documentation of an object, or of the function, module or MCP server a dotted name refers to; modules also list their public members, and functions and classes your code defined in an earlier call also show that code."""
    import importlib

    target = request
    name = request if isinstance(request, str) else None
    if name is not None:
        builtins = vars(_builtins)
        head, *rest = name.split(".")
        try:
            if head in builtins:
                target = builtins[head]
            elif head in _STATE:
                target = _STATE[head]
            else:
                target = importlib.import_module(head)
            for i, part in enumerate(rest):
                try:
                    target = getattr(target, part)
                except AttributeError:
                    dotted = ".".join([head, *rest[: i + 1]])
                    target = importlib.import_module(dotted)
        except Exception:
            print(f"help: {name}: not found")
            return

    if isinstance(target, _Server):
        listing = repr(target)
        print(listing, limit=None)
        return

    module = isinstance(target, _types.ModuleType)
    if name is None and module:
        name = target.__name__
    if name is None:
        owner = getattr(target, "__module__", None)
        qualname = getattr(target, "__qualname__", None) or type(target).__qualname__
        name = qualname if owner in (None, "builtins", "__main__") else f"{owner}.{qualname}"

    header = getattr(target, "_smol_header", None)
    if header is None and module:
        header = f"module {name}"
    if header is None:
        try:
            signature = _inspect.signature(target)
            header = f"{name}{signature}"
        except (TypeError, ValueError):
            header = name

    parts = [header]
    if module:
        public = [member for member in dir(target) if not member.startswith("_")]
        members = ", ".join(public)
        parts.append(f"Members: {members}")
    # Only the object's own attribute counts because instances would inherit it from the class
    source = getattr(target, "__dict__", {}).get("_smol_source")
    doc = source or _inspect.getdoc(target)
    if doc:
        parts.append(doc)

    text = "\n".join(parts)
    print(text, limit=None)

_BUILTINS = [edit, ls, grep, fetch, view, help]
# Workers of agents that agent() started have none of it
if _AGENTS:
    _BUILTINS[5:5] = [agent, Agent, Agent.get, Agent.wait, Agent.status, Agent.stop]
_FUNCTIONS = []
for _function in _BUILTINS:
    _name = _function.__qualname__
    _header = _function._smol_header
    _summary = _inspect.getdoc(_function).split("\n\n")[0]
    _FUNCTIONS.append((_name, _header, _summary))
    # Methods come with their class
    if "." not in _name:
        vars(_builtins)[_name] = _function
