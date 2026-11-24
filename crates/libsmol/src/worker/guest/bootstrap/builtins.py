import builtins as _builtins
import inspect as _inspect
import io as _io
import os as _os
import pickle as _pickle
import re as _re
import stat as _stat
import sys as _sys
import types as _types
import weakref as _weakref
from multiprocessing.reduction import ForkingPickler as _ForkingPickler

# pickle finds functions and classes defined in the code by module name
_MAIN = _types.ModuleType("__main__")
_sys.modules["__main__"] = _MAIN
_STATE = vars(_MAIN)

def _unsent(self, obj):
    if isinstance(obj, (_types.FunctionType, type)) and getattr(obj, "__module__", None) == "__main__":
        raise _pickle.PicklingError(f"multiprocessing: {obj.__qualname__}: defined in your code, so it can't be sent to another process; put it in a .py file and import it")
    return NotImplemented

# Spawned processes can't import a __main__ without a file, and a Pool waits forever on one that fails to load it
_ForkingPickler.reducer_override = _unsent

_REAL_OPEN = _io.open
_REAL_PRINT = print
# Flushed after each snippet, which lets other programs read files left open
_OPEN_FILES = _weakref.WeakSet()
# Written pipes and devices, whose flush can wait for good on a writer blocked on them
_PIPES = _weakref.WeakSet()

def _stream(fd):
    buffered = _REAL_OPEN(fd, "wb", closefd=False)
    # Windows would write \r\n, and every \r would count toward cut output
    return _io.TextIOWrapper(buffered, encoding="utf-8", errors="backslashreplace", newline="\n", line_buffering=True)

_STDOUT, _STDERR = _stream(1), _stream(2)
# Only prints to this stream are capped, not ones captured by redirect_stdout
_SHOWN = None

def _open(file, mode="r", buffering=-1, encoding=None, errors=None, newline=None, closefd=True, opener=None):
    opened = _REAL_OPEN(file, mode, buffering, encoding, errors, newline, closefd, opener)
    found = _os.fstat(opened.fileno())
    if _stat.S_ISREG(found.st_mode):
        _OPEN_FILES.add(opened)
    elif opened.writable():
        _PIPES.add(opened)
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
        marker = "\n" + _PRINT_CUT.format(cut=cut) + "\n"
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

def _bound(signature, header, args, kwargs):
    try:
        return signature.bind(*args, **kwargs)
    except TypeError as exc:
        raise TypeError(f"{header}: {exc}") from None

def _builtin(function):
    signature = _inspect.signature(function)
    header = f"{function.__qualname__}{signature}"

    def builtin(*args, **kwargs):
        bound = _bound(signature, header, args, kwargs)
        return function(*bound.args, **bound.kwargs)

    builtin.__name__ = function.__name__
    builtin.__qualname__ = function.__qualname__
    builtin.__signature__ = signature
    # The agent sets these values too, so docs name them in braces
    builtin.__doc__ = function.__doc__.format(chars=_MAX_MEMORY_CHARS, recalled=_MAX_RECALLED)
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
    """Replaces old with new in a text file; create new files with open().
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

class _Found(list):
    """The matching lines grep() returns, which print as ripgrep shows them"""

    def __init__(self, matched, around, context):
        super().__init__(matched)
        # {path: {line number: line}} of the context lines, which the list doesn't hold
        self._around = around
        self._context = context

    def __getitem__(self, index):
        item = super().__getitem__(index)
        if isinstance(index, slice):
            return _Found(item, self._around, self._context)
        return item

    # Another process gets a plain list, as it has no _Found to rebuild
    def __reduce__(self):
        return (list, (list(self),))

    def __repr__(self):
        # The code can put other items in the list
        if not all(isinstance(item, tuple) and len(item) == 3 for item in self):
            return super().__repr__()

        files = {}
        for path, number, line in self:
            files.setdefault(path, {})[number] = line

        matches = "match" if len(self) == 1 else "matches"
        named = "file" if len(files) == 1 else "files"
        blocks = [f"{len(self)} {matches} in {len(files)} {named}"]
        for path, matched in files.items():
            shown = dict(matched)
            around = self._around.get(path, {})
            for number in matched:
                for near in range(number - self._context, number + self._context + 1):
                    if near in around:
                        shown.setdefault(near, around[near])

            rows = [path]
            previous = None
            for number in sorted(shown):
                if self._context and previous is not None and number > previous + 1:
                    rows.append("--")
                mark = ":" if number in matched else "-"
                rows.append(f"{number}{mark}{shown[number]}")
                previous = number
            blocks.append("\n".join(rows))
        return "\n\n".join(blocks)

@_builtin
def grep(pattern: str, path: str | _os.PathLike = ".", glob: str | None = None, context: int = 0) -> list[tuple[str, int, str]]:
    """(path, line number from 1, line) for each matching line, printed grouped by file.
    context=N also prints the N lines before and after each match.
    pattern is a Rust regex without lookaround or backreferences.
    Skips .gitignore'd and binary files.

    Searches a file, or every file below a directory, for a Rust regex.
    - Its syntax is that of Python re, without lookaround or backreferences; (?i) ignores case.
    - Below a directory it skips .git, symbolic links, .gitignore'd and binary files.
    - Returns the matching lines; paths work with open() and lines are counted from 1.
    - The list prints like ripgrep: the number of matches, then each file's path and its lines as number:line, with number-line for context lines.

    Args:
        glob: Only search files whose names match this glob
        context: Lines printed before and after each match, which the list doesn't hold
    """
    pattern = _str("grep", "pattern", pattern)
    path = "." if path is None else _path("grep", "path", path)
    glob = None if glob is None else _str("grep", "glob", glob)
    if isinstance(context, bool) or not isinstance(context, int):
        kind = type(context).__name__
        raise TypeError(f"grep: context must be int, not {kind}")
    if context < 0:
        raise ValueError(f"grep: context must be 0 or more, not {context}")

    matched, around = [], {}
    for name, number, line, hit in _grep(pattern, path, glob, context):
        if hit:
            matched.append((name, number, line))
        else:
            around.setdefault(name, {})[number] = line
    return _Found(matched, around, context)

@_builtin
def fetch(url: str) -> str:
    """The page as Markdown.

    Returns a web page: HTML as Markdown, other text as it is; binary content raises an error."""
    url = _str("fetch", "url", url)
    return _fetch(url)

@_builtin
def view(path: str | _os.PathLike) -> None:
    """Shows you an image or video file by adding it to this call's result.

    Adds a PNG, JPEG, GIF or WebP image, or an MP4, WebM or MOV video, to this call's result for you to see."""
    path = _path("view", "path", path)
    # The code may have changed directory but the label is relative to the session directory
    absolute = _os.path.abspath(path)
    _view(path, absolute)

def _asked(method, params):
    try:
        return _ask(method, params)
    except (TypeError, ValueError) as exc:
        raise type(exc)(f"{method}: {exc}") from None

@_builtin
def remember(text: str, project: bool = True) -> int:
    """Saves text of at most {chars} characters as a memory and returns its id.
    The memory is for this project, or global with project=False."""
    text = _str("remember", "text", text)
    project = bool(project)
    return _asked("remember", {"text": text, "project": project})

@_builtin
def recall(query: str = "") -> list[tuple[int, str]]:
    """(id, text) of up to {recalled} memories: those that match any word of query, best first, or the newest if none match."""
    query = _str("recall", "query", query)
    found = _asked("recall", {"query": query})
    return [(id, text) for id, text in found]

@_builtin
def forget(id: int) -> None:
    """Deletes the memory with this id."""
    if isinstance(id, bool) or not isinstance(id, int):
        kind = type(id).__name__
        raise TypeError(f"forget: id must be int, not {kind}")
    # The agent keeps ids in 64 bits, so a larger one names no memory
    if not -2**63 <= id < 2**63:
        raise ValueError(f"forget: {_NO_MEMORY}")
    _asked("forget", {"id": id})

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
            own = _STATE.get(head)
            documented = callable(own) or isinstance(own, _types.ModuleType)
            # The code's functions, classes and modules hide builtins of the same name, and its other values don't
            if head in _STATE and (documented or head not in builtins):
                target = own
            elif head in builtins:
                target = builtins[head]
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

_FUNCTIONS = []
for _function in [edit, ls, grep, fetch, view, remember, recall, forget, help]:
    _name = _function.__qualname__
    _header = _function._smol_header
    _summary = _inspect.getdoc(_function).split("\n\n")[0]
    _FUNCTIONS.append((_name, _header, _summary))
    vars(_builtins)[_name] = _function
