import ast as _ast
import io as _io
import itertools as _itertools
import linecache as _linecache
import re as _re
import site as _site
import sys as _sys
import textwrap as _textwrap
import threading as _threading
import time as _time
import tokenize as _tokenize
import unicodedata as _unicodedata

_SNIPPET = "<smolcode-"
_RUNS = _itertools.count(1)
_BOOTSTRAP = (lambda: None).__code__.co_filename
_MAX_LEAVES = 3
_MAX_LINKS = 2

_DEFINITIONS = (_ast.FunctionDef, _ast.AsyncFunctionDef, _ast.ClassDef)
_SCOPES = (*_DEFINITIONS, _ast.Lambda, _ast.ListComp, _ast.SetComp, _ast.DictComp, _ast.GeneratorExp)

def _reads(tree, name):
    for child in _ast.walk(tree):
        if isinstance(child, _ast.Name) and child.id == name:
            return True
    return False

# Lines starting with < and any </ are never Python but stray tags or other tool call formats
_TAG = _re.compile(r"^[ \t]*<[^\n]*>[ \t]*(?:\n|$)|</[^<>\n]*>", _re.M)
# Only leading tags count because a tag mid-code may open a repeated call that stripping would run
_LEAD = _re.compile(r"(?:\s*<[^<>\n]*>)+")
_LITERALS = {"STRING", "COMMENT", "FSTRING_MIDDLE", "TSTRING_MIDDLE"}

# Tags inside strings and comments stay, and code that still won't parse is cut before the first tag
def _untagged(code, filename):
    readline = _io.StringIO(code).readline
    try:
        generated = _tokenize.generate_tokens(readline)
        tokens = list(generated)
    except (SyntaxError, _tokenize.TokenError):
        return None

    # Token positions count lines and characters
    starts, at = [], 0
    for line in code.split("\n"):
        starts.append(at)
        at += len(line) + 1
    literals = []
    for token in tokens:
        if _tokenize.tok_name[token.type] in _LITERALS:
            (row, column), (end_row, end_column) = token.start, token.end
            begin = starts[row - 1] + column
            finish = starts[end_row - 1] + end_column
            literals.append((begin, finish))

    # Code may follow a leading tag on the same line and only the tag is removed
    tags, start = [], 0
    lead = _LEAD.match(code)
    if lead:
        tags.append(lead.span())
        start = lead.end()
    for tag in _TAG.finditer(code, start):
        offset = tag.start()
        inside = [begin <= offset < finish for begin, finish in literals]
        if not any(inside):
            tags.append(tag.span())
    if not tags:
        return None

    pieces, last = [], 0
    for begin, end in tags:
        piece = code[last:begin]
        pieces.append(piece)
        last = end
    pieces.append(code[last:])
    candidates = ["".join(pieces)]
    for end in range(1, len(pieces)):
        before = "".join(pieces[:end])
        if before.strip():
            candidates.append(before)
            break

    for candidate in candidates:
        try:
            _ast.parse(candidate, filename)
        except SyntaxError:
            continue
        if candidate.strip():
            return candidate
    return None

def _pths():
    found = set()
    for folder in _site.getsitepackages():
        try:
            names = _os.listdir(folder)
        except OSError:
            continue
        found.update((folder, name) for name in names if name.endswith(".pth"))
    return found

# The ones site processed at start
_PTHS = _pths()

_FLUSH_WAIT = 0.1
# The thread flushing each pipe, which a later snippet doesn't start again while it waits
_FLUSHERS = _weakref.WeakKeyDictionary()

def _flushed(opened):
    try:
        opened.flush()
    except Exception:
        pass

def _message(exc):
    try:
        return str(exc)
    except Exception:
        return ""

def _described(exc):
    message = _message(exc)
    kind = type(exc).__name__
    text = f"{kind}: {message}" if message else kind
    notes = getattr(exc, "__notes__", None)
    if isinstance(notes, list):
        for note in notes:
            if isinstance(note, str):
                text += f"\n{note}"
    return text

def _located(exc, filename):
    own = model = last = None
    tb = exc.__traceback__
    while tb is not None:
        name = tb.tb_frame.f_code.co_filename
        if name == filename:
            own = tb
        if name.startswith(_SNIPPET):
            model = tb
        if name != _BOOTSTRAP:
            last = tb
        tb = tb.tb_next

    lines, shown = [], []
    for tb in (own, model, last):
        # Instructions without a line number give None
        if tb is None or tb.tb_lineno is None or tb in shown:
            continue
        shown.append(tb)

        code = tb.tb_frame.f_code
        if code.co_filename == filename:
            where = f"line {tb.tb_lineno}"
        elif code.co_filename.startswith(_SNIPPET):
            where = f"an earlier call, line {tb.tb_lineno}, in {code.co_name}"
        else:
            where = f"{code.co_filename}, line {tb.tb_lineno}, in {code.co_name}"
        # The module's loader gives the source of what isn't a file, like the stdlib in its zip
        source = _linecache.getline(code.co_filename, tb.tb_lineno, tb.tb_frame.f_globals).strip()
        lines.append(f"  {where}: {source}" if source else f"  {where}")
    return lines

def _run(code):
    global _SHOWN, _STDOUT, _STDERR
    # Streams the previous snippet swapped in are not kept
    _sys.stdout, _sys.stderr = _STDOUT, _STDERR
    # Dropping one the code made on fd 1 or 2 closed that fd, which the next file opened would take
    for fd in (1, 2):
        try:
            _os.fstat(fd)
        except OSError:
            _os.dup2(_OUTPUT, fd)
    # Streams the code closed are reopened on the fds
    if _STDOUT.closed or _STDERR.closed:
        _STDOUT, _STDERR = _stream(1), _stream(2)
        _sys.__stdout__, _sys.__stderr__ = _STDOUT, _STDERR
        _sys.stdout, _sys.stderr = _STDOUT, _STDERR
    _SHOWN = _STDOUT

    # Packages installed since, like editable ones, add their paths as site would after a restart
    for folder, name in sorted(_pths() - _PTHS):
        _PTHS.add((folder, name))
        _site.addpackage(folder, name, None)

    filename = f"{_SNIPPET}{next(_RUNS)}>"
    error = repaired = None
    blocks = []
    defined = {}
    try:
        try:
            try:
                parsed = _ast.parse(code, filename)
            except SyntaxError:
                # The repaired code goes back to be saved, and line numbers and errors refer to it
                repaired = _untagged(code, filename)
                if repaired is None:
                    raise
                code = repaired
                parsed = _ast.parse(code, filename)
            for node in parsed.body:
                if isinstance(node, _ast.Expr):
                    tree, mode = _ast.Expression(node.value), "eval"
                else:
                    tree, mode = _ast.Module([node], []), "exec"
                block = compile(tree, filename, mode)
                blocks.append(block)
            # Without an mtime, checkcache keeps the entry instead of looking for a file
            cached = [line + "\n" for line in code.split("\n")]
            _linecache.cache[filename] = (len(code), None, cached, filename)
        except SyntaxError as exc:
            blocks = []
            source = exc.text
            column = max((exc.offset or 1) - 1, 0)

            # Errors found only when compiling, like return outside a function, carry no source line
            lines = code.split("\n")
            if source is None and exc.lineno and 0 < exc.lineno <= len(lines):
                source = lines[exc.lineno - 1]
                # Such an error counts its column in UTF-8 bytes
                before = source.encode("utf-8", "surrogatepass")[:column]
                column = len(before.decode("utf-8", "ignore"))

            source = (source or "").rstrip()
            padding = []
            for c in source[:column]:
                # Tabs are copied to expand like the line above
                if c == "\t":
                    padding.append(c)
                # Wide characters take two terminal columns
                elif _unicodedata.east_asian_width(c) in ("W", "F"):
                    padding.append("  ")
                else:
                    padding.append(" ")
            caret = "".join(padding) + "^"

            # Like the error for null bytes, which belongs to no line
            if exc.lineno is None:
                error = f"syntax error: {exc.msg}"
            else:
                error = f"syntax error on line {exc.lineno}: {exc.msg}\n{source}\n{caret}"

        # The last statement rebinding each global name,
        # outside functions, classes and comprehensions
        bound = {}
        pending = [(node, node) for node in parsed.body] if blocks else []
        while pending:
            node, statement = pending.pop()
            if isinstance(node, _ast.stmt):
                statement = node

            names = []
            if isinstance(node, _DEFINITIONS):
                names.append(node.name)
            elif isinstance(node, (_ast.Import, _ast.ImportFrom)):
                imported = [(alias.asname or alias.name).partition(".")[0] for alias in node.names]
                names.extend(imported)
            elif isinstance(node, _ast.NamedExpr):
                # The value is the right side of :=, not the whole statement around it
                if not _reads(node.value, node.target.id):
                    names.append(node.target.id)
                pending.append((node.value, statement))
            elif isinstance(node, _ast.Name) and isinstance(node.ctx, _ast.Store):
                # An assignment that reads the name's old value is not a rebinding
                value = getattr(statement, "value", None)
                reads = value is not None and _reads(value, node.id)
                if not reads and not isinstance(statement, _ast.AugAssign):
                    names.append(node.id)
            elif not isinstance(node, _SCOPES):
                children = [(child, statement) for child in _ast.iter_child_nodes(node)]
                pending.extend(children)

            for name in names:
                # One line may hold several assignments
                at = (node.lineno, node.col_offset)
                if name not in bound or at >= bound[name][0]:
                    bound[name] = (at, statement)

        # Record the old values, which a def that never ran must not attach its source to
        for name, (_, statement) in bound.items():
            if isinstance(statement, _DEFINITIONS):
                defined[name] = (statement, _STATE.get(name))

        for block in blocks:
            value = eval(block, _STATE)
            if value is not None:
                text = value if isinstance(value, str) else repr(value)
                print(text, limit=None)

    except BaseException as exc:
        # Source lines are left out of what links are checked against, as their numbers would match short messages
        said = _described(exc)
        parts = [said, *_located(exc, filename)]

        if isinstance(exc, BaseExceptionGroup):
            leaves, pending = [], list(reversed(exc.exceptions))
            while pending and len(leaves) < _MAX_LEAVES:
                member = pending.pop()
                if isinstance(member, BaseExceptionGroup):
                    pending.extend(reversed(member.exceptions))
                else:
                    leaves.append(member)
            for leaf in leaves:
                described = _described(leaf)
                said += f"\n{described}"
                parts.append(f"Sub-exception {described}")
                parts.extend(_located(leaf, filename))

        # A chain the code made may loop back, and skipped links don't stop the walk
        seen, link, linked = {id(exc)}, exc, 0
        while linked < _MAX_LINKS:
            if link.__cause__ is not None:
                label, link = "Caused by", link.__cause__
            elif link.__context__ is not None and not link.__suppress_context__:
                label, link = "Raised while handling", link.__context__
            else:
                break
            if id(link) in seen:
                break
            seen.add(id(link))

            message = _message(link)
            if message and f": {message}" in said:
                continue
            described = _described(link)
            said += f"\n{described}"
            parts.append(f"{label} {described}")
            parts.extend(_located(link, filename))
            linked += 1

        error = "\n".join(parts)

    finally:
        _SHOWN = None

        # A pipe flushes on a thread the end marker waits for only briefly, since a writer blocked on it holds its lock
        started = []
        for opened in list(_PIPES):
            flusher = _FLUSHERS.get(opened)
            if opened.closed or (flusher is not None and flusher.is_alive()):
                continue
            flusher = _threading.Thread(target=_flushed, args=(opened,), daemon=True)
            flusher.start()
            _FLUSHERS[opened] = flusher
            started.append(flusher)

        # Flush without closing because variables still hold them
        for opened in [*_OPEN_FILES, _sys.stdout, _sys.stderr, _STDOUT, _STDERR]:
            try:
                if not opened.closed:
                    opened.flush()
            except Exception:
                pass

        deadline = _time.monotonic() + _FLUSH_WAIT
        for flusher in started:
            left = deadline - _time.monotonic()
            flusher.join(max(left, 0))

    # An interrupt may land here and then no source is attached
    try:
        for name, (statement, old) in defined.items():
            value = _STATE.get(name)
            if value is old:
                continue

            # The full code for help() to print, from the first decorator on and with comments kept
            linenos = [part.lineno for part in [statement, *statement.decorator_list]]
            start = min(linenos)
            lines = code.split("\n")[start - 1 : statement.end_lineno]
            written = "\n".join(lines)
            try:
                value._smol_source = _textwrap.dedent(written)
            except (AttributeError, TypeError):
                pass
    except BaseException:
        pass

    # Undecodable filename bytes become lone surrogates that Rust can't accept
    clean = lambda text: text if text is None else text.encode("utf-8", "backslashreplace").decode()
    return {"error": clean(error), "code": clean(repaired)}
