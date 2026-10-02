"""One of each thing a Saltcorn Python plugin can supply (§2 of the API).

An action, a function, a table provider, the module's own settings and an
`on_load` — written the way the specification's example is written, because what
these tests are for is that the example works.

The action reaches `sc.db`, which is the claim §2 makes that the JavaScript
module tier cannot: a Python plugin has no v1 to be compatible with, so it is
handed the plans directly with the same authority and the same budgets a code
body has.
"""

import warnings

import saltcorn as sc

sc.settings(
    sc.Field.string("api_key", label="API key", secret=True, required=True),
    sc.Field.string("greeting", label="Greeting", default="hello"),
)

#: What `on_load` saw, so a test can assert it was called — and called *again*
#: after a configuration change (§11).
STATE = {"loads": 0, "configuration": {}}

#: The table provider's rows. Module level, so a write through the provider is
#: visible to the next read; reset when the package is imported again, which is
#: exactly what a reload is.
ROWS = [
    {"id": 1, "name": "alpha", "size": 3},
    {"id": 2, "name": "beta", "size": 7},
]


@sc.on_load
def load(configuration):
    STATE["loads"] += 1
    STATE["configuration"] = dict(configuration)


@sc.action(
    description="Note something about a row",
    config=[sc.Field.string("note", label="Note", required=True)],
)
def fixture_note(row, config, configuration, trigger, mode):
    """Asks for five of the nine parameters, and is passed exactly those."""
    who = (row or {}).get("who")
    sc.db.notes.insert(what=config["note"], who=who)
    return {
        "noted": config["note"],
        "who": who,
        # The module's own settings, which no trigger can change.
        "greeting": configuration.get("greeting"),
        "key": configuration.get("api_key"),
        "trigger": trigger,
        "mode": mode,
        "loads": STATE["loads"],
        # Read back through the same surface, so the test knows the write went
        # to the real table rather than into a dictionary somewhere.
        "notes": sc.db.notes.count(),
    }


@sc.action(name="insert_row", description="claims a built-in's name")
def clashes():
    """A name the built-ins already have.

    Here on purpose: "a name that collides with a built-in is reported and not
    installed" is a rule, and a rule needs a plugin that breaks it.
    """
    return "this should never run"


@sc.function(description="Shout a string")
def fixture_shout(text: str) -> str:
    return f"{text.upper()}!"


@sc.function()
def fixture_add(a: int, b: int) -> int:
    return a + b


@sc.function(description="What on_load last saw")
def fixture_loaded():
    """The evidence a reload is a re-import (§11).

    ``loads`` counts from zero **per import**, so it is 1 after a load however
    many times the module has been loaded — and 2 would mean the package's own
    state had survived, which is what §11 says it must not.
    """
    return {"loads": STATE["loads"], "greeting": STATE["configuration"].get("greeting")}


@sc.table_provider(
    "Fixture rows",
    config=[sc.Field.int("min_size", label="Smallest size", default=0)],
)
class FixtureRows:
    """A table of two rows, filtered by the one setting it declares.

    It defines all three write methods, which is what makes a table backed by it
    writable — v1's rule, said in Python.
    """

    def fields(self, configuration):
        return [
            sc.Field.int("id", primary_key=True),
            sc.Field.string("name"),
            sc.Field.int("size"),
        ]

    def rows(self, configuration, where=None, options=None):
        least = configuration.get("min_size") or 0
        return [row for row in ROWS if row["size"] >= least]

    def insert_row(self, configuration, record):
        key = max((row["id"] for row in ROWS), default=0) + 1
        ROWS.append({"id": key, "name": record.get("name"), "size": record.get("size") or 0})
        return key

    def update_row(self, configuration, record, id):
        for row in ROWS:
            if row["id"] == id:
                row.update(record)

    def delete_rows(self, configuration, where):
        # The `where` a provider is handed is the narrowest thing the caller
        # could say about the rows it means, in v1's own vocabulary — which for
        # a delete is `{"id": {"in": [...]}}`, because the statement's filter may
        # be an expression that object cannot spell.
        wanted = (where or {}).get("id")
        if isinstance(wanted, dict):
            keys = list(wanted.get("in") or [])
        elif wanted is None:
            keys = []
        else:
            keys = [wanted]
        ROWS[:] = [row for row in ROWS if row["id"] not in keys]


@sc.model_provider(
    "fixture_mean",
    description="Predict the mean of the label",
    config=[sc.Field.numeric_column("label", label="Label", required=True)],
    hyperparameters=[sc.Field.float("shift", label="Shift", default=0.0)],
    outcome=sc.Outcome.regression("label"),
)
class FixtureMean:
    """A model provider that is arithmetic rather than machine learning.

    What it is here to prove is the **seam**: a declaration reaching the
    manifest with its label picker unresolved, a columnar frame reaching `fit`,
    a state and its parameters coming back, and one prediction per row. A real
    estimator would need numpy, and this fixture is installed with no network.
    """

    def fit(self, frame, configuration, hyperparameters):
        values = [float(v) for v in frame[configuration["label"]]]
        mean = sum(values) / (len(values) or 1)
        shift = float(hyperparameters.get("shift") or 0)
        # Both ways a provider warns: `warnings.warn`, which the host catches,
        # and the `warnings` of the answer.
        if shift < 0:
            warnings.warn("a negative shift predicts below every mean")
        return {
            "state": {"mean": mean + shift},
            "warnings": [configuration["note"]] if configuration.get("note") else [],
            "parameters": [
                sc.Parameter.scalar("Mean", mean),
                sc.Parameter.table(
                    "Rows seen",
                    ["Column", "Rows"],
                    [[configuration["label"], len(values)]],
                ),
            ],
        }

    def predict(self, state, frame):
        # Bare numbers: the host reads one as a regression's answer rather than
        # asking for `{"prediction": "number", ...}` fifty thousand times.
        return [state["mean"]] * len(frame)


@sc.model_provider(
    "fixture_sign",
    description="Cluster rows by the sign of a column",
    config=[sc.Field.column("on", label="Column", required=True)],
    outcome=sc.Outcome.cluster(),
    standardise=True,
    # What a fit shows (analytics TODO A3.2): its rule, and a bar chart over a
    # frame of its own.
    outputs=[
        {"name": "rule", "label": "Rule", "kind": "parameters", "block": "Rule"},
        {
            "name": "signs",
            "label": "Rows by sign",
            "kind": "plot",
            "data": "signs",
            "spec": {
                "layers": [
                    {
                        "mark": "bar",
                        "encoding": {"x": {"field": "sign"}, "y": {"field": "rows"}},
                    }
                ]
            },
        },
    ],
)
class FixtureSign:
    """The other direction: a cluster number is not a number a regression
    predicts, so it is written out in full."""

    def fit(self, frame, configuration, hyperparameters):
        values = [float(v) for v in frame[configuration["on"]]]
        negative = sum(1 for v in values if v < 0)
        return {
            "state": {"on": configuration["on"]},
            "parameters": [sc.Parameter.text("Rule", "negative is 0, otherwise 1")],
            "outputs": {
                "signs": {
                    "rows": 2,
                    "columns": [
                        {"name": "sign", "type": "str", "values": ["negative", "positive"]},
                        {
                            "name": "rows",
                            "type": "int",
                            "values": [negative, len(values) - negative],
                        },
                    ],
                }
            },
        }

    def predict(self, state, frame):
        return [
            sc.Prediction.cluster(0 if float(v) < 0 else 1) for v in frame[state["on"]]
        ]
