import builtins as _builtins
import inspect as _inspect
import io as _io
import os as _os
import re as _re
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
    return _LineStream(buffered, encoding="utf-8", errors="backslashreplace")

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

def _bound(signature, header, args, kwargs):
    try:
        return signature.bind(*args, **kwargs)
    except TypeError as exc:
        raise TypeError(f"{header}: {exc}") from None

def _builtin(function, header=None):
    signature = _inspect.signature(function)
    header = header or f"{function.__name__}{signature}"

    def builtin(*args, **kwargs):
        bound = _bound(signature, header, args, kwargs)
        return function(*bound.args, **bound.kwargs)

    builtin.__name__ = builtin.__qualname__ = function.__name__
    builtin.__signature__ = signature
    builtin.__doc__ = function.__doc__
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
    """replaces old with new; old must appear exactly once

    Replaces old with new in a text file; raises unless old appears exactly once."""
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
    """sorted paths relative to path (src/a.py is listed as 'a.py'), directories end with /; recursive=True skips .gitignore'd paths

    Lists directory path as sorted paths relative to it, directories ending with /; recursive=True lists everything below it, skipping .git and .gitignore'd paths and not entering symbolic links."""
    path = "." if path is None else _path("ls", "path", path)
    recursive = bool(recursive)
    return _ls(path, recursive)

@_builtin
def grep(pattern: str, path: str | _os.PathLike = ".", glob: str | None = None) -> list[tuple[str, int, str]]:
    """Rust regex, no lookaround or backreferences; skips .gitignore'd and binary files

    Searches a file, or every file below a directory except .git, symbolic links, .gitignore'd and binary files, for a Rust regex (Python re without lookaround or backreferences; (?i) ignores case); returns the matching lines, paths usable with open() and lines counted from 1.

    Args:
        glob: Only search files whose names match this glob
    """
    pattern = _str("grep", "pattern", pattern)
    path = "." if path is None else _path("grep", "path", path)
    glob = None if glob is None else _str("grep", "glob", glob)
    return _grep(pattern, path, glob)

@_builtin
def fetch(url: str) -> str:
    """the page as Markdown

    Returns a web page: HTML as Markdown, other text as it is; binary content raises an error."""
    url = _str("fetch", "url", url)
    return _fetch(url)

@_builtin
def view(path: str | _os.PathLike) -> None:
    """attaches an image or video file to this call's result

    Attaches a PNG, JPEG, GIF or WebP image, or an MP4, WebM or MOV video, to this call's result."""
    path = _path("view", "path", path)
    _view(path)

@_builtin
def help(request: object) -> None:
    """prints the signature and docs of a function or module

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
_FUNCTIONS = []
for _function in _BUILTINS:
    _name = _function.__name__
    _header = _function._smol_header
    _summary = _inspect.getdoc(_function).split("\n\n")[0]
    _FUNCTIONS.append((_name, _header, _summary))
    vars(_builtins)[_name] = _function
