"""What a Python **plugin module** declares, and how this server reads it.

Specification §2 of the Python API ("A plugin module"), §8 (two module
languages, one ``_fd_modules``) and §11 (a reload is best-effort).

A plugin is an ordinary ``pip``-installable distribution that says what it
supplies with decorators::

    import saltcorn as sc

    sc.settings(sc.Field.string("api_key", label="API key", secret=True))

    @sc.on_load
    def load(configuration):
        global client
        client = Scorer(configuration["api_key"])

    @sc.action(description="Score a lead",
               config=[sc.Field.string("model", required=True)])
    def score_lead(row, config, user):
        return {"score": client.score(row["email"], model=config["model"])}

    @sc.function(description="Markdown to HTML")
    def md_to_html(text: str) -> str: ...

    @sc.table_provider("CSV file", config=[sc.Field.string("path", required=True)])
    class CsvTable: ...

    @sc.model_provider("ridge", outcome=sc.Outcome.regression("label"))
    class Ridge: ...

Everything above is re-exported from ``saltcorn`` so an author writes ``sc.``
and never names this module; the host imports *this* one, because the ops below
are its side of the same conversation.

# The registry is per package, and the key is the code's own ``__module__``

One interpreter holds every plugin (§1), so two of them must not be able to see
each other's declarations. Each registration is filed under the **root package**
of the decorated object's own ``__module__`` — not under "whichever plugin the
host happens to be importing", which would misfile anything a plugin registered
outside its own import, and not under a global list, which would merge them.
``settings()`` decorates nothing, so it reads its caller's frame instead, which
is the same name one level of indirection away.

# What the host asks for, and what it is told

One entry point, [`dispatch`], and one payload each way. The manifest it answers
a load with is the **same shape** a JavaScript module answers
(``sc_module::ModuleManifest``), which is what lets one Modules tab, one action
registry and one pair of catalog hosts serve both languages without either
knowing the other exists.

# What is *not* here

A sandbox (§10). A plugin's code runs with the server's privileges, as its
``pip install`` already did, and this module does not pretend otherwise: there
is no gate on a plugin's imports, because a plugin is a package an admin chose
to install rather than a body somebody typed into a form.
"""

import importlib
import importlib.metadata
import inspect
import os
import re
import sys
import traceback
import warnings

#: The distribution metadata group a package advertises its plugin under — the
#: idiomatic way a Python distribution says "this is a plugin of X" (§9).
ENTRY_POINT_GROUP = "saltcorn.plugins"

#: The parameters an action may ask for by name (§2). Anything else in a
#: signature is a mistake, and is refused by name rather than passed as ``None``.
ACTION_PARAMETERS = (
    "row",
    "old",
    "table",
    "user",
    "payload",
    "config",
    "configuration",
    "trigger",
    "mode",
)

#: What a field may be restricted to the **dataset's columns** by — the server
#: queries `sc_model::provider` resolves against the dataset a model is over
#: (§10). A model provider naming a label declares one of these rather than a
#: free-text box, so the admin picks a column that exists.
COLUMN_QUERIES = (
    "dataset_columns",
    "dataset_numeric_columns",
    "dataset_categorical_columns",
)

#: The types [`Field`] offers, in **this system's** vocabulary rather than v1's
#: (§2: "it declares its settings in this system's field vocabulary").
FIELD_TYPES = ("string", "int", "float", "bool", "date", "json")

#: Python annotations that mean one of those, for the signature a function
#: reports. An annotation this does not know is reported under its own name: a
#: signature is a hint in an editor, not a contract anything here enforces.
_ANNOTATIONS = {
    str: "string",
    int: "int",
    float: "float",
    bool: "bool",
    dict: "json",
    list: "json",
}


class Field:
    """One setting, in the vocabulary every configurable thing here speaks.

    A thin constructor over this system's ``FormField`` (§6.2): what an admin
    sees is rendered by the same trigger and settings forms that render a file
    store's backend, which is why no screen has to learn what a Python module
    is.

    ``primary_key`` and ``unique`` are past §2's list and mean nothing for a
    *setting*: they are here because a **table provider**'s ``fields()`` answers
    the same objects, and a provided table with no primary key is a table
    nothing can address a row of.
    """

    __slots__ = (
        "name",
        "type",
        "label",
        "required",
        "default",
        "options",
        "secret",
        "multiline",
        "primary_key",
        "unique",
        "server_query",
    )

    def __init__(
        self,
        name,
        type="string",  # noqa: A002 — the field's own word for it
        label=None,
        required=False,
        default=None,
        options=None,
        secret=False,
        multiline=False,
        primary_key=False,
        unique=False,
        server_query=None,
    ):
        if not isinstance(name, str) or not name.strip():
            raise ValueError("a field needs a name")
        if type not in FIELD_TYPES:
            raise ValueError(
                f"`{type}` is not a field type; the types are {', '.join(FIELD_TYPES)}"
            )
        self.name = name.strip()
        self.type = type
        self.label = label
        self.required = bool(required)
        self.default = default
        self.options = list(options) if options is not None else None
        self.secret = bool(secret)
        self.multiline = bool(multiline)
        self.primary_key = bool(primary_key)
        self.unique = bool(unique)
        if server_query is not None and server_query not in COLUMN_QUERIES:
            raise ValueError(
                f"`{server_query}` is not a server query; they are "
                + ", ".join(COLUMN_QUERIES)
            )
        self.server_query = server_query

    @classmethod
    def string(cls, name, **kwargs):
        return cls(name, "string", **kwargs)

    @classmethod
    def int(cls, name, **kwargs):  # noqa: A003
        return cls(name, "int", **kwargs)

    @classmethod
    def float(cls, name, **kwargs):  # noqa: A003
        return cls(name, "float", **kwargs)

    @classmethod
    def bool(cls, name, **kwargs):  # noqa: A003
        return cls(name, "bool", **kwargs)

    @classmethod
    def date(cls, name, **kwargs):
        return cls(name, "date", **kwargs)

    @classmethod
    def json(cls, name, **kwargs):
        return cls(name, "json", **kwargs)

    @classmethod
    def column(cls, name, **kwargs):
        """A field restricted to the **dataset's columns** — a label picker.

        The options are filled in by the server against the dataset the model is
        over, which is why they are not listed here: a provider declares its form
        once and every model has a different dataset.
        """
        return cls(name, "string", server_query="dataset_columns", **kwargs)

    @classmethod
    def numeric_column(cls, name, **kwargs):
        """The same, restricted to the columns a number can be read from."""
        return cls(name, "string", server_query="dataset_numeric_columns", **kwargs)

    @classmethod
    def categorical_column(cls, name, **kwargs):
        """The same, restricted to the columns that are categories."""
        return cls(name, "string", server_query="dataset_categorical_columns", **kwargs)

    def to_json(self):
        """The declaration as it crosses the seam, for the host to translate."""
        declared = {
            "name": self.name,
            "type": self.type,
            "required": self.required,
            "secret": self.secret,
            "multiline": self.multiline,
        }
        if self.label is not None:
            declared["label"] = self.label
        if self.default is not None:
            declared["default"] = self.default
        if self.options is not None:
            declared["options"] = self.options
        if self.primary_key:
            declared["primary_key"] = True
        if self.unique:
            declared["unique"] = True
        if self.server_query is not None:
            declared["server_query"] = self.server_query
        return declared

    def __repr__(self):
        return f"Field({self.name!r}, {self.type!r})"


def _fields(declared, what):
    """A ``config=`` list as [`Field`]s, refusing anything that is not one."""
    out = []
    for field in declared or ():
        if isinstance(field, Field):
            out.append(field)
        elif isinstance(field, dict):
            # A dict of the same keys, for a plugin that would rather build its
            # settings than write them out.
            out.append(Field(**field))
        else:
            raise TypeError(
                f"{what} declares a setting that is not a saltcorn.Field: {field!r}"
            )
    return out


class _Action:
    __slots__ = ("name", "fn", "description", "config", "require_row")

    def __init__(self, name, fn, description, config, require_row):
        self.name = name
        self.fn = fn
        self.description = description
        self.config = config
        self.require_row = require_row


class _Function:
    __slots__ = ("name", "fn", "description")

    def __init__(self, name, fn, description):
        self.name = name
        self.fn = fn
        self.description = description


class _Provider:
    __slots__ = ("name", "cls", "config", "_instance")

    def __init__(self, name, cls, config):
        self.name = name
        self.cls = cls
        self.config = config
        self._instance = None

    def instance(self):
        """The provider object, built once.

        Lazily, because a provider's ``__init__`` is the plugin's code and a
        module that would not load must still be able to show its settings form
        — which is what an admin needs to fix it.
        """
        if self._instance is None:
            self._instance = self.cls()
        return self._instance


class Frame:
    """A materialised dataset, **column by column** (TODO §9, §14).

    Columnar because every consumer wants a column and because the wire is: a
    50 000 × 12 dataset crosses as twelve JSON arrays and not as 50 000 objects
    with the same twelve keys repeated. It lands here as lists, which is what
    ``numpy.asarray`` takes directly::

        import numpy as np
        X = np.asarray([frame[name] for name in frame.features(config["label"])]).T
        y = np.asarray(frame[config["label"]])

    A missing value is ``None``. The host drops rows with one before a fit and
    refuses one at predict time, so a provider that is handed a ``None`` is
    being handed a column the host was told to keep — it is the provider's
    question what to do with it.
    """

    __slots__ = ("rows", "types", "_columns", "_order")

    def __init__(self, payload):
        payload = payload or {}
        self._columns = {}
        self._order = []
        self.types = {}
        for column in payload.get("columns") or ():
            name = column.get("name")
            self._order.append(name)
            self._columns[name] = list(column.get("values") or ())
            self.types[name] = column.get("type")
        self.rows = int(payload.get("rows") or 0)

    @property
    def names(self):
        """Every column name, in the dataset's own order."""
        return list(self._order)

    def features(self, *excluding):
        """Every column but the ones named — the usual "everything but the label"."""
        skip = {name for name in excluding if name}
        return [name for name in self._order if name not in skip]

    def column(self, name):
        """One column as a list, or a `KeyError` naming what there is instead."""
        if name not in self._columns:
            raise KeyError(
                f"the frame has no column `{name}`; it has "
                + (", ".join(f"`{n}`" for n in self._order) or "none")
            )
        return self._columns[name]

    def __getitem__(self, name):
        return self.column(name)

    def __contains__(self, name):
        return name in self._columns

    def __len__(self):
        return self.rows

    def matrix(self, names=None):
        """The named columns as a row-major list of lists — one list per row."""
        chosen = list(names) if names is not None else self.names
        columns = [self.column(name) for name in chosen]
        return [[column[i] for column in columns] for i in range(self.rows)]

    def __repr__(self):
        return f"Frame({self.rows} rows, {len(self._order)} columns)"


class Outcome:
    """What a fit of a model provider produces, as a declaration (TODO §10).

    A *function of the configuration*, which is why it is a declaration and not
    a constant: a random forest is a regressor or a classifier depending on the
    type of the column its configuration names. So what is declared here is
    **which configuration key** holds the label, and the host resolves it
    against the dataset.
    """

    @staticmethod
    def supervised(label="label"):
        """A regression when the labelled column is numeric, else a classification."""
        return {"kind": "supervised", "label": label}

    @staticmethod
    def regression(label="label"):
        return {"kind": "regression", "label": label}

    @staticmethod
    def classification(label="label"):
        return {"kind": "classification", "label": label}

    @staticmethod
    def cluster():
        return {"kind": "cluster"}

    @staticmethod
    def embedding(components="components"):
        """A vector per row, as long as the named configuration key says."""
        return {"kind": "embedding", "components": components}

    @staticmethod
    def test():
        """A hypothesis test: no per-row output, and the parameters are the answer."""
        return {"kind": "test"}


class Parameter:
    """One fitted parameter, in the shape the instance screen renders it in (§7).

    Three variants and no more, so the screen has three renderings to write and
    never has to know what a coefficient or an explained-variance ratio is.
    """

    @staticmethod
    def scalar(name, value):
        return {"block": "scalar", "name": name, "value": float(value)}

    @staticmethod
    def table(name, columns, rows):
        """A coefficient table, cluster centres, feature importances.

        Every row must be as wide as ``columns``: the host refuses a ragged one
        rather than rendering a standard error under *p*.
        """
        return {
            "block": "table",
            "name": name,
            "columns": [str(column) for column in columns],
            "rows": [{"cells": list(row)} for row in rows],
        }

    @staticmethod
    def text(name, body):
        """Text the provider produced and nobody should reformat."""
        return {"block": "text", "name": name, "body": str(body)}


class Prediction:
    """What a fitted provider answers for one row.

    A ``predict`` may also answer the bare values — a number, a class name, or a
    list for a vector — which is what a provider written over numpy naturally
    produces. These exist for the two answers a bare value cannot carry: a
    cluster **number** (which is not the number a regression predicts) and a
    class with its probability.
    """

    @staticmethod
    def number(value):
        return {"prediction": "number", "value": float(value)}

    @staticmethod
    def cluster(index):
        return {"prediction": "cluster", "cluster": int(index)}

    @staticmethod
    def class_index(index, probability=None):
        """A classification's answer as an **index** into the fitted encoding.

        What a provider answers, because a class index is what the target
        encoding handed it and what its arithmetic produced. The host maps it
        back to the class name; a provider that answered the name itself would be
        guessing at an encoding it does not hold.
        """
        out = {"prediction": "class_index", "index": int(index)}
        if probability is not None:
            out["probability"] = float(probability)
        return out

    @staticmethod
    def class_(name, probability=None):
        out = {"prediction": "class", "class": str(name)}
        if probability is not None:
            out["probability"] = float(probability)
        return out

    @staticmethod
    def vector(values):
        return {"prediction": "vector", "values": [float(v) for v in values]}


class _ModelProvider:
    __slots__ = (
        "name",
        "cls",
        "description",
        "config",
        "hyperparameters",
        "outcome",
        "standardise",
        "outputs",
        "_instance",
    )

    def __init__(
        self,
        name,
        cls,
        description,
        config,
        hyperparameters,
        outcome,
        standardise,
        outputs=None,
    ):
        self.name = name
        self.cls = cls
        self.description = description
        self.config = config
        self.hyperparameters = hyperparameters
        self.outcome = outcome
        self.standardise = standardise
        self.outputs = outputs
        self._instance = None

    def instance(self):
        """The provider object, built once — as a table provider's is, and for
        the same reason: a module that would not construct must still be able to
        show its form."""
        if self._instance is None:
            self._instance = self.cls()
        return self._instance


class Registry:
    """What one package declared. One per package, kept apart by name."""

    def __init__(self, package):
        self.package = package
        #: The distribution the host loaded it as, once it has.
        self.distribution = None
        self.settings = []
        self.on_load = None
        self.actions = {}
        self.functions = {}
        self.providers = {}
        self.model_providers = {}
        self.issues = []
        #: The module's own configuration, as the last load was given it.
        self.configuration = {}


#: Every package that has declared anything, by root package name.
_REGISTRIES = {}

#: The registries the host has loaded, by normalised distribution name — the
#: name `_fd_modules` holds and every host call arrives under.
_BY_DISTRIBUTION = {}


def normalise(name):
    """A distribution name in the one spelling everything here compares."""
    return re.sub(r"[-_.]+", "-", str(name)).strip().lower()


def _root(module_name):
    return (module_name or "").split(".")[0]


def _registry(package):
    registry = _REGISTRIES.get(package)
    if registry is None:
        registry = Registry(package)
        _REGISTRIES[package] = registry
    return registry


def _registry_of(obj):
    """The registry of whatever package `obj` was defined in."""
    return _registry(_root(getattr(obj, "__module__", None)))


# --- the decorators ---------------------------------------------------------


def settings(*fields):
    """Declare the module's own settings — what the Modules tab asks an admin.

    Called at module level rather than as a decorator, because it decorates
    nothing: the fields *are* the declaration. The package it belongs to is the
    caller's own module, which is the same answer the decorators get from
    ``__module__``.
    """
    package = _root(sys._getframe(1).f_globals.get("__name__"))
    registry = _registry(package)
    registry.settings = _fields(fields, f"the module `{package}`")
    return registry.settings


def on_load(fn):
    """Called at load and after every configuration change (§11).

    Where a plugin builds what its actions close over: a client, a model, a
    connection. Its failure is an **issue** on the module rather than a failed
    load, so an admin whose API key is wrong still gets the settings form that
    is where they would fix it.
    """
    _registry_of(fn).on_load = fn
    return fn


def action(fn=None, *, name=None, description="", config=(), require_row=False):
    """Register an action, which is an ordinary trigger action to this server.

    ``config`` is what the *trigger form* asks for when an admin picks this
    action; the module's own settings are separate and reach the function as
    ``configuration``.

    The function asks for what it wants — any of ``row``, ``old``, ``table``,
    ``user``, ``payload``, ``config``, ``configuration``, ``trigger``, ``mode``,
    or ``**kwargs`` for all of them.
    """

    def register(fn):
        registered = name or fn.__name__
        registry = _registry_of(fn)
        if inspect.iscoroutinefunction(fn):
            # Nothing here can await one: a run is a thread, and there is no
            # event loop in the interpreter to give it. Said now, by name,
            # rather than as a coroutine object arriving where a result was
            # expected.
            registry.issues.append(
                f"the action `{registered}` is `async def`, which this version cannot "
                f"call; it is not available"
            )
            return fn
        registry.actions[registered] = _Action(
            registered,
            fn,
            description or (inspect.getdoc(fn) or "").split("\n")[0],
            _fields(config, f"the action `{registered}`"),
            bool(require_row),
        )
        return fn

    return register(fn) if fn is not None else register


def function(fn=None, *, name=None, description=""):
    """Register a function: callable from a formula and from a code body.

    The signature the editor shows is read from the function itself
    (``inspect``), which is the one place this API is better than the JavaScript
    one rather than merely different.
    """

    def register(fn):
        registered = name or fn.__name__
        registry = _registry_of(fn)
        if inspect.iscoroutinefunction(fn):
            registry.issues.append(
                f"the function `{registered}` is `async def`, which this version cannot "
                f"call; it is not available"
            )
            return fn
        registry.functions[registered] = _Function(
            registered,
            fn,
            description or (inspect.getdoc(fn) or "").split("\n")[0],
        )
        return fn

    return register(fn) if fn is not None else register


def table_provider(name, *, config=()):
    """Register a table provider: a table whose rows this class produces.

    The class answers ``fields(configuration)`` and ``rows(configuration, …)``,
    and *may* answer ``insert_row``, ``update_row`` and ``delete_rows`` — whose
    **presence** is what makes a table backed by it writable, which is v1's rule
    too.
    """

    def register(cls):
        _registry_of(cls).providers[name] = _Provider(
            name, cls, _fields(config, f"the table provider `{name}`")
        )
        return cls

    return register


#: The outcome kinds a model provider may declare, and the key each needs.
_OUTCOME_KINDS = {
    "supervised": "label",
    "regression": "label",
    "classification": "label",
    "cluster": None,
    "embedding": "components",
    "test": None,
}


def _outcome(declared, what):
    """One ``outcome=`` declaration, checked here rather than at fit time."""
    if not isinstance(declared, dict):
        raise TypeError(
            f"{what} must declare an outcome, such as "
            f"`outcome=saltcorn.Outcome.regression(\"label\")`"
        )
    kind = declared.get("kind")
    if kind not in _OUTCOME_KINDS:
        raise ValueError(
            f"{what} declares the outcome kind {kind!r}, which is not one of "
            + ", ".join(_OUTCOME_KINDS)
        )
    key = _OUTCOME_KINDS[kind]
    if key and not isinstance(declared.get(key), str):
        raise ValueError(
            f"{what} declares a {kind} outcome, which needs a string `{key}` naming "
            f"the configuration key that holds it"
        )
    return {"kind": kind, key: declared[key]} if key else {"kind": kind}


def model_provider(
    name,
    *,
    description="",
    config=(),
    hyperparameters=(),
    outcome=None,
    standardise=False,
    outputs=None,
):
    """Register a **model provider**: code that can fit something (TODO §10, §14).

    The class answers ``fit(frame, configuration, hyperparameters)`` — returning
    ``{"state": ..., "parameters": [...], "warnings": ["..."]}``, or just the
    state — and ``predict(state, frame)``, returning one answer per row.

    ``warnings`` are sentences the admin should read before trusting the fit,
    saying what to do ("the solver did not converge: raise `max_iter`"). A
    ``warnings.warn`` raised while ``fit`` runs (scikit-learn's
    ``ConvergenceWarning``) is caught and added to them. The host records them
    on the fit, and a fit with any is not clean, so ``fit_model`` with
    ``activate: if_clean`` leaves it inactive::

        @sc.model_provider(
            "ridge",
            description="Linear regression with an L2 penalty",
            config=[sc.Field.string("label", label="Label", required=True)],
            hyperparameters=[sc.Field.float("alpha", label="Alpha", default=1.0)],
            outcome=sc.Outcome.regression("label"),
            standardise=True,
        )
        class Ridge:
            def fit(self, frame, configuration, hyperparameters): ...
            def predict(self, state, frame): ...

    ``outputs`` says what a fit shows (analytics TODO A3.2), as a list of
    dicts: ``{"name": "coefficients", "label": "Coefficients", "kind":
    "parameters", "block": "Coefficients"}``, ``{"name": "metrics", "label":
    "Metrics", "kind": "metrics"}``, or a plot — ``{"name": "fitted", "label":
    "Fitted values", "kind": "plot", "data": "rows", "spec": {...}, "optional":
    True}`` — whose ``spec`` is a plot spec over output data: the host's
    ``rows`` (each scored row, with ``actual``, ``fitted`` and ``residual``) or
    a frame of the provider's own, which ``fit`` returns as ``"outputs":
    {"name": {"rows": n, "columns": [{"name": ..., "type": "float", "values":
    [...]}]}}``. Without it, a fit shows the standard outputs of its outcome.

    It declares **no metrics**, and cannot: R², RMSE, accuracy and the confusion
    matrix are computed by the host over the same splits with the same code for
    every provider, which is what makes this estimator's number comparable with
    a built-in regression's.
    """

    def register(cls):
        what = f"the model provider `{name}`"
        _registry_of(cls).model_providers[name] = _ModelProvider(
            name,
            cls,
            description,
            _fields(config, what),
            _fields(hyperparameters, what),
            _outcome(outcome, what),
            bool(standardise),
            list(outputs) if outputs is not None else None,
        )
        return cls

    return register


# --- discovery --------------------------------------------------------------


def _entry_point_module(distribution):
    """The module a distribution advertises as its plugin, if it does (§9)."""
    for entry in distribution.entry_points:
        if entry.group == ENTRY_POINT_GROUP:
            # `module:attribute` is the entry-point syntax; what is imported is
            # the module, because the declarations are made by importing it.
            return entry.value.split(":")[0].strip()
    return None


def _top_level(distribution):
    """The distribution's own top-level package, off its metadata."""
    declared = None
    try:
        declared = distribution.read_text("top_level.txt")
    except (OSError, ValueError):
        declared = None
    for line in (declared or "").splitlines():
        if line.strip():
            return line.strip()
    # No `top_level.txt` — a wheel built by something other than setuptools.
    # The installed files say the same thing: the first directory that is not
    # metadata is the package.
    for path in distribution.files or ():
        parts = str(path).split("/")
        if len(parts) > 1 and not parts[0].endswith((".dist-info", ".egg-info", ".data")):
            return parts[0]
    return None


def module_name(name):
    """Which module to import for the distribution `name` (§9).

    The ``saltcorn.plugins`` entry point when the distribution declares one,
    else its top-level package, else the name itself with the spelling a
    distribution name is allowed and a module name is not.
    """
    try:
        distribution = importlib.metadata.distribution(name)
    except Exception:  # noqa: BLE001 — not installed, or metadata that will not parse
        distribution = None
    if distribution is not None:
        found = _entry_point_module(distribution) or _top_level(distribution)
        if found:
            return found
    return normalise(name).replace("-", "_")


def _forget(package):
    """Drop a package's modules from ``sys.modules`` (§11).

    Correct for a pure-Python package and best-effort for anything else: every
    object built from the old modules lives on, and a package with a C extension
    in it cannot be re-initialised at all. The Modules tab says a version change
    takes full effect at the next restart, which is the guarantee this is not.
    """
    for name in [n for n in sys.modules if n == package or n.startswith(package + ".")]:
        sys.modules.pop(name, None)


# --- the manifest -----------------------------------------------------------


def _annotation(parameter):
    """The declared type of one parameter, in this system's vocabulary."""
    annotation = parameter.annotation
    if annotation is inspect.Parameter.empty:
        return None
    known = _ANNOTATIONS.get(annotation)
    if known is not None:
        return known
    return getattr(annotation, "__name__", None) or str(annotation)


def _arguments(fn):
    """A function's signature, as the manifest reports it."""
    try:
        signature = inspect.signature(fn)
    except (TypeError, ValueError):
        # A builtin or a C function has no signature to read. Reported as
        # unknown rather than as none, which would read as "takes nothing".
        return []
    out = []
    for parameter in signature.parameters.values():
        if parameter.kind in (parameter.VAR_POSITIONAL, parameter.VAR_KEYWORD):
            continue
        out.append({"name": parameter.name, "type": _annotation(parameter)})
    return out


def manifest(distribution, registry):
    """What this package supplies, in the shape both languages answer with."""
    return {
        "name": distribution,
        "plugin_name": registry.package,
        "actions": [
            {
                "name": action.name,
                "description": action.description,
                "requireRow": action.require_row,
                "configFields": [field.to_json() for field in action.config],
            }
            for action in registry.actions.values()
        ],
        "functions": [
            {
                "name": function.name,
                "description": function.description,
                # Nothing here is awaited: an `async def` never reaches the
                # manifest, so this is what a signature says and not a promise.
                "isAsync": False,
                "arguments": _arguments(function.fn),
            }
            for function in registry.functions.values()
        ],
        "table_providers": [
            {
                "name": provider.name,
                "config_fields": [field.to_json() for field in provider.config],
            }
            for provider in registry.providers.values()
        ],
        "model_providers": [
            {
                "name": provider.name,
                "description": provider.description,
                "config_fields": [field.to_json() for field in provider.config],
                "hyperparameters": [
                    field.to_json() for field in provider.hyperparameters
                ],
                "outcome": provider.outcome,
                "standardise": provider.standardise,
                "outputs": provider.outputs,
            }
            for provider in registry.model_providers.values()
        ],
        "config_fields": [field.to_json() for field in registry.settings],
        # Every entity type this version does not load is one a Python plugin
        # has no way to declare in the first place: there is no decorator for a
        # view, a type or a fieldview, so there is nothing to count.
        "unsupported": [],
        "issues": list(registry.issues),
    }


# --- errors -----------------------------------------------------------------


def format_error(exc):
    """A plugin's failure as its own frames, most recent last.

    This module's frames and the surface's are dropped: somebody looking at
    ``score_lead`` failing wants the line in ``their`` file, not the dispatch
    that reached it.
    """
    kind = type(exc).__name__
    text = str(exc)
    head = f"{kind}: {text}" if text else kind
    lines = []
    for frame in traceback.extract_tb(exc.__traceback__):
        if frame.filename.startswith("<saltcorn"):
            continue
        where = os.path.basename(frame.filename)
        source = (frame.line or "").strip()
        lines.append(
            f"  {where}, line {frame.lineno}, in {frame.name}"
            + (f": {source}" if source else "")
        )
    if lines:
        return head + "\n" + "\n".join(lines[-4:])
    return head


def _require(distribution):
    registry = _BY_DISTRIBUTION.get(normalise(distribution))
    if registry is None:
        raise LookupError(
            f"the Python module `{distribution}` is not loaded in this interpreter; "
            f"it may have been uninstalled, or failed to load"
        )
    return registry


def _selected(fn, available, what):
    """The arguments `fn` asked for, of the ones there are.

    ``**kwargs`` takes them all. A parameter that is not one of the available
    names is refused **by name**, rather than passed as ``None``: a plugin that
    asks for `rows` when the name is `row` has a typo, and finding it as an
    empty value inside somebody's action is the silent failure this system
    refuses.
    """
    try:
        signature = inspect.signature(fn)
    except (TypeError, ValueError):
        return dict(available)
    selected = {}
    for parameter in signature.parameters.values():
        if parameter.kind == parameter.VAR_KEYWORD:
            return dict(available)
        if parameter.kind == parameter.VAR_POSITIONAL:
            continue
        if parameter.name in available:
            selected[parameter.name] = available[parameter.name]
        elif parameter.default is inspect.Parameter.empty:
            raise TypeError(
                f"{what} asks for `{parameter.name}`, which is not one of the "
                f"parameters it is passed: {', '.join(sorted(available))}"
            )
    return selected


# --- the ops ----------------------------------------------------------------


def op_load(payload):
    """Import a distribution's plugin module and read what it declared."""
    distribution = payload["module"]
    configuration = payload.get("configuration") or {}
    site_packages = payload.get("site_packages")
    if site_packages and site_packages not in sys.path:
        # The environment may have been created, or installed into, after this
        # interpreter fixed its `sys.path` at start — an install is a
        # subprocess, and the boot path cannot know what a later one will put
        # there.
        sys.path.append(site_packages)
    # A distribution installed since the last import is invisible to the
    # finders' directory caches until this is called.
    importlib.invalidate_caches()

    name = module_name(distribution)
    package = _root(name)
    # §11: a reload is a re-import, so the old registrations go first — a
    # decorator that is no longer there must not survive as an action.
    _forget(package)
    _REGISTRIES.pop(package, None)
    module = importlib.import_module(name)

    registry = _REGISTRIES.get(package)
    if registry is None:
        registry = _registry(package)
        registry.issues.append(
            f"`{getattr(module, '__name__', name)}` declared nothing: a Saltcorn Python "
            f"plugin registers what it supplies with the `saltcorn` decorators "
            f"(`@saltcorn.action`, `@saltcorn.function`, `@saltcorn.table_provider`, "
            f"`@saltcorn.model_provider`)"
        )
    registry.distribution = distribution
    registry.configuration = configuration
    _BY_DISTRIBUTION[normalise(distribution)] = registry

    if registry.on_load is not None:
        try:
            registry.on_load(configuration)
        except Exception as exc:  # noqa: BLE001 — reported, not fatal
            registry.issues.append(f"its `on_load` failed: {format_error(exc)}")
    return manifest(distribution, registry)


def op_unload(payload):
    """Forget a distribution, after an uninstall.

    The **import** stays behind, which §11 says out loud: Python has no unload,
    and a package whose modules were dropped from ``sys.modules`` has still left
    every object it built alive. What this does is what can be done — nothing
    here will answer for it again.
    """
    registry = _BY_DISTRIBUTION.pop(normalise(payload["module"]), None)
    if registry is not None:
        _REGISTRIES.pop(registry.package, None)
        _forget(registry.package)
    return None


def op_action(payload):
    """Run one action, passing only the parameters it declared (§2)."""
    registry = _require(payload["module"])
    name = payload["action"]
    action = registry.actions.get(name)
    if action is None:
        raise LookupError(
            f"the Python module `{registry.distribution}` supplies no action `{name}`"
        )
    available = dict(payload.get("args") or {})
    # The module's own settings are the host's to supply rather than the
    # caller's: they are what an admin typed on the Modules tab, and a trigger
    # cannot change them.
    available["configuration"] = registry.configuration
    for parameter in ACTION_PARAMETERS:
        available.setdefault(parameter, None)
    return action.fn(**_selected(action.fn, available, f"the action `{name}`"))


def op_function(payload):
    """Call one function with v1's positional arguments."""
    registry = _require(payload["module"])
    name = payload["function"]
    function = registry.functions.get(name)
    if function is None:
        raise LookupError(
            f"the Python module `{registry.distribution}` supplies no function `{name}`"
        )
    return function.fn(*(payload.get("args") or []))


def _provider(payload):
    registry = _require(payload["module"])
    name = payload["provider"]
    provider = registry.providers.get(name)
    if provider is None:
        raise LookupError(
            f"the Python module `{registry.distribution}` supplies no table provider "
            f"`{name}`"
        )
    return provider


def _provider_call(payload, method, arguments, required=True):
    provider = _provider(payload)
    instance = provider.instance()
    fn = getattr(instance, method, None)
    if fn is None:
        if not required:
            return None
        raise TypeError(
            f"the table provider `{provider.name}` has no `{method}`, which every "
            f"provider must answer"
        )
    available = dict(arguments)
    available["configuration"] = payload.get("configuration") or {}
    return fn(**_selected(fn, available, f"the table provider `{provider.name}`"))


def op_provider_fields(payload):
    """The columns this provider presents for one configuration."""
    declared = _provider_call(payload, "fields", {}) or []
    return [field.to_json() if isinstance(field, Field) else field for field in declared]


def op_provider_rows(payload):
    rows = _provider_call(
        payload,
        "rows",
        {
            "where": payload.get("where") or {},
            "options": payload.get("options") or {},
            "table": payload.get("table"),
        },
    )
    return list(rows or [])


def op_provider_writes(payload):
    """Which writes this provider answers — by which methods it **defines**."""
    instance = _provider(payload).instance()
    return {
        "insert": callable(getattr(instance, "insert_row", None)),
        "update": callable(getattr(instance, "update_row", None)),
        "delete": callable(getattr(instance, "delete_rows", None)),
    }


def op_provider_insert(payload):
    key = _provider_call(payload, "insert_row", {"record": payload.get("record")})
    return {"key": key}


def op_provider_update(payload):
    _provider_call(
        payload,
        "update_row",
        {"record": payload.get("record"), "id": payload.get("id")},
    )
    return None


def op_provider_delete(payload):
    _provider_call(payload, "delete_rows", {"where": payload.get("where") or {}})
    return None


def _model_provider(payload):
    registry = _require(payload["module"])
    name = payload["provider"]
    provider = registry.model_providers.get(name)
    if provider is None:
        raise LookupError(
            f"the Python module `{registry.distribution}` supplies no model provider "
            f"`{name}`"
        )
    return provider


def _model_call(payload, method, arguments):
    provider = _model_provider(payload)
    fn = getattr(provider.instance(), method, None)
    if fn is None:
        raise TypeError(
            f"the model provider `{provider.name}` has no `{method}`, which every "
            f"model provider must answer"
        )
    return fn(**_selected(fn, arguments, f"the model provider `{provider.name}`"))


def op_model_fit(payload):
    """Fit one model provider over a columnar frame.

    A provider that answers a bare value rather than the pair is read as having
    answered its state: ``state`` is the half without which nothing can predict,
    and ``parameters`` is the half a screen shows.
    """
    with warnings.catch_warnings(record=True) as caught:
        # "always", or a warning Python has already shown once in this
        # process would be missing from the second fit that raised it.
        warnings.simplefilter("always")
        result = _model_call(
            payload,
            "fit",
            {
                "frame": Frame(payload.get("frame")),
                "configuration": payload.get("configuration") or {},
                "hyperparameters": payload.get("hyperparameters") or {},
            },
        )
    if isinstance(result, dict) and "state" in result:
        answer = {
            "state": result.get("state"),
            "parameters": list(result.get("parameters") or ()),
            "warnings": [str(w) for w in result.get("warnings") or ()],
        }
        # A frame of the provider's own per name, for the plots it declares
        # (analytics TODO A3.2), in the frame's JSON.
        if result.get("outputs"):
            answer["outputs"] = dict(result["outputs"])
    else:
        answer = {"state": result, "parameters": [], "warnings": []}
    for w in caught:
        sentence = f"{w.category.__name__}: {w.message}"
        if sentence not in answer["warnings"]:
            answer["warnings"].append(sentence)
    return answer


def op_model_predict(payload):
    """Predict with one, over a frame of any height — a row is a frame of one."""
    answer = _model_call(
        payload,
        "predict",
        {"state": payload.get("state"), "frame": Frame(payload.get("frame"))},
    )
    if answer is None:
        return []
    # A numpy array is not a list and is not JSON; `tolist()` is what makes it
    # one, and a provider should not have to remember to call it.
    tolist = getattr(answer, "tolist", None)
    if callable(tolist):
        answer = tolist()
    return [_prediction(value) for value in answer]


def _prediction(value):
    """One prediction, as JSON the host reads.

    Bare numbers, strings and lists pass through — the host reads those as a
    regression's answer, a class and a vector — and anything numpy-shaped is
    reduced to one of them first, so a provider does not have to.
    """
    if isinstance(value, dict):
        return value
    if isinstance(value, bool):
        return str(value)
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        return value
    item = getattr(value, "item", None)
    if callable(item):
        try:
            return _prediction(item())
        except (ValueError, TypeError):
            pass
    tolist = getattr(value, "tolist", None)
    if callable(tolist):
        return _prediction(tolist())
    if isinstance(value, (list, tuple)):
        return [float(v) for v in value]
    raise TypeError(f"a model provider answered a prediction this host cannot read: {value!r}")


_OPS = {
    "load": op_load,
    "unload": op_unload,
    "action": op_action,
    "function": op_function,
    "provider_fields": op_provider_fields,
    "provider_rows": op_provider_rows,
    "provider_writes": op_provider_writes,
    "provider_insert": op_provider_insert,
    "provider_update": op_provider_update,
    "provider_delete": op_provider_delete,
    "model_fit": op_model_fit,
    "model_predict": op_model_predict,
}


def dispatch(op, payload):
    """The one entry point the host calls, for every op it has."""
    handler = _OPS.get(op)
    if handler is None:
        # Unreachable from the Rust side, which spells all of them.
        raise LookupError(f"`{op}` is not something a Python module can be asked")
    return handler(payload)
