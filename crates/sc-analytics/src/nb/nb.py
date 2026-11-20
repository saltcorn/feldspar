# A notebook's `nb` in Python (analytics TODO A10.4): run once in a session's
# globals, it defines `nb`, the `print` a cell writes with, and
# `__session_cell__`, which runs each cell and answers its envelope. Every `nb`
# function is a plan sent over the cell's own `db` (`{"op": "nb", "what": …}`),
# answered by `sc_analytics::nb::answer` and the server — the same plans the
# JavaScript prelude sends.

import traceback as _nb_traceback


def _nb_lower(v):
    """A value as it crosses: plain JSON, a dataset as its chain."""
    lower = getattr(v, "_nb_lower", None)
    if lower is not None:
        return lower()
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, dict):
        return {str(k): _nb_lower(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [_nb_lower(x) for x in v]
    return {"__nb": "text", "text": repr(v)}


class Dataset:
    """A dataset built in a cell: a base and its steps, read by the server.

    ``nb.dataset("houses").filter("price > 100000").aggregate(["town"], n="count()")``
    """

    def __init__(self, base, steps=(), name=None):
        self._base = base
        self._steps = tuple(steps)
        self._name = name

    def _step(self, method, *args):
        step = {"method": method, "args": [_nb_lower(a) for a in args]}
        return Dataset(self._base, self._steps + (step,), self._name)

    def filter(self, condition):
        return self._step("filter", condition)

    def mutate(self, column=None, **columns):
        out = self
        for name, formula in {**(column or {}), **columns}.items():
            out = out._step("mutate", {name: formula})
        return out

    def select(self, *columns):
        return self._step("select", *columns)

    def sort(self, *keys):
        return self._step("sort", *keys)

    def aggregate(self, by=None, summaries=None, **named):
        return self._step("aggregate", by, {**(summaries or {}), **named})

    def limit(self, n):
        return self._step("limit", n)

    def sample(self, n, seed=None):
        return self._step("sample", n) if seed is None else self._step("sample", n, seed)

    def operation(self, kind, params):
        return self._step("then", kind, params)

    def named(self, name):
        return Dataset(self._base, self._steps, str(name))

    def rows(self, limit=None):
        return nb._send("rows", data=self, limit=limit)["rows"]

    def columns(self):
        return nb._send("columns", data=self)

    def _nb_lower(self):
        return {"__nb": "dataset", "base": self._base, "steps": list(self._steps), "name": self._name}

    def __repr__(self):
        return f"<dataset over {self._base!r}, {len(self._steps)} steps>"


class Panel:
    """A panel a cell made: shown when it is the cell's value, or by ``nb.show``."""

    def __init__(self, panel):
        self.panel = panel
        self.kind = panel.get("kind")

    def _nb_lower(self):
        return {"__nb": "panel", "kind": self.kind, "panel": self.panel}

    def __repr__(self):
        return f"<{self.kind} panel>"


class Fit:
    """A model's fit, finished: ``fit.output("residuals_fitted")`` is a panel."""

    def __init__(self, got):
        self._got = got
        self.fit = got["fit"]
        self.model = got["model"]
        self.status = got.get("status")
        self.outputs = [o["name"] for o in got["outputs"]]

    def output(self, name):
        for o in self._got["outputs"]:
            if o["name"] == name:
                return Panel(o["panel"])
        have = ", ".join(f"`{o}`" for o in self.outputs)
        raise ValueError(f"this fit has no output `{name}`; it has {have}")

    def _nb_lower(self):
        return {"__nb": "fit", **self._got}

    def __repr__(self):
        return f"<fit of {self.model!r}: {self.status}>"


class Nb:
    """The notebook: datasets, panels, fits, and what a cell shows."""

    def __init__(self):
        self._outputs = []

    def _send(self, what, **extra):
        plan = {"op": "nb", "what": what}
        plan.update({k: _nb_lower(v) for k, v in extra.items()})
        # Bound in each run's globals (and not written as a name here: inside a
        # class body, `__sc_db` would be mangled to `_Nb__sc_db`).
        return globals()["__sc_db"](plan)

    def dataset(self, name):
        if not isinstance(name, str) or not name:
            raise TypeError('nb.dataset() takes a dataset\'s or a table\'s name, as in nb.dataset("houses")')
        return Dataset(name)

    def plot(self, spec=None, **fields):
        return Panel(self._send("plot", spec={**(spec or {}), **fields})["panel"])

    def table(self, data, spec=None, **fields):
        return Panel(self._send("table", data=data, spec={**(spec or {}), **fields})["panel"])

    def test(self, data, spec=None, **fields):
        return Panel(self._send("test", data=data, spec={**(spec or {}), **fields})["panel"])

    def card(self, data, spec=None, **fields):
        return Panel(self._send("card", data=data, spec={**(spec or {}), **fields})["panel"])

    def map(self, spec):
        return Panel(self._send("map", spec=spec)["panel"])

    def markdown(self, text):
        return Panel(self._send("text", markdown=str(text))["panel"])

    def fit(self, model, **options):
        return Fit(self._send("fit", model=model, options=options or None))

    def show(self, *values):
        for v in values:
            self._outputs.append({"__nb": "text", "text": v} if isinstance(v, str) else v)

    def __repr__(self):
        return "<notebook>"


nb = Nb()


def print(*args, sep=" ", end="\n", file=None, flush=False):  # noqa: A001 — the cell's own
    nb._outputs.append({"__nb": "text", "text": sep.join(str(a) for a in args)})


def __session_cell__(code, last):
    nb._outputs = []
    try:
        exec(code, globals())
        value = None if last is None else eval(last, globals())
        return {
            "__nb": "finish",
            "outputs": [_nb_lower(o) for o in nb._outputs],
            "value": _nb_lower(value),
            "has_value": value is not None,
        }
    except Exception as e:  # noqa: BLE001 — the cell's error is its output
        line = None
        for frame in _nb_traceback.extract_tb(e.__traceback__):
            if frame.filename == code.co_filename:
                line = frame.lineno
        text = str(e)
        ours = type(e).__module__ == "saltcorn"
        message = text if ours and text else (f"{type(e).__name__}: {text}" if text else type(e).__name__)
        return {
            "__nb": "fail",
            "outputs": [_nb_lower(o) for o in nb._outputs],
            "message": message,
            "line": line,
        }
