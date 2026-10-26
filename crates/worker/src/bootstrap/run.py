import ast as _ast
import gc as _gc
import io as _io
import linecache as _linecache
import re as _re
import sys as _sys
import textwrap as _textwrap
import tokenize as _tokenize
import types as _types
import unicodedata as _unicodedata

def _text(value):
    return value if isinstance(value, str) else repr(value)

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
def _untagged(code):
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
            _ast.parse(candidate, _FILENAME)
        except SyntaxError:
            continue
        if candidate.strip():
            return candidate
    return None

def _run(code):
    global _SHOWN, _STDOUT, _STDERR
    # Streams the code closed are reopened on fds that are still open
    if _STDOUT.closed or _STDERR.closed:
        _STDOUT, _STDERR = _stream(1), _stream(2)
        _sys.__stdout__, _sys.__stderr__ = _STDOUT, _STDERR
    # Streams the previous snippet swapped in are not kept
    _sys.stdout, _sys.stderr = _STDOUT, _STDERR
    _SHOWN = _STDOUT

    error = repaired = None
    blocks = []
    defined = {}
    try:
        try:
            try:
                parsed = _ast.parse(code, _FILENAME)
            except SyntaxError:
                # The repaired code goes back to be saved, and line numbers and errors refer to it
                repaired = _untagged(code)
                if repaired is None:
                    raise
                code = repaired
                parsed = _ast.parse(code, _FILENAME)
            for node in parsed.body:
                if isinstance(node, _ast.Expr):
                    tree, mode = _ast.Expression(node.value), "eval"
                else:
                    tree, mode = _ast.Module([node], []), "exec"
                block = compile(tree, _FILENAME, mode)
                blocks.append(block)
        except SyntaxError as exc:
            blocks = []
            source = exc.text
            # Errors found only when compiling (like return outside a function) carry no source line
            lines = code.split("\n")
            if source is None and exc.lineno and 0 < exc.lineno <= len(lines):
                source = lines[exc.lineno - 1]
            source = (source or "").rstrip()
            # RustPython's ast.parse reports the column in UTF-8 bytes
            column = max((exc.offset or 1) - 1, 0)
            before = source.encode("utf-8", "surrogatepass")[:column]
            padding = []
            for c in before.decode("utf-8", "ignore"):
                # Wide characters take two terminal columns
                # and tabs are copied to expand like the line above
                if c == "\t":
                    padding.append(c)
                elif _unicodedata.east_asian_width(c) in ("W", "F"):
                    padding.append("  ")
                else:
                    padding.append(" ")
            caret = "".join(padding) + "^"
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
                text = _text(value)
                print(text, limit=None)

    except BaseException as exc:
        message = str(exc)
        kind = type(exc).__name__
        error = f"{kind}: {message}" if message else kind

        # Functions from earlier snippets also have the <smolcode> file name but other line numbers
        codes, pending = set(), list(blocks)
        while pending:
            compiled = pending.pop()
            identity = id(compiled)
            codes.add(identity)
            nested = [const for const in compiled.co_consts if isinstance(const, _types.CodeType)]
            pending.extend(nested)

        line, last, tb = None, None, exc.__traceback__
        while tb is not None:
            filename = tb.tb_frame.f_code.co_filename
            if id(tb.tb_frame.f_code) in codes:
                line = tb.tb_lineno
            if filename != _BOOTSTRAP:
                last = tb
            tb = tb.tb_next

        lines = code.split("\n")
        if line is not None and 0 < line <= len(lines):
            text = lines[line - 1].strip()
            error += f"\n  line {line}: {text}"

        if last is not None and last.tb_frame.f_code.co_filename != _FILENAME:
            where = last.tb_frame.f_code
            error += f"\n  {where.co_filename}, line {last.tb_lineno}, in {where.co_name}"
            source = _linecache.getline(where.co_filename, last.tb_lineno).strip()
            if source:
                error += f": {source}"

    finally:
        _SHOWN = None
        # Flush without closing because variables still hold them
        for opened in [*_OPEN_FILES, _sys.stdout, _sys.stderr, _STDOUT, _STDERR]:
            try:
                if not opened.closed:
                    opened.flush()
            except Exception:
                pass
        # RustPython's automatic GC only scans generation 0, so promoted cycles are only freed here
        _gc.collect()

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

    # Strings passed to the host can't hold lone surrogates
    clean = lambda text: text if text is None else text.encode("utf-8", "backslashreplace").decode()
    return {"error": clean(error), "code": clean(repaired)}
