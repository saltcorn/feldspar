"""The surface an app builder writes: five handles, and the errors they raise.

``db`` the tables, ``fetch`` one HTTP request, ``fs`` the file stores,
``trigger`` this server's other triggers, ``modfn`` the functions its modules
supply — and nothing else, because those are the five host traits ``sc-expr``
has.

This is the counterpart of the five JavaScript preludes, and it is Python for
the reason they are JavaScript: the Rust side sees **plans**, so adding a chain
method or a file operation touches no Rust, and the two languages cannot drift
apart about what one means because they lower to the same object and it is
resolved in one place.

It is shipped inside the binary and installed by a meta-path loader at
interpreter start, so there is no file to find, no version to skew and nothing
to ``pip install`` for the surface itself.

Chain methods are **pure and cheap** — each answers a new query and touches
nothing — and terminals execute::

    overdue = (db.invoices
        .where(paid=False, due__lt=payload["today"])
        .select("id", "amount", "customerⱵemail")
        .order_by("due")
        .limit(50)
        .rows())

Nothing is awaited, on any of the five. A terminal blocks the run's own thread
with the GIL released, so every other resident run executes while this one
waits::

    res = fetch(payload["url"], headers={"authorization": token})
    fs("uploads").open("rates/today.json").write(res.json())
    trigger("recalculate").run(day=payload["today"])

Two kinds of error, kept apart on purpose. A mistake in what the *body* wrote —
an operator that is not one, a path that climbs out of its store, a ``data=``
that should have been ``json=`` — is a ``TypeError`` or a ``ValueError``, which
is what a Python author expects of a library. A **refusal** — an ownership rule
that would not allow a write, a budget spent, a store this server does not have
— is the surface's own error (``DbError``, ``FileError``, …), because it is the
same refusal the host makes and a body may legitimately catch it and fall back.

A status an endpoint did not like is neither: ``res.ok`` is False and nothing
raises, which is what makes a retry or a fallback something a body writes rather
than a trigger that failed.

The same package is what an installed **plugin** declares itself with —
``sc.settings``, ``@sc.on_load``, ``@sc.action``, ``@sc.function``,
``@sc.table_provider``, ``@sc.model_provider`` and ``sc.Field`` — and a plugin's code reaches the five
handles above exactly as a body does, bound for the duration of a call and
raising outside one. That half lives in ``plugin.py`` and is re-exported here,
so an author writes ``sc.`` and never names it.
"""

import base64 as _b64
import json as _json
import re

# The seam: the host functions and the exception hierarchy, in a built-in module
# the interpreter was started with. Bound here at module level, where Python's
# private-name mangling does not apply — inside a class body `__sc.__sc_db`
# would silently become `__sc._Query__sc_db`.
from __sc import __sc_db as _call_db
from __sc import __sc_fetch as _call_fetch
from __sc import __sc_fs as _call_fs
from __sc import __sc_modfn as _call_modfn
from __sc import __sc_names as _call_names
from __sc import __sc_trigger as _call_trigger
from __sc import (
    DbError,
    FetchError,
    FileError,
    ModuleError,
    SaltcornError,
    Timeout,
    TriggerError,
)

# The **plugin** half of this package (§2 of the API): what an installed
# distribution declares itself with. Re-exported rather than defined here
# because it is a different conversation — an author writes `@sc.action` and the
# host reads the registry those decorators fill — and because a code body, which
# is what everything above is for, never touches it.
from __sc_plugin import (
    Field,
    Frame,
    Outcome,
    Parameter,
    Prediction,
    action,
    function,
    model_provider,
    on_load,
    settings,
    table_provider,
)

__all__ = [
    "Db",
    "DbError",
    "Dir",
    "FetchError",
    "Field",
    "File",
    "FileError",
    "Frame",
    "Fs",
    "Headers",
    "ModFns",
    "Models",
    "ModuleError",
    "ModuleFunctions",
    "Outcome",
    "Parameter",
    "Prediction",
    "Query",
    "Response",
    "SaltcornError",
    "Store",
    "Timeout",
    "TriggerError",
    "TriggerHandle",
    "Triggers",
    "action",
    "and_",
    "db",
    "fetch",
    "fs",
    "function",
    "modfn",
    "model_provider",
    "models",
    "not_",
    "on_load",
    "or_",
    "settings",
    "table_provider",
    "trigger",
]

#: What a filter may say about one column — the vocabulary every other surface
#: in this system speaks, spelled as a keyword suffix: ``due__lt=today``.
OPERATORS = (
    "eq",
    "ne",
    "gt",
    "gte",
    "lt",
    "lte",
    "in",
    "nin",
    "like",
    "ilike",
    "is_null",
)

#: The key the **formula** spelling of a filter rides under.
_FORMULA = "formula"

#: An aggregate is written the way a formula writes one — ``count()``,
#: ``sum(price * qty)`` — and lowers to the plan's own ``{alias, fn, arg}``.
_AGGREGATE = re.compile(r"^\s*([A-Za-z_][A-Za-z_0-9]*)\s*\(([\s\S]*)\)\s*$")


def _condition(value):
    """One filter, in either of the two spellings §3 gives."""
    if isinstance(value, str):
        return {_FORMULA: value}
    if isinstance(value, dict):
        return value
    raise TypeError(
        "a condition is keywords (paid=False), a filter dict "
        "({'due': {'lt': '2026-01-01'}}) or a formula string, not a "
        f"{type(value).__name__}"
    )


def _split_operator(key):
    """``due__lt`` as ``("due", "lt")``; a bare name as ``(name, None)``.

    Only a trailing ``__<op>`` naming one of :data:`OPERATORS` is read as an
    operator, so a column whose own name ends that way is reached with the dict
    spelling instead — which can say everything the keyword one can.
    """
    for op in OPERATORS:
        suffix = "__" + op
        if key.endswith(suffix) and len(key) > len(suffix):
            return key[: -len(suffix)], op
    return key, None


def _keyword_condition(kwargs):
    """``.where(paid=False, due__lt=x)`` as one filter object.

    Flat while the fields are distinct, which is the common plan and the one a
    reader of the plan recognises; two comparisons on **one** field cannot share
    a key, so those become an explicit ``and``.
    """
    flat = {}
    extra = []
    for key, value in kwargs.items():
        field, op = _split_operator(key)
        term = value if op is None else {op: value}
        if field in flat:
            extra.append({field: term})
        else:
            flat[field] = term
    if not extra:
        return flat
    return {"and": [{field: term} for field, term in flat.items()] + extra}


def and_(*conditions):
    """Every one of these — what several ``.where()`` calls already mean."""
    return {"and": [_condition(c) for c in conditions]}


def or_(*conditions):
    """Any one of these."""
    return {"or": [_condition(c) for c in conditions]}


def not_(condition):
    """The opposite of this one."""
    return {"not": _condition(condition)}


def _projections(value):
    """One argument of ``.select()`` as the plan's projections."""
    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        return [
            {"alias": alias, _FORMULA: formula} for alias, formula in value.items()
        ]
    raise TypeError(
        "select() takes field names, Ⱶ-paths and alias=\"formula\" keywords, "
        f"not a {type(value).__name__}"
    )


def _aggregates(spec):
    """``.aggregate(total="sum(amount)")`` as the plan's own ``{alias, fn, arg}``."""
    out = []
    for alias, source in spec.items():
        found = _AGGREGATE.match(str(source))
        if found is None:
            raise ValueError(
                f"`{source}` is not an aggregate: write count(), sum(field) or "
                "sum(an expression)"
            )
        arg = found.group(2).strip()
        out.append({"alias": alias, "fn": found.group(1), "arg": arg or None})
    return out


def _direction(dir):
    if dir is None:
        return "asc"
    if isinstance(dir, str) and dir.lower() in ("asc", "desc"):
        return dir.lower()
    raise ValueError(f'order_by() sorts "asc" or "desc", not `{dir}`')


def _whole_number(value, what):
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{what} takes a whole number of rows, e.g. .{what}(50)")
    return value


class Query:
    """One table, and what has been said about it so far.

    Immutable: every chain method answers a **new** query, so a query held in a
    variable can be narrowed two ways without either narrowing the other, and a
    ``for`` loop over one may be walked twice.
    """

    __slots__ = (
        "_table",
        "_authority",
        "_where",
        "_select",
        "_order",
        "_group",
        "_having",
        "_aggregate",
        "_limit",
        "_offset",
    )

    def __init__(self, table, authority):
        self._table = table
        self._authority = authority
        self._where = ()
        self._select = ()
        self._order = ()
        self._group = ()
        self._having = ()
        self._aggregate = ()
        self._limit = None
        self._offset = None

    # -- the chain, which touches nothing ---------------------------------

    def _derive(self, **patch):
        clone = Query(self._table, self._authority)
        for slot in Query.__slots__:
            setattr(clone, slot, getattr(self, slot))
        for name, value in patch.items():
            setattr(clone, "_" + name, value)
        return clone

    def where(self, *conditions, **kwargs):
        """Narrow the rows. Several conditions, and several calls, are ANDed."""
        added = [_condition(c) for c in conditions]
        if kwargs:
            added.append(_keyword_condition(kwargs))
        if not added:
            raise TypeError(
                "where() needs a condition: keywords (paid=False), a filter "
                "dict, or a formula string"
            )
        return self._derive(where=self._where + tuple(added))

    def select(self, *columns, **aliased):
        """Which values to answer: columns, Ⱶ-paths, and aliased formulas."""
        added = []
        for column in columns:
            added.extend(_projections(column))
        added.extend(_projections(aliased))
        return self._derive(select=self._select + tuple(added))

    def order_by(self, field, dir=None):
        """Sort by `field`, ascending unless told otherwise."""
        return self._derive(
            order=self._order + ({"field": field, "dir": _direction(dir)},)
        )

    def group_by(self, *fields):
        """One answer row per distinct combination of these."""
        return self._derive(group=self._group + tuple(fields))

    def aggregate(self, **spec):
        """The aggregate values a group answers: ``total="sum(amount)"``."""
        return self._derive(aggregate=self._aggregate + tuple(_aggregates(spec)))

    def having(self, *conditions, **kwargs):
        """Narrow the **groups**, by this aggregate's own aliases."""
        added = [_condition(c) for c in conditions]
        if kwargs:
            added.append(_keyword_condition(kwargs))
        if not added:
            raise TypeError("having() needs a condition")
        return self._derive(having=self._having + tuple(added))

    def limit(self, rows):
        """At most this many rows."""
        return self._derive(limit=_whole_number(rows, "limit"))

    def offset(self, rows):
        """Skip this many first."""
        return self._derive(offset=_whole_number(rows, "offset"))

    def as_user(self):
        """Run under the event's **caller**, where §7.3's ownership rule decides."""
        return self._derive(authority="user")

    def as_admin(self):
        """Run under the trigger's own authority — the default."""
        return self._derive(authority="admin")

    # -- the plan ----------------------------------------------------------

    def _plan(self, op, **extra):
        plan = {"op": op, "table": self._table, "authority": self._authority}
        # Repeated .where() calls AND; one is itself, so the common plan is flat.
        if len(self._where) == 1:
            plan["where"] = self._where[0]
        elif self._where:
            plan["where"] = {"and": list(self._where)}
        if self._select:
            plan["select"] = list(self._select)
        if self._order:
            plan["order"] = list(self._order)
        if self._group:
            plan["group"] = list(self._group)
        if len(self._having) == 1:
            plan["having"] = self._having[0]
        elif self._having:
            plan["having"] = {"and": list(self._having)}
        if self._aggregate:
            plan["aggregate"] = list(self._aggregate)
        if self._limit is not None:
            plan["limit"] = self._limit
        if self._offset is not None:
            plan["offset"] = self._offset
        plan.update(extra)
        return plan

    def _bounded(self, op):
        """A whole table rewritten or emptied is not something an omitted call
        should be able to cause. The host refuses it too; here it is named at the
        place the author can see."""
        if not self._where:
            raise DbError(
                f"db.{self._table}.{op}() without a .where() would touch every "
                "row; add a .where()"
            )

    def _read(self, **extra):
        """What a terminal reads: the rows of a select, or the groups of an
        aggregate — which is one object when there is nothing to group by."""
        if not self._aggregate:
            if self._group:
                raise ValueError(
                    f"db.{self._table}.group_by(...) needs an .aggregate(...): a "
                    "group answers aggregate values, so say which"
                )
            return _call_db(self._plan("select", **extra))
        answer = _call_db(self._plan("aggregate", **extra))
        return answer if isinstance(answer, list) else [answer]

    def _scalar(self, fn, arg=None):
        """A scalar terminal is one nameless group: the same plan, the one value
        unwrapped. Grouped, there is no one value to unwrap, so it says so rather
        than answering the first group's."""
        if self._group or self._aggregate:
            raise ValueError(
                f".{fn}() answers one value, and this query groups; ask for it by "
                f'name: .aggregate({fn}="{fn}({arg or ""})").rows()'
            )
        answer = _call_db(
            self._plan("aggregate", aggregate=[{"alias": "value", "fn": fn, "arg": arg}])
        )
        if not isinstance(answer, dict):
            return None
        return answer.get("value")

    # -- the terminals, which execute --------------------------------------

    def rows(self):
        """Every matching row, as a ``list`` of ``dict``."""
        return self._read()

    def iter(self, batch=None):
        """The rows, **one batch per host call**, as a generator.

        The interpreter holds one batch rather than the whole answer, so a body
        can walk a table it could never fit in memory — and a body that stops
        early has paid for only what it read, because nothing is fetched until
        the loop asks for it. What bounds it is the call budget rather than the
        row cap.

        The order is the host's business: it appends the primary key to whatever
        this query sorts by, so no two rows tie and no batch boundary can skip or
        repeat one. A ``.limit()`` bounds the **iteration** and is spent here, by
        stopping; an ``.offset()`` skips rows once, at the start.
        """
        if self._aggregate or self._group:
            raise ValueError(
                f"db.{self._table}.iter() streams rows, and this query aggregates "
                "them; ask for the groups with .rows(), which answers them all at "
                "once"
            )
        if batch is not None and (
            isinstance(batch, bool) or not isinstance(batch, int) or batch < 1
        ):
            raise ValueError(
                "iter()'s argument is how many rows to read at a time, e.g. "
                ".iter(200)"
            )
        total = self._limit
        taken = 0
        after = None
        while True:
            want = batch
            if total is not None:
                left = total - taken
                if left <= 0:
                    return
                if want is None or left < want:
                    want = left
            plan = self._plan("select", cursor=True)
            if want is not None:
                plan["limit"] = want
            if after is not None:
                plan["after"] = after
                # The host refuses a resumed batch that carries an offset, which
                # is the same rule said where a guest cannot reach it.
                plan.pop("offset", None)
            answer = _call_db(plan)
            for row in answer["rows"]:
                yield row
                taken += 1
                if total is not None and taken >= total:
                    return
            if answer.get("cursor") is None:
                return
            after = answer["cursor"]

    def __iter__(self):
        """``for row in db.books.where(...)`` — the same walk :meth:`iter` is."""
        return self.iter()

    def first(self):
        """The first matching row, or ``None``."""
        found = self._read(limit=1)
        return found[0] if found else None

    def get(self, pk):
        """The row with this primary key, or ``None``."""
        found = _call_db(self._plan("select", pk=pk, limit=1))
        return found[0] if found else None

    def exists(self):
        """Whether anything matches."""
        return len(_call_db(self._plan("select", limit=1))) > 0

    def count(self):
        """How many rows match."""
        return self._scalar("count")

    def sum(self, field):
        """The total of `field` over the matching rows."""
        return self._scalar("sum", field)

    def avg(self, field):
        """The mean of `field` over the matching rows."""
        return self._scalar("avg", field)

    def min(self, field):
        """The least `field` among the matching rows."""
        return self._scalar("min", field)

    def max(self, field):
        """The greatest `field` among the matching rows."""
        return self._scalar("max", field)

    def insert(self, values=None, **fields):
        """Write a row, or a list of rows.

        ``insert(title="Dune")``, ``insert({"title": "Dune"})`` and
        ``insert([{...}, {...}])`` are the same call spelled three ways. The
        write goes through the row layer, so it is coerced against its columns,
        validated, and **observed by triggers** exactly as an API caller's write
        is.
        """
        if values is not None and fields:
            raise TypeError(
                "insert() takes the row as keywords or as one dict, not both"
            )
        row = fields if values is None else values
        if not isinstance(row, (dict, list, tuple)):
            raise TypeError(
                "insert() takes keywords, a dict, or a list of dicts, not a "
                f"{type(row).__name__}"
            )
        if isinstance(row, tuple):
            row = list(row)
        return _call_db(self._plan("insert", values=row))

    def update(self, values=None, **fields):
        """Change every matching row. **Refused without a** ``.where()``."""
        if values is not None and fields:
            raise TypeError(
                "update() takes the assignments as keywords or as one dict, not both"
            )
        assignments = fields if values is None else values
        if not isinstance(assignments, dict):
            raise TypeError(
                "update() takes keywords or a dict of assignments, not a "
                f"{type(assignments).__name__}"
            )
        self._bounded("update")
        return _call_db(self._plan("update", values=assignments))

    def delete(self):
        """Remove every matching row. **Refused without a** ``.where()``."""
        self._bounded("delete")
        return _call_db(self._plan("delete"))

    def __repr__(self):
        return f"<saltcorn query on `{self._table}` as {self._authority}>"


class Db:
    """The tables, under one authority.

    ``db.invoices`` is a table and ``db.table("customer orders")`` is the same
    thing for a name that is not an identifier — so a table called ``table``,
    ``sql``, ``as_user`` or ``as_admin`` is reached the second way, the four
    names this handle has of its own being the only ones attribute access does
    not answer with a query.
    """

    __slots__ = ("_authority",)

    def __init__(self, authority="admin"):
        self._authority = authority

    def table(self, name):
        """The table called `name`, whatever it is called."""
        return Query(name, self._authority)

    def as_user(self):
        """Delegate to the event's **caller**: §7.3's ownership rule then decides
        every row, and a refusal is a catchable :class:`DbError`."""
        return Db("user")

    def as_admin(self):
        """The trigger's own authority — the default, because a trigger is
        server-side configuration and the audit row a caller may not insert is
        the archetype of what a trigger exists to write."""
        return Db("admin")

    def sql(self, sql, params=None, as_user=None):
        """The body's own SQL, for the question the chain does not ask — a window
        function, a recursive CTE, an ``ON CONFLICT``::

            ranked = db.sql(
                "select owner, title, rank() over (partition by owner "
                "order by pages desc) as r from books where pages > $1",
                [200],
            )

        The text is the author's and runs as written; the values are **binds**
        and never part of it. It is the same admission a custom SQL query is, and
        it carries the same consequences: raw SQL does not go through the row
        layer, so no ownership formula filters it, no rich type coerces it, and a
        write inside one **raises no table event**. The row cap, the call budget
        and the caller's transaction all still apply.
        """
        if not isinstance(sql, str):
            raise TypeError('sql() takes the SQL text, e.g. db.sql("select 1 as n")')
        if params is None:
            params = []
        elif isinstance(params, tuple):
            params = list(params)
        elif not isinstance(params, list):
            raise TypeError(
                "sql()'s second argument is the list of values its placeholders "
                "stand for"
            )
        if as_user is None:
            authority = self._authority
        else:
            authority = "user" if as_user else "admin"
        return _call_db(
            {"op": "sql", "authority": authority, "sql": sql, "params": params}
        )

    def __getattr__(self, name):
        # Only reached for names this handle does not have of its own. A private
        # or dunder name is never a table: answering `copy.copy` or
        # `__deepcopy__` with a query would make every such protocol think this
        # object supports it.
        if name.startswith("_"):
            raise AttributeError(name)
        return Query(name, self._authority)

    def __repr__(self):
        return f"<saltcorn db as {self._authority}>"

# ---------------------------------------------------------------------------
# `fetch` — one HTTP request
# ---------------------------------------------------------------------------
#
# Shaped like `requests`, because that is the Python every author already
# knows: `fetch(url, headers=..., json=...)` answers a response whose `.text`
# and `.json()` read like `requests`', and a status the endpoint did not like is
# **not** an exception — `res.ok` is False and nothing raises. Only a transport
# failure raises, as a `FetchError`.
#
# What is not `requests`': there is no session, no streaming and no `verify=` —
# the seam carries one JSON value and the client is the server's own — and the
# whole request is bounded by what is left of this run's wall clock, whatever
# `timeout` says.

#: The methods a body may send, upper-cased before the check. `CONNECT` and
#: `TRACE` are absent for the reason the host leaves them out: a proxy or an echo
#: of this server's request headers is a way to use the server as a tool rather
#: than a way to call an endpoint.
_METHODS = ("GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS")

#: A header name, as RFC 9110 spells a token. Checked here as well as in the
#: host so the message can name the line that wrote it.
_HEADER_NAME = re.compile(r"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$")

#: "Nothing was passed", as distinct from `None` — which for `json=` is the
#: value `null` and a request an author may legitimately want to send.
_MISSING = object()


def _header_name(name):
    text = str(name)
    if _HEADER_NAME.match(text) is None:
        raise TypeError(f"`{text}` is not a valid HTTP header name")
    return text.lower()


def _header_value(value):
    text = str(value).strip(" \t\r\n")
    if "\0" in text or "\r" in text or "\n" in text:
        raise TypeError("an HTTP header value may not contain a newline")
    return text


class Headers:
    """HTTP headers, read case-insensitively.

    A repeated header stays two headers — which is the whole difference for
    ``set-cookie``, where joining with a comma is wrong — and :meth:`get` joins
    them with ``", "``, as every other header API does.
    """

    __slots__ = ("_pairs",)

    def __init__(self, init=None):
        self._pairs = []
        if init is None:
            return
        if isinstance(init, Headers):
            items = list(init._pairs)
        elif isinstance(init, dict):
            items = list(init.items())
        else:
            try:
                items = [tuple(pair) for pair in init]
            except TypeError:
                raise TypeError(
                    "headers are a dict, [name, value] pairs, or Headers, not a "
                    f"{type(init).__name__}"
                ) from None
            for pair in items:
                if len(pair) != 2:
                    raise TypeError("headers take [name, value] pairs")
        for name, value in items:
            self._pairs.append((_header_name(name), _header_value(value)))

    def get(self, name, default=None):
        """This header's value, with repeats joined by ``", "``."""
        key = _header_name(name)
        found = [value for header, value in self._pairs if header == key]
        return ", ".join(found) if found else default

    def __getitem__(self, name):
        found = self.get(name, _MISSING)
        if found is _MISSING:
            raise KeyError(name)
        return found

    def __contains__(self, name):
        try:
            key = _header_name(name)
        except TypeError:
            return False
        return any(header == key for header, _ in self._pairs)

    def keys(self):
        """Each header name once, in the order it first arrived."""
        seen = []
        for name, _ in self._pairs:
            if name not in seen:
                seen.append(name)
        return seen

    def values(self):
        return [self.get(name) for name in self.keys()]

    def items(self):
        return [(name, self.get(name)) for name in self.keys()]

    def __iter__(self):
        return iter(self.keys())

    def __len__(self):
        return len(self.keys())

    def append(self, name, value):
        """Add a header, keeping any of that name already here."""
        self._pairs.append((_header_name(name), _header_value(value)))

    def set_default(self, name, value):
        """Add a header only where the caller has not set one of that name."""
        if name not in self:
            self.append(name, value)

    def pairs(self):
        """The pairs **as written**, uncombined — what crosses the seam."""
        return [[name, value] for name, value in self._pairs]

    def __repr__(self):
        return f"<saltcorn headers {self.items()!r}>"


class Response:
    """What :func:`fetch` answers: the status, the headers and the body.

    ``.text`` and ``.content`` are **properties**, as ``requests``' are, because
    the body arrived with the response — there is nothing left to wait for.
    """

    __slots__ = (
        "status",
        "status_text",
        "url",
        "redirected",
        "headers",
        "_text",
        "_base64",
    )

    def __init__(self, answer):
        self.status = answer.get("status", 0)
        self.status_text = answer.get("status_text") or ""
        self.url = answer.get("url") or ""
        self.redirected = answer.get("redirected") is True
        self.headers = Headers(answer.get("headers"))
        self._text = answer.get("text")
        self._base64 = answer.get("base64")

    @property
    def ok(self):
        """Whether the status is a 2xx. A 404 is a value, not an exception."""
        return 200 <= self.status < 300

    @property
    def text(self):
        """The body as text — lossily decoded when the bytes are not UTF-8,
        exactly as a browser's ``res.text()`` is."""
        return "" if self._text is None else self._text

    @property
    def content(self):
        """The body as ``bytes``.

        The host sends the text of everything it could read as text and the
        base64 as well when the bytes are not valid UTF-8, so a JSON or HTML
        answer crosses the seam once and a PNG crosses twice.
        """
        if self._base64 is not None:
            return _b64.b64decode(self._base64)
        return self.text.encode("utf-8")

    def json(self):
        """The body parsed as JSON."""
        try:
            return _json.loads(self.text)
        except ValueError as e:
            raise ValueError(f"the response body is not JSON: {e}") from None

    def raise_for_status(self):
        """Raise :class:`FetchError` unless the status is a 2xx; answer self."""
        if not self.ok:
            where = f" from {self.url}" if self.url else ""
            raise FetchError(
                f"the endpoint answered {self.status} {self.status_text}".rstrip()
                + where
            )
        return self

    def __repr__(self):
        return f"<saltcorn response {self.status} from {self.url}>"


def fetch(
    url,
    method="GET",
    *,
    headers=None,
    json=_MISSING,
    data=None,
    timeout=None,
    timeout_ms=None,
):
    """Call one endpoint, and answer a :class:`Response`.

    ``json=`` sends a value as JSON and sets the content type; ``data=`` sends a
    ``str`` or ``bytes`` as they are. A ``dict`` in ``data=`` is refused rather
    than form-encoded, because ``requests``' two meanings for one argument are
    exactly the mistake this surface should not carry over.

    **Two spellings of the clock, both bounded.** ``timeout=`` is ``requests``'
    and is in **seconds**; ``timeout_ms=`` is this system's and is in
    milliseconds. Whichever is given is clamped to what is left of this code's
    own time limit, so a request may only ever be shorter than the run.
    """
    if not isinstance(url, str) or url.strip() == "":
        raise TypeError("fetch() takes an absolute http(s) URL as its first argument")
    name = str(method).upper()
    if name not in _METHODS:
        raise ValueError(
            f"`{name}` is not a method fetch() sends; the methods are: "
            + ", ".join(_METHODS)
        )
    head = Headers(headers)
    body, base64 = _fetch_body(name, head, json, data)
    return Response(
        _call_fetch(
            {
                "url": url.strip(),
                "method": name,
                "headers": head.pairs(),
                "body": body,
                "body_base64": base64,
                "timeout_ms": _fetch_timeout(timeout, timeout_ms),
            }
        )
    )


def _fetch_body(method, headers, json, data):
    """The request body as it crosses the seam: text, and whether it is base64.

    ``data=None`` is ``requests``' "no body"; ``json=None`` is the value ``null``,
    which is why only one of the two has a sentinel. The content type is set only
    where the caller did not: a body that named one means it.
    """
    if json is not _MISSING and data is not None:
        raise TypeError("fetch() takes `json=` or `data=`, not both")
    if json is _MISSING and data is None:
        return None, False
    if method in ("GET", "HEAD"):
        raise TypeError(f"a {method} request cannot carry a body")
    if json is not _MISSING:
        try:
            text = _json.dumps(json)
        except (TypeError, ValueError) as e:
            raise TypeError(
                f"fetch()'s `json=` could not be encoded as JSON: {e}"
            ) from None
        headers.set_default("content-type", "application/json")
        return text, False
    if isinstance(data, str):
        headers.set_default("content-type", "text/plain;charset=UTF-8")
        return data, False
    if isinstance(data, (bytes, bytearray, memoryview)):
        headers.set_default("content-type", "application/octet-stream")
        return _b64.b64encode(bytes(data)).decode("ascii"), True
    # `requests` would form-encode this. Refused rather than guessed: an object
    # that reached an endpoint as `a=1&b=2` when JSON was meant is a bug that
    # looks like a working request.
    raise TypeError(
        "fetch()'s `data=` takes a str or bytes; send a dict or a list with "
        f"`json=`, not a {type(data).__name__}"
    )


def _fetch_timeout(timeout, timeout_ms):
    """One clock in milliseconds, from either spelling."""
    if timeout is not None and timeout_ms is not None:
        raise TypeError(
            "fetch() takes `timeout=` (seconds) or `timeout_ms=` (milliseconds), "
            "not both"
        )
    if timeout_ms is not None:
        ms = timeout_ms
    elif timeout is not None:
        ms = timeout * 1000
    else:
        return None
    if isinstance(ms, bool) or not isinstance(ms, (int, float)):
        raise TypeError("fetch()'s timeout is a number, e.g. timeout=5")
    if ms <= 0:
        raise ValueError("fetch()'s timeout must be more than zero")
    return int(ms)


# ---------------------------------------------------------------------------
# `fs` — the file stores
# ---------------------------------------------------------------------------
#
# `fs("uploads")` is a store, `.open(path)` a file **reference** and `.dir(path)`
# a directory one — no I/O, and the path need not exist. Everything that touches
# the store is a method on the reference, and the vocabulary is `pathlib`'s
# where `pathlib` has one: `read_text`, `write`, `exists`, `iterdir`.
#
# Creating is not a second concept, which is the answer to "what replaces a
# `write(path, data)` free function": a reference that can be read can be
# written, and the parent directories are made on the way. `write` replaces what
# is there, `create` refuses to.
#
# What crosses is one JSON operation per method that touches the store, and the
# *path handling* is here as well as in the host: `..`, a null byte and an
# absolute path are refused here, where the message can name the line that wrote
# them, and refused again there, which trusts nothing it is sent.


def _fs_path(path, what):
    """A store-relative path: ``/``-separated, and confined to the store."""
    if not isinstance(path, str):
        raise TypeError(f"{what} takes a path, as a string")
    if "\0" in path:
        raise TypeError("a file path may not contain a null byte")
    if path[:1] in ("/", "\\"):
        raise TypeError(
            f"`{path}` is an absolute path; a file store's paths are relative to "
            "its root"
        )
    parts = []
    for segment in path.split("/"):
        if segment in ("", "."):
            continue
        if segment == "..":
            # Refused rather than resolved: a path that climbs out is either a
            # bug or an attempt, and neither is served by clamping it at the root.
            raise TypeError(
                f"`{path}` leaves the file store: `..` is not a path segment here"
            )
        parts.append(segment)
    return "/".join(parts)


def _fs_named(path, what):
    """The same, for somewhere the store root is not an answer."""
    clean = _fs_path(path, what)
    if clean == "":
        raise ValueError(f"{what} needs a name, not the store root")
    return clean


def _fs_join(directory, rest):
    return rest if directory == "" else directory + "/" + rest


def _fs_send(store, plan):
    """One operation, carrying what every operation carries: the store's name and
    whose authority it runs under."""
    request = {"store": store.name, "authority": store.authority}
    request.update(plan)
    return _call_fs(request)


class Fs:
    """``fs("uploads")`` — the file stores this run may reach.

    Callable rather than a mapping because a store is *named*, not indexed, and
    because ``fs.stores`` should be the list of names rather than something
    ``in`` has an opinion about.
    """

    __slots__ = ()

    def __call__(self, name):
        if not isinstance(name, str) or name == "":
            raise TypeError('fs() takes the name of a file store, as in fs("uploads")')
        known = _call_names("stores")
        # Named at once rather than at the first read: the names came with this
        # run, so a typo is a sentence naming the stores that do exist rather
        # than an error four lines later. An empty list is "no list to check
        # against" — a host that cannot enumerate answers with one — and every
        # name then goes through to be decided by the operation itself.
        if known and name not in known:
            raise FileError(
                f"there is no file store named `{name}`; this server has: "
                + ", ".join(known)
            )
        return Store(name, "admin")

    @property
    def stores(self):
        """What this run can reach, for a body that discovers rather than knows."""
        return tuple(_call_names("stores"))

    def __repr__(self):
        return "<saltcorn fs>"


class Store:
    """One file store, under one authority."""

    __slots__ = ("name", "authority")

    def __init__(self, name, authority):
        self.name = name
        self.authority = authority

    def open(self, path):
        """The file at `path`. No I/O: the path need not exist."""
        return File(self, _fs_named(path, "open()"))

    def dir(self, path):
        """The directory at `path`; ``dir("")`` is the store root."""
        return Dir(self, _fs_path(path, "dir()"))

    @property
    def root(self):
        return Dir(self, "")

    def as_user(self):
        """Delegate to the event's **caller**, where §14.1's path-cumulative
        ``min_role`` rule decides every operation."""
        return Store(self.name, "user")

    def as_admin(self):
        """The trigger's own authority — the default."""
        return Store(self.name, "admin")

    def __repr__(self):
        return f"<saltcorn file store `{self.name}` as {self.authority}>"


class _Entry:
    """What a file and a directory have in common: where they are, and the four
    operations that do not care which they are."""

    __slots__ = ("_store", "_path")

    def __init__(self, store, path):
        self._store = store
        self._path = path

    @property
    def path(self):
        return self._path

    @property
    def name(self):
        return self._path.rsplit("/", 1)[-1]

    @property
    def store(self):
        return self._store

    def _send(self, plan):
        return _fs_send(self._store, plan)

    def stat(self):
        """The entry's own facts, or ``None`` when nothing is there."""
        found = self._send({"op": "stat", "path": self._path})
        if found is None:
            return None
        return {
            "size": found.get("size"),
            "is_directory": found.get("isDirectory"),
            "modified": found.get("modified"),
            "mime_type": found.get("mimeType"),
        }

    def delete(self):
        """Remove it, answering whether anything was there."""
        return self._send({"op": "delete", "path": self._path})

    def meta(self):
        """The rule set here, the rule that **applies** given every directory
        above, and the free-form attributes."""
        found = self._send({"op": "meta", "path": self._path})
        return {
            "min_role": found.get("minRole"),
            "effective_min_role": found.get("effectiveMinRole"),
            "attributes": found.get("attributes"),
        }

    def set_meta(self, meta=None, **fields):
        """Replace this entry's metadata: ``set_meta(min_role=40)``.

        Replaces rather than merges, as the store's own metadata does — read it
        first when what you want is a change to one attribute.
        """
        if meta is not None and fields:
            raise TypeError(
                "set_meta() takes the metadata as keywords or as one dict, not both"
            )
        given = fields if meta is None else meta
        if not isinstance(given, dict):
            raise TypeError(
                "set_meta() takes keywords or a dict: min_role, attributes"
            )
        for key in given:
            if key not in ("min_role", "attributes"):
                raise TypeError(
                    f"`{key}` is not part of a file's metadata; it holds: "
                    "min_role, attributes"
                )
        self._send(
            {
                "op": "setMeta",
                "path": self._path,
                "minRole": given.get("min_role"),
                "attributes": given.get("attributes") or {},
            }
        )
        return self

    def __eq__(self, other):
        return (
            isinstance(other, _Entry)
            and type(self) is type(other)
            and other._store.name == self._store.name
            and other._path == self._path
        )

    def __hash__(self):
        return hash((type(self).__name__, self._store.name, self._path))


class File(_Entry):
    """One file, which need not exist yet."""

    __slots__ = ()

    @property
    def is_directory(self):
        return False

    @property
    def parent(self):
        at = self._path.rfind("/")
        return Dir(self._store, "" if at < 0 else self._path[:at])

    def exists(self):
        """Whether a **file** is there. A directory sitting at this path is not
        this file, so it answers False rather than sending a body on to read it."""
        found = self.stat()
        return found is not None and found["is_directory"] is False

    def read_text(self):
        """The whole file, as text."""
        return self._send({"op": "read", "path": self._path, "encoding": "text"})["text"]

    def read_json(self):
        """The whole file, parsed as JSON."""
        text = self.read_text()
        try:
            return _json.loads(text)
        except ValueError as e:
            raise ValueError(f"`{self._path}` is not JSON: {e}") from None

    def read_bytes(self):
        """The whole file, as ``bytes``."""
        answer = self._send({"op": "read", "path": self._path, "encoding": "base64"})
        return _b64.b64decode(answer["base64"])

    def write(self, data):
        """Write it, replacing what is there. Answers the bytes written."""
        return self._put(data, True)

    def create(self, data):
        """The same, **refusing** an existing file."""
        return self._put(data, False)

    def _put(self, data, overwrite):
        # Another file is copied **host-side**: the bytes never enter the
        # interpreter, so `backup.write(original)` is not bounded by what one
        # read may carry.
        if isinstance(data, File):
            return _fs_send(
                data._store,
                {
                    "op": "copy",
                    "path": data._path,
                    "toStore": self._store.name,
                    "toPath": self._path,
                    "overwrite": overwrite,
                },
            )["bytes"]
        if isinstance(data, Dir):
            raise TypeError("a directory cannot be written to a file")
        plan = {"op": "write", "path": self._path, "overwrite": overwrite}
        if isinstance(data, str):
            plan["text"] = data
        elif isinstance(data, (bytes, bytearray, memoryview)):
            plan["base64"] = _b64.b64encode(bytes(data)).decode("ascii")
        elif isinstance(data, Response):
            # What `file.write(fetch(url))` is for.
            plan["base64"] = _b64.b64encode(data.content).decode("ascii")
        elif data is None:
            raise TypeError(
                "there is nothing to write — write() takes a str, bytes, a "
                "response, a file, or a value to store as JSON"
            )
        else:
            # An object is JSON, for the reason `fetch`'s `json=` is: the
            # alternative is somebody's `repr()` in a file, which is a bug every
            # time it happens.
            try:
                plan["text"] = _json.dumps(data)
            except (TypeError, ValueError) as e:
                raise TypeError(f"this value cannot be written: {e}") from None
        return self._send(plan)["bytes"]

    def move_to(self, dest):
        """Move it, answering the file it is now."""
        return self._relocate(dest, "rename", "move_to()")

    def copy_to(self, dest):
        """Copy it, answering the new file."""
        return self._relocate(dest, "copy", "copy_to()")

    def _relocate(self, dest, op, what):
        if isinstance(dest, File):
            store = dest._store
            path = dest._path
        elif isinstance(dest, Dir):
            raise TypeError(
                f"{what} takes a file — name the file inside the directory with "
                "`dir.file(name)`"
            )
        else:
            store = self._store
            path = _fs_named(dest, what)
        self._send(
            {
                "op": op,
                "path": self._path,
                "toStore": store.name,
                "toPath": path,
                "overwrite": False,
            }
        )
        # The destination as a file: what a body does next is read it or write
        # beside it, and neither should need the path spelled a second time.
        return File(store, path)

    def __str__(self):
        return f"{self._store.name}:{self._path}"

    def __repr__(self):
        return f"<saltcorn file `{self}`>"


class Dir(_Entry):
    """One directory, which need not exist yet."""

    __slots__ = ()

    @property
    def is_directory(self):
        return True

    @property
    def parent(self):
        # The root's parent is None rather than the root itself: a loop walking
        # upwards has to end somewhere, and pretending a store contains itself is
        # how it would not.
        if self._path == "":
            return None
        at = self._path.rfind("/")
        return Dir(self._store, "" if at < 0 else self._path[:at])

    def file(self, name):
        """The file of that name inside this directory."""
        return File(self._store, _fs_join(self._path, _fs_named(name, "file()")))

    def dir(self, name):
        """The directory of that name inside this one."""
        return Dir(self._store, _fs_join(self._path, _fs_named(name, "dir()")))

    def exists(self):
        """Whether a **directory** is there."""
        found = self.stat()
        return found is not None and found["is_directory"] is True

    def list(self):
        """The direct children, as the same objects everything else takes — so a
        listing is walked and acted on rather than read and re-opened by name."""
        entries = self._send({"op": "list", "path": self._path})
        return [
            Dir(self._store, entry["path"])
            if entry["isDirectory"]
            else File(self._store, entry["path"])
            for entry in entries
        ]

    def iterdir(self):
        """``pathlib``'s name for :meth:`list`, which is one host call either way."""
        return iter(self.list())

    def __iter__(self):
        return self.iterdir()

    def create(self):
        """Make it, parents included. Idempotent: one already there is what the
        caller wanted."""
        self._send({"op": "mkdir", "path": self._path})
        return self

    def __str__(self):
        return f"{self._store.name}:{self._path}/"

    def __repr__(self):
        return f"<saltcorn directory `{self}`>"


# ---------------------------------------------------------------------------
# `trigger` — this server's other triggers
# ---------------------------------------------------------------------------
#
# `trigger(name)` is a **handle** — no dispatch, and the run happens only at
# `run()`. A handle rather than attribute access, because a trigger's name is
# the admin's own words for it and may contain spaces; and `run()` as the only
# verb, because running one is the only thing a body can do to a trigger.


class TriggerHandle:
    """One trigger, and whose authority a run of it would carry."""

    __slots__ = ("name", "authority")

    def __init__(self, name, authority):
        self.name = name
        self.authority = authority

    def run(self, payload=None, **fields):
        """Run it, and answer what its action returned.

        ``run(before="2026-01-01")`` and ``run({"before": …})`` are the same
        call. The dispatcher's own rules apply: the trigger's ``only_if`` runs,
        ``None`` comes back when it declines, and the **cascade bound** counts
        this run — so a body that runs the trigger it is itself the action of
        stops at the same depth every other cascade does.
        """
        if payload is not None and fields:
            raise TypeError(
                "run() takes the payload as keywords or as one value, not both"
            )
        # Nothing passed is `{}` rather than None, so `payload["x"]` in the
        # trigger that runs is a KeyError rather than a TypeError about None.
        if payload is None:
            payload = fields
        return _call_trigger(
            {"trigger": self.name, "payload": payload, "authority": self.authority}
        )

    def as_user(self):
        """Delegate to the event's **caller**: the target's own ``min_role``
        then decides, and a refusal is a catchable :class:`TriggerError`."""
        return TriggerHandle(self.name, "user")

    def as_admin(self):
        """The trigger's own authority — the default, because running one
        trigger from another is configuration calling configuration."""
        return TriggerHandle(self.name, "admin")

    def __repr__(self):
        return f"<saltcorn trigger `{self.name}` as {self.authority}>"


class Triggers:
    """``trigger("archive_done")`` — the triggers this server has."""

    __slots__ = ()

    def __call__(self, name):
        if not isinstance(name, str) or name == "":
            raise TypeError(
                'trigger() takes the name of a trigger, as in trigger("archive_done")'
            )
        known = _call_names("triggers")
        # Named at once rather than at `run()`, for `fs`'s reason — including the
        # disabled ones, because a disabled trigger exists and "there is no
        # trigger named `nightly`" would be the wrong sentence about one an admin
        # switched off this morning.
        if known and name not in known:
            raise TriggerError(
                f"there is no trigger named `{name}`; this server has: "
                + ", ".join(known)
            )
        return TriggerHandle(name, "admin")

    @property
    def names(self):
        """What this run can reach, for a body that discovers rather than knows."""
        return tuple(_call_names("triggers"))

    def __repr__(self):
        return "<saltcorn trigger>"


# ---------------------------------------------------------------------------
# `modfn` — the functions this server's modules supply
# ---------------------------------------------------------------------------
#
# A callable *and* an object, because a module function has two names and both
# are wanted: `modfn.md_to_html(text)` is what an author writes, and
# `modfn("@saltcorn/markdown").md_to_html(text)` is what they write when two
# modules each supply the name — which v1 allows and nothing here prevents.
#
# **Synchronous**, including for a function v1 itself made `async`: everything in
# this surface is, and the wait is a host call with the GIL released like every
# other one.


def _modfn_bind(module, function):
    """One function, bound to one module."""

    def call(*args, **kwargs):
        if kwargs:
            raise TypeError(
                f"`{function}` takes positional arguments: a module function's "
                "signature is v1's, which has no keywords"
            )
        return _call_modfn(
            {"module": module, "function": function, "args": list(args)}
        )

    call.__name__ = function
    call.__qualname__ = f"{module}.{function}"
    return call


class ModuleFunctions:
    """The functions one named module supplies."""

    __slots__ = ("module", "_names")

    def __init__(self, module, names):
        self.module = module
        self._names = tuple(names)

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        if name not in self._names:
            raise AttributeError(
                f"the module `{self.module}` supplies no function `{name}`; it "
                "supplies: " + (", ".join(self._names) or "none")
            )
        return _modfn_bind(self.module, name)

    def __dir__(self):
        return sorted(set(object.__dir__(self)) | set(self._names))

    def __repr__(self):
        return f"<saltcorn module functions of `{self.module}`>"


class ModFns:
    """``modfn.md_to_html(…)``, and ``modfn("@pkg").md_to_html(…)``.

    A name nothing supplies is an ``AttributeError`` naming what this server does
    have — Python's own answer to an attribute that is not there, so
    ``getattr(modfn, name, None)`` still works. A name **two** modules supply is
    a :class:`ModuleError` naming both and the spelling that would work, because
    silently choosing the module that happened to load first is a wrong answer
    inside somebody's trigger.
    """

    __slots__ = ()

    def __call__(self, module):
        if not isinstance(module, str) or module == "":
            raise TypeError(
                'modfn() takes a module\'s package name, as in '
                'modfn("@saltcorn/markdown")'
            )
        supplied = [f for f in _call_names("functions") if f["module"] == module]
        if not supplied:
            modules = sorted({f["module"] for f in _call_names("functions")})
            raise ModuleError(
                f"no module named `{module}` supplies functions to this server"
                + ("" if not modules else "; these do: " + ", ".join(modules))
            )
        return ModuleFunctions(module, [f["name"] for f in supplied])

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        supplying = [f["module"] for f in _call_names("functions") if f["name"] == name]
        if len(supplying) == 1:
            return _modfn_bind(supplying[0], name)
        if not supplying:
            known = sorted({f["name"] for f in _call_names("functions")})
            raise AttributeError(
                f"there is no module function named `{name}`"
                + (
                    "; no installed module supplies one"
                    if not known
                    else "; this server has: " + ", ".join(known)
                )
            )
        raise ModuleError(
            f"`{name}` is supplied by " + " and ".join(supplying) + "; say which "
            f'module you mean, as in modfn("{supplying[0]}").{name}(…)'
        )

    @property
    def functions(self):
        """Every function this run can call, as ``{module, name, description,
        is_async}`` — for a body that discovers rather than knows."""
        return tuple(_call_names("functions"))

    def __dir__(self):
        names = {f["name"] for f in _call_names("functions")}
        return sorted(set(object.__dir__(self)) | names)

    def __repr__(self):
        return "<saltcorn modfn>"


# ---------------------------------------------------------------------------
# `models` — a handle on a fitted model
# ---------------------------------------------------------------------------
#
# ``models.get(name)`` answers a handle on a model's active fit (or on
# ``fit=id``'s), read through the ``db`` host (milestone 31 §3): every call is
# one database call of this run, on its budget, and a body with no ``db`` has no
# ``models``. The handle is built from one ``get``, and every later call names
# the fit that ``get`` resolved, so it does not change fit halfway through a body
# when somebody activates another. It is the JavaScript handle, synchronously
# and in snake case.


def _check_variable(what, variable):
    if not isinstance(variable, str) or not variable:
        raise TypeError(
            f'm.{what}() takes the variable first, as in m.{what}("alpha")'
        )
    return variable


def _check_elements(what, elements, keys):
    if keys is not None and elements is not None:
        raise TypeError(f"give m.{what}() either keys= or elements=, not both")
    if keys is not None:
        # The first axis by key or label — a one-axis variable's usual case.
        return {"1": list(keys) if isinstance(keys, (list, tuple)) else [keys]}
    return elements


def _check_thin(thin):
    if isinstance(thin, bool) or not isinstance(thin, int) or thin < 1:
        raise ValueError("thin= keeps every n-th draw, and takes a whole number from 1")
    return thin


#: The methods only a posterior's handle has.
_POSTERIOR_ONLY = ("draws", "summary", "variables", "write_posterior")


class Model:
    """A fitted model, as ``models.get(…)`` answers it::

        m = models.get("House prices")
        m.name, m.provider, m.table, m.outcome
        m.fit                      # id, name, status, active, warnings, metrics, …
        m.predict({"id": 3})       # one value
        m.predict([r1, r2], detail=True)   # [{"value": …, "probability": …}, …]

        r = models.get("Radon")    # a posterior also has
        r.draws("alpha", keys=[27001], chains=[1, 2], thin=10)
        r.summary("alpha")
        r.variables
        r.write_posterior(variable="alpha", statistics={"mean": "alpha_mean"})

    ``draws``, ``summary``, ``variables`` and ``write_posterior`` exist on a
    handle whose fit is a posterior; on any other they are absent, and reaching
    one raises an ``AttributeError`` saying what the model is.
    """

    __slots__ = ("_got", "_authority")

    def __init__(self, got, authority="admin"):
        object.__setattr__(self, "_got", got)
        object.__setattr__(self, "_authority", authority)

    def __setattr__(self, name, value):
        raise AttributeError("a model handle cannot be changed")

    def _send(self, what, **extra):
        plan = {
            "op": "models",
            "what": what,
            "model": self._got["name"],
            "fit": self._got["fit"]["id"],
        }
        plan.update({k: v for k, v in extra.items() if v is not None})
        return _call_db(plan)

    @property
    def name(self):
        return self._got["name"]

    @property
    def provider(self):
        return self._got["provider"]

    @property
    def table(self):
        return self._got["table"]

    @property
    def outcome(self):
        """The outcome as recorded on the fit: ``{"outcome": "regression",
        "label": "price"}`` and the like."""
        return self._got.get("outcome")

    @property
    def fit(self):
        """The fit: its id, name, status, whether it is active, when it was
        made, its error, warnings, metrics and parameters."""
        return self._got["fit"]

    def _kind(self):
        outcome = self._got.get("outcome") or {}
        return outcome.get("outcome") or "model"

    def predict(self, rows, *, detail=False):
        """One value for one row (a dict), or one per row, in order, for a list
        of them — one request either way. A row carrying the table's primary
        key is read through the model's dataset; any other must supply every
        feature. ``detail=True`` answers ``{"value": …, "probability": …}``."""
        refusal = self._got.get("no_prediction")
        if refusal:
            # What the host would refuse the request with, without sending it.
            raise DbError(refusal)
        many = isinstance(rows, (list, tuple))
        if not many and not isinstance(rows, dict):
            raise TypeError("m.predict() takes a row dict or a list of them")
        answer = self._send(
            "predict", rows=list(rows) if many else [rows], detail=bool(detail)
        )
        return answer if many else answer[0]

    def as_user(self):
        """The same handle, writing back under the event's caller's authority."""
        return Model(self._got, "user")

    def as_admin(self):
        """The same handle, writing back under the trigger's own authority —
        the default."""
        return Model(self._got, "admin")

    def __getattr__(self, name):
        # Reached only for what the class does not have: the posterior-only
        # four, on a handle whose fit is not a posterior. An `AttributeError`,
        # so `hasattr(m, "draws")` is the honest False — carrying the sentence.
        if name in _POSTERIOR_ONLY:
            raise AttributeError(
                f"`{self._got['name']}` is a {self._got['provider']} {self._kind()}; "
                f"`{name}` is for posterior models"
            )
        raise AttributeError(
            f"a model handle has no `{name}`; it has predict, fit, name, provider, "
            "table and outcome, and a posterior's also has draws, summary, variables "
            "and write_posterior"
        )

    def __dir__(self):
        names = [n for n in object.__dir__(self) if not n.startswith("_")]
        if self._kind() == "posterior":
            names += list(_POSTERIOR_ONLY)
        return sorted(set(names))

    def __repr__(self):
        return f"<saltcorn model `{self._got['name']}` ({self._kind()}), fit {self._got['fit']['id']}>"


class Posterior(Model):
    """A handle whose fit is a posterior: :class:`Model`, plus its draws."""

    __slots__ = ()

    def draws(self, variable, *, elements=None, keys=None, chains=None,
              warmup=False, thin=1):
        """One variable's draws: per chain, one list per selected element, with
        the axes' labels and keys beside them."""
        return self._send(
            "draws",
            variable=_check_variable("draws", variable),
            elements=_check_elements("draws", elements, keys),
            chains=list(chains) if chains is not None else None,
            warmup=bool(warmup),
            thin=_check_thin(thin),
        )

    def summary(self, variable, *, elements=None, keys=None):
        """The posterior summary of a variable — mean, sd, MCSE, quantiles,
        R-hat and effective sample sizes — per selected element."""
        return self._send(
            "summary",
            variable=_check_variable("summary", variable),
            elements=_check_elements("summary", elements, keys),
        )

    @property
    def variables(self):
        """What the fit drew, its ``__`` internals left out."""
        return tuple(self._got.get("variables") or ())

    def write_posterior(self, **write):
        """Write a variable's summary into rows, as the admin's Write back
        does, under this handle's authority — so ownership is checked and the
        target table's triggers fire::

            m.write_posterior(variable="alpha",
                              statistics={"mean": "alpha_mean", "sd": "alpha_sd"})
        """
        if not write:
            raise TypeError(
                'm.write_posterior() takes what to write, as in m.write_posterior('
                'variable="alpha", statistics={"mean": "alpha_mean"})'
            )
        return self._send("write_posterior", write=write, authority=self._authority)

    def as_user(self):
        return Posterior(self._got, "user")

    def as_admin(self):
        return Posterior(self._got, "admin")


class Models:
    """The models, by name::

        m = models.get("House prices")               # its active fit
        m = models.get("House prices", fit=fit_id)   # a specific fit
    """

    __slots__ = ()

    def get(self, model, *, fit=None):
        """A handle on ``model``'s active fit, or on the fit ``fit`` names."""
        if not isinstance(model, str) or not model:
            raise TypeError('models.get() takes a model\'s name, as in models.get("House prices")')
        plan = {"op": "models", "what": "get", "model": model}
        if fit is not None:
            plan["fit"] = fit
        got = _call_db(plan)
        outcome = got.get("outcome") or {}
        if outcome.get("outcome") == "posterior":
            return Posterior(got)
        return Model(got)

    def __repr__(self):
        return "<saltcorn models>"


# ---------------------------------------------------------------------------
# The handles a code body is given
# ---------------------------------------------------------------------------
#
# Building each costs nothing and holds nothing: whose run this is, what it may
# reach, what it has spent and what it may **name** all live on the **thread**
# (see the bridge), which is why one shared handle is safe and why a body cannot
# reach another run's authority — there is no name for it in the interpreter.
#
# Each is bound into a run's globals only where that run has the surface behind
# it, so naming `fs` on a server with no file stores is a `NameError` naming it
# rather than a call that fails later. Reached the long way round —
# `import saltcorn; saltcorn.fs(…)` — they are here whatever the run has, and
# refuse by saying which surface this body was not given.

#: The tables.
db = Db("admin")
#: The file stores.
fs = Fs()
#: The other triggers.
trigger = Triggers()
#: The functions this server's modules supply.
modfn = ModFns()
#: The models, over `db`: ``models.get(name)``.
models = Models()
