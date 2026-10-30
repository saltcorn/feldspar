"""The Python half of the body pipeline (TODO "The Python code adapter" §3).

Three things live here rather than in Rust, and each for the same reason: they
are `ast`, `linecache` and `traceback` calls, and writing them through the C
API would be a transliteration nobody could read.

- ``install_module`` — the meta-path finder that serves the ``saltcorn``
  package out of the binary, so the surface an app builder writes has no file
  to find, no version to skew and nothing to install.
- ``compile_body`` — the author's source, parsed and *moved* into a
  ``FunctionDef`` rather than re-indented into one. That is what makes ``return``
  legal at the top level while leaving every line number where the author put
  it, including inside a triple-quoted string and inside a nested ``def``.
- ``execute`` — run one compiled body and answer what it returned.
- ``format_error`` — a failure rendered as the author's own frames and nothing
  else: a body that divides by zero should say ``line 7, in <body>``, not
  fifteen frames of runtime.

Nothing here is a security boundary. It is imported once, at interpreter start,
under a module name a body has no reason to touch; a body that reaches into it
anyway is the same body that can reach ``().__class__.__mro__``, which §10 of
the specification says out loud rather than pretending otherwise.
"""

import ast
import importlib.util
import linecache
import sys
import traceback

# The name the compiled wrapper carries. It is an identifier because `compile`
# wants one; `format_error` renders it as `<body>`, which is what the author
# would call it.
BODY_FN = "__sc_body"


class _EmbeddedLoader:
    """Executes one module whose source came out of the binary."""

    def __init__(self, name, source, filename):
        self._name = name
        self._source = source
        self._filename = filename

    def create_module(self, spec):
        # The default semantics: an ordinary module object.
        return None

    def exec_module(self, module):
        code = compile(self._source, self._filename, "exec")
        exec(code, module.__dict__)


class _EmbeddedFinder:
    """The meta-path entry that knows which modules live in the binary.

    First on ``sys.meta_path``, so ``import saltcorn`` cannot be answered by a
    file of that name somebody put in the environment. That is not a security
    boundary — §10 of the specification says there is none — it is the same rule
    a built-in module already has, said for a module that happens to be shipped
    as source.
    """

    def __init__(self):
        self.sources = {}

    def find_spec(self, fullname, path=None, target=None):
        entry = self.sources.get(fullname)
        if entry is None:
            return None
        source, filename = entry
        return importlib.util.spec_from_loader(
            fullname,
            _EmbeddedLoader(fullname, source, filename),
            origin=filename,
        )


_FINDER = _EmbeddedFinder()


def install_module(name, source):
    """Make ``name`` importable from the binary's own copy of ``source``.

    Registered in ``linecache`` for the reason a body's source is: without it a
    traceback through this module carries the right line numbers with a blank
    line beside each of them.
    """
    filename = f"<saltcorn {name}>"
    _FINDER.sources[name] = (source, filename)
    linecache.cache[filename] = (
        len(source),
        None,
        source.splitlines(keepends=True),
        filename,
    )
    if _FINDER not in sys.meta_path:
        sys.meta_path.insert(0, _FINDER)
    # A reinstall replaces what has already been imported, which is what a
    # second call could only mean.
    sys.modules.pop(name, None)


def compile_body(source, filename):
    """Compile ``source`` as the body of a function, through the AST.

    The wrapper node is given the first statement's position, so no statement of
    the author's is renumbered, and the source is registered in ``linecache``
    under ``filename`` — otherwise a rendered traceback carries the right line
    *numbers* with a blank line beside each of them, because the body is not a
    file anything can read.
    """
    tree = ast.parse(source, filename=filename, mode="exec")
    empty = ast.Pass(lineno=1, col_offset=0, end_lineno=1, end_col_offset=0)
    body = tree.body or [empty]
    extra = {}
    # `type_params` is a required field from 3.12 on and does not exist in 3.11,
    # and a developer build may link a 3.11 (`PYO3_PYTHON`), so both are live.
    if sys.version_info >= (3, 12):
        extra["type_params"] = []
    wrapper = ast.FunctionDef(
        name=BODY_FN,
        args=ast.arguments(
            posonlyargs=[],
            args=[],
            vararg=None,
            kwonlyargs=[],
            kw_defaults=[],
            kwarg=None,
            defaults=[],
        ),
        body=body,
        decorator_list=[],
        returns=None,
        **extra,
    )
    # The wrapper spans exactly what the author wrote, so nothing of theirs is
    # renumbered. All four positions are set rather than left to
    # `fix_missing_locations`, which would fill the missing ones in from the
    # module's defaults and produce a node ending before it starts — which is a
    # `ValueError` at compile time for any body whose first line is a comment.
    first, last = body[0], body[-1]
    wrapper.lineno = first.lineno
    wrapper.col_offset = 0
    wrapper.end_lineno = getattr(last, "end_lineno", None) or first.lineno
    wrapper.end_col_offset = getattr(last, "end_col_offset", None) or 0
    module = ast.Module(body=[wrapper], type_ignores=[])
    ast.fix_missing_locations(module)
    linecache.cache[filename] = (
        len(source),
        None,
        source.splitlines(keepends=True),
        filename,
    )
    return compile(module, filename, "exec")


def forget_source(filename):
    """Drop a body's source from ``linecache`` when its code object is evicted."""
    linecache.cache.pop(filename, None)


def execute(code, scope):
    """Define the body in ``scope`` and call it."""
    exec(code, scope)
    return scope[BODY_FN]()


def format_syntax_error(exc):
    """A ``SyntaxError`` as the author's own line and column."""
    message = getattr(exc, "msg", None) or str(exc)
    line = getattr(exc, "lineno", None)
    column = getattr(exc, "offset", None)
    if line is None:
        return message
    where = f"line {line}"
    if column is not None:
        where += f", column {column}"
    return f"{where}: {message}"


def format_error(exc, filename):
    """A failure rendered as the author's frames, most recent last.

    Frames from anywhere but ``filename`` are dropped: the wrapper's own frame,
    this module's ``execute``, and every frame inside a library the body called
    are noise at the point where somebody is looking for their own mistake. A
    body that fails entirely inside a library still gets the exception itself,
    which is the whole of what there is to say about it.
    """
    if isinstance(exc, SyntaxError) and getattr(exc, "filename", None) == filename:
        return format_syntax_error(exc)
    kind = type(exc).__name__
    text = str(exc)
    head = f"{kind}: {text}" if text else kind
    lines = []
    for frame in traceback.extract_tb(exc.__traceback__):
        if frame.filename != filename:
            continue
        name = "<body>" if frame.name == BODY_FN else frame.name
        source = (frame.line or "").strip()
        lines.append(f"  line {frame.lineno}, in {name}" + (f": {source}" if source else ""))
    if lines:
        return head + "\n" + "\n".join(lines)
    return head


def drain_async_exception():
    """Absorb a ``SetAsyncExc`` that was queued but never delivered.

    The instrument that stops a runaway body queues an exception on the thread
    state; CPython delivers it at the next bytecode boundary, which a body that
    had *already* finished never reaches. The exception would then be raised in
    whatever ran next on this thread — somebody else's run. So a thread that was
    fired at executes a few hundred harmless bytecodes before it goes back in the
    idle cache, and swallows whatever they deliver.
    """
    try:
        for _ in range(256):
            pass
    except BaseException:  # noqa: BLE001 — absorbing is the whole point
        return True
    return False
