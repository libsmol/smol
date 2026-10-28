import builtins as _builtins
import inspect as _inspect
import json as _json
import keyword as _keyword
import sys as _sys
import unicodedata as _unicodedata

class _Server:
    def __init__(self, name, source, listing, functions):
        self.__dict__.update(functions)
        self._name = name
        self._source = source
        self._listing = listing
        self._functions = ", ".join(functions)

    def __repr__(self):
        return self._listing

    def __getattr__(self, attribute):
        state = vars(self)
        # copy and pickle look attributes up before __init__ has run
        if attribute.startswith("__") or "_name" not in state:
            raise AttributeError(attribute)
        name, functions = state["_name"], state["_functions"]
        raise AttributeError(f"{name}: {attribute}: no such function (functions: {functions})")

_TYPES = {"string": "str", "boolean": "bool", "integer": "int", "number": "float", "array": "list", "object": "dict", "null": "None", "any": "Any"}

def _kind(field, schema, seen=()):
    if not isinstance(field, dict):
        return "Any"

    ref = field.get("$ref")
    if isinstance(ref, str):
        if ref in seen:
            return "Any"
        section, _, key = ref.removeprefix("#/").partition("/")
        defined = schema.get(section)
        target = defined.get(key) if isinstance(defined, dict) else None
        return _kind(target, schema, (*seen, ref))

    if "const" in field:
        return f"Literal[{field['const']!r}]"
    values = field.get("enum")
    if isinstance(values, list) and values:
        reprs = [repr(value) for value in values]
        shown = ", ".join(reprs)
        return f"Literal[{shown}]"

    options = field.get("anyOf") or field.get("oneOf")
    if isinstance(options, list) and options:
        kinds = []
        for option in options:
            kind = _kind(option, schema, seen)
            if kind not in kinds:
                kinds.append(kind)
        return " | ".join(kinds)

    kind = field.get("type")
    if isinstance(kind, list):
        kinds = [_TYPES.get(name, "Any") for name in kind if isinstance(name, str)]
        return " | ".join(kinds) or "Any"
    items = field.get("items")
    if kind == "array" and isinstance(items, dict):
        item = _kind(items, schema, seen)
        return f"list[{item}]"
    return _TYPES.get(kind, "Any") if isinstance(kind, str) else "Any"

def _identifier(name):
    # Python normalizes identifiers in code to NFKC when parsing
    name = _unicodedata.normalize("NFKC", name)
    chars = [c if ("_" + c).isidentifier() else "_" for c in name]
    ident = "".join(chars)
    if not ident[:1].isidentifier():
        ident = "_" + ident
    if _keyword.iskeyword(ident):
        ident += "_"
    return ident

def _unique(name, used):
    unique, number = name, 2
    while unique in used:
        unique = f"{name}_{number}"
        number += 1
    return unique

# Only stdlib names count because a sys.path lookup would vary with the working directory
_MODULES = frozenset(_sys.builtin_module_names) | _sys.stdlib_module_names

def _server(source, description, tools):
    builtins = vars(_builtins)
    name = _identifier(source)
    builtin = name in builtins and not isinstance(builtins[name], _Server)
    # A single import would shadow a server named like a module
    if name in _MODULES or builtin:
        name += "_mcp"
    taken = builtins.get(name)
    if isinstance(taken, _Server) and taken._source != source:
        return {"name": name, "server": taken._source}

    functions = {}
    entries = []
    headers = []
    for spec in tools:
        function, entry = _tool(name, spec, functions)
        functions[function.__name__] = function
        entries.append(entry)
        headers.append(function._smol_header)

    titled = name if name == source else f"{name} (configured as {source})"
    about = description.strip()
    about = f"\n{about}" if about else ""
    entries = "\n".join(entries)
    listing = f"MCP server {titled}{about}\n\nFunctions (help({name}.<function>) prints one in full):\n\n{entries}"
    builtins[name] = _Server(name, source, listing, functions)
    return {"name": name, "headers": headers}

def _unserver(name):
    builtins = vars(_builtins)
    server = builtins.get(name)
    if isinstance(server, _Server):
        del builtins[name]

_MAX_SUMMARY_CHARS = 160

# Closures written inside a loop would share the loop variable
def _tool(server, spec, used):
    name = _identifier(spec["name"])
    name = _unique(name, used)
    schema = spec.get("inputSchema") or {}
    listed = schema.get("required") or []
    required = set(listed)
    properties = schema.get("properties") or {}

    params = []
    for key, field in properties.items():
        field = field if isinstance(field, dict) else {}
        kind = _kind(field, schema)
        about = field.get("description")
        about = about.strip() if isinstance(about, str) else ""
        taken = {param[0] for param in params}
        ident = _identifier(key)
        ident = _unique(ident, taken)
        params.append((ident, key, kind, about, key not in required))
    # Required parameters must come first for positional arguments
    params.sort(key=lambda param: param[4])

    typed = []
    for ident, _, kind, _, optional in params:
        default = " = None" if optional else ""
        typed.append(f"{ident}: {kind}{default}")
    typed = ", ".join(typed)
    output = spec.get("outputSchema")
    returns = "dict" if output else "str"
    # Error prefixes and images in results both use this name
    called = f"{server}.{name}"
    header = f"{called}({typed}) -> {returns}"

    described = spec.get("description") or ""
    doc = described
    args = [f"\n    {ident}: {about}" for ident, _, _, about, _ in params if about]
    if args:
        doc += "\n\nArgs:" + "".join(args)
    if output:
        shape = _json.dumps(output, indent=2, ensure_ascii=False)
        # Newlines inside strings are escaped and every piece split here is a whole line
        indented = [" " * 8 + line for line in shape.split("\n")]
        shape = "\n".join(indented)
        doc += "\n\nReturns:\n    A dict matching this JSON schema:\n" + shape

    positional = _inspect.Parameter.POSITIONAL_OR_KEYWORD
    parameters = []
    for ident, _, _, _, optional in params:
        default = None if optional else _inspect.Parameter.empty
        parameter = _inspect.Parameter(ident, positional, default=default)
        parameters.append(parameter)
    signature = _inspect.Signature(parameters)
    keys = {ident: key for ident, key, *_ in params}
    # The signature's = None means left out, as servers that refuse null would reject it
    unset = {ident for ident, _, kind, _, optional in params if optional and "None" not in kind.split(" | ")}

    def function(*args, **kwargs):
        bound = _bound(signature, header, args, kwargs).arguments
        arguments = {keys[ident]: value for ident, value in bound.items() if value is not None or ident not in unset}
        try:
            # The agent finds the connection by the name used in the code
            return _call(server, spec["name"], arguments, called)
        except (TypeError, ValueError) as exc:
            raise type(exc)(f"{called}: {exc}") from None

    function.__name__ = function.__qualname__ = name
    function.__signature__ = signature
    function.__doc__ = doc
    function._smol_header = header

    lines = [line.strip() for line in described.splitlines() if line.strip()]
    first = lines[0] if lines else ""
    if len(first) > _MAX_SUMMARY_CHARS:
        first = first[: _MAX_SUMMARY_CHARS - 1] + "…"
    entry = f"{header}\n    {first}" if first else header
    return function, entry
