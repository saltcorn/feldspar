"""What `feldspar-sklearn` supplies: five scikit-learn estimators, as providers.

The third bundled module (`plugins/README.md`), and the one that makes "a model
provider is an extension point" a claim with something behind it: nothing on the
model form, the instance screen or `predict()` in a formula knows that these
five are Python. They are declared with `@sc.model_provider`, they answer `fit`
and `predict` over a **columnar frame**, and the host does everything else.

# What the host has already done by the time `fit` is called

Quite a lot, and knowing it is what keeps this file short:

- **The frame is numeric.** A one-hot expansion of a categorical column, an
  epoch cast of a date and the standardisation these providers ask for have all
  happened, once, in the host — and the resulting encoding is stored on the
  instance and applied identically at predict time. There is no `LabelEncoder`
  or `StandardScaler` here, and there must not be: a second, unrecorded encoding
  is exactly the failure that puts every coefficient against the wrong column.
- **The label is a column of the fit frame** and is absent from the predict
  frame, so `frame.features(label)` is the feature set in both.
- **A classification's label is an integer class index**, which is how a
  provider that is a regressor or a classifier depending on its label — gradient
  boosting, and the SVM — tells the two apart: the column's type says so.
- **Metrics are not ours.** R², RMSE, accuracy and the confusion matrix are the
  host's, computed over the same splits with the same code for every provider,
  which is what makes these numbers comparable with the built-in regression's.

# The state is a pickle, and why that is the right answer here

`state` is JSON, and a fitted `GradientBoostingRegressor` is not. The options
were to reimplement each estimator's prediction arithmetic (which would make the
answers this module's rather than scikit-learn's) or to carry the estimator
itself; the design says a provider wanting to store bytes stores base64, so that
is what happens. The cost is stated rather than hidden: **an instance's state is
readable by the scikit-learn that wrote it**, and a major upgrade of the package
may mean refitting. The instance carries the version it was fitted with so that
the failure names itself.
"""

import base64
import pickle

import numpy as np
import saltcorn as sc
from sklearn.cluster import DBSCAN
from sklearn.ensemble import GradientBoostingClassifier, GradientBoostingRegressor
from sklearn.linear_model import Ridge
from sklearn.manifold import TSNE
from sklearn.svm import SVC, SVR

import sklearn

#: What a fitted estimator is carried as. Pickle's highest protocol, base64 for
#: the JSON column — see the module docstring for the trade this makes.
_PICKLE_PROTOCOL = 4


def _dump(payload):
    """A fitted estimator and its feature order, as a JSON-safe state."""
    return {
        "sklearn": sklearn.__version__,
        "pickle": base64.b64encode(
            pickle.dumps(payload, protocol=_PICKLE_PROTOCOL)
        ).decode("ascii"),
    }


def _load(state):
    """The other direction, naming the version mismatch rather than raising a
    `pickle` error nobody can act on."""
    if not isinstance(state, dict) or "pickle" not in state:
        raise ValueError(
            "this instance was not fitted by feldspar-sklearn, or its state did not "
            "survive being stored"
        )
    try:
        return pickle.loads(base64.b64decode(state["pickle"]))
    except Exception as exc:  # noqa: BLE001 — re-raised with what an admin can do
        raise ValueError(
            f"this instance was fitted with scikit-learn {state.get('sklearn')} and "
            f"cannot be read by {sklearn.__version__}; refit the model "
            f"({type(exc).__name__}: {exc})"
        ) from exc


def _matrix(frame, names):
    """The named columns as a float matrix, in the order given.

    `None` becomes `nan` rather than an error: the host drops a row with a null
    feature before a fit and refuses one at predict time, so a `None` that
    reaches here is in a column the host was told to keep.
    """
    if not names:
        raise ValueError(
            "this dataset has no feature columns: a model needs at least one column "
            "beside its label"
        )
    return np.asarray(
        [[np.nan if v is None else float(v) for v in frame[name]] for name in names],
        dtype=float,
    ).T


def _label(frame, configuration, key="label"):
    """The label column's name, checked against the frame it must be in."""
    name = (configuration or {}).get(key)
    if not name or name not in frame:
        raise ValueError(
            f"`{key}`: `{name}` is not a column of this dataset (it has "
            + (", ".join(f"`{n}`" for n in frame.names) or "none")
            + ")"
        )
    return name


def _classifies(frame, label):
    """Whether the label is a **class index** rather than a measurement.

    The frame says so: the host encodes a classification's target as an integer
    column and a regression's as a float one, precisely so that a provider whose
    outcome depends on its label can read it here rather than guess.
    """
    return frame.types.get(label) == "int"


def _importances(estimator, names):
    """A feature-importance table, sorted so the top of it is the answer."""
    pairs = sorted(
        zip(names, (float(v) for v in estimator.feature_importances_)),
        key=lambda pair: -pair[1],
    )
    return sc.Parameter.table(
        "Feature importances", ["Feature", "Importance"], [list(p) for p in pairs]
    )


def _classified(estimator, features):
    """A classifier's answers as class **indices**, with a probability where the
    estimator has one.

    The index is the host's, not ours: it is what the target encoding handed us
    as `y`, so it is what comes back. Mapping it to a class name is the host's
    job, which holds the encoding.
    """
    predicted = estimator.predict(features)
    try:
        probabilities = estimator.predict_proba(features)
        classes = list(estimator.classes_)
    except (AttributeError, NotImplementedError):
        return [sc.Prediction.class_index(int(v)) for v in predicted]
    out = []
    for row, value in zip(probabilities, predicted):
        index = int(value)
        column = classes.index(index) if index in classes else None
        out.append(
            sc.Prediction.class_index(
                index, None if column is None else float(row[column])
            )
        )
    return out


@sc.model_provider(
    "sklearn_ridge",
    description="Ridge regression (scikit-learn): least squares with an L2 penalty",
    config=[sc.Field.numeric_column("label", label="Label", required=True)],
    hyperparameters=[
        sc.Field.float("alpha", label="Alpha (penalty)", default=1.0),
    ],
    outcome=sc.Outcome.regression("label"),
    # The penalty is a statement about the size of the coefficients, so it means
    # nothing until the features are on one scale. The coefficient table below
    # is therefore in standardised units, and says so in its name.
    standardise=True,
)
class SklearnRidge:
    def fit(self, frame, configuration, hyperparameters):
        label = _label(frame, configuration)
        names = frame.features(label)
        estimator = Ridge(alpha=float(hyperparameters.get("alpha", 1.0)))
        estimator.fit(_matrix(frame, names), np.asarray(frame[label], dtype=float))
        return {
            "state": _dump({"estimator": estimator, "features": names}),
            "parameters": [
                sc.Parameter.table(
                    "Coefficients (standardised features)",
                    ["Feature", "Estimate"],
                    [[name, float(c)] for name, c in zip(names, estimator.coef_)],
                ),
                sc.Parameter.scalar("Intercept", float(estimator.intercept_)),
            ],
        }

    def predict(self, state, frame):
        fitted = _load(state)
        return fitted["estimator"].predict(_matrix(frame, fitted["features"]))


@sc.model_provider(
    "sklearn_gradient_boosting",
    description=(
        "Gradient boosting (scikit-learn): boosted trees, a regressor or a "
        "classifier according to the label"
    ),
    config=[sc.Field.column("label", label="Label", required=True)],
    hyperparameters=[
        sc.Field.int("n_estimators", label="Trees", default=100),
        sc.Field.float("learning_rate", label="Learning rate", default=0.1),
        sc.Field.int("max_depth", label="Maximum depth", default=3),
    ],
    # The case `Outcome` exists for: one algorithm, two outcomes, decided by the
    # type of the column the configuration names.
    outcome=sc.Outcome.supervised("label"),
    # Trees split on order, not on scale, so standardising would change nothing
    # but the readability of what is stored.
    standardise=False,
)
class SklearnGradientBoosting:
    def fit(self, frame, configuration, hyperparameters):
        label = _label(frame, configuration)
        names = frame.features(label)
        settings = {
            "n_estimators": int(hyperparameters.get("n_estimators", 100)),
            "learning_rate": float(hyperparameters.get("learning_rate", 0.1)),
            "max_depth": int(hyperparameters.get("max_depth", 3)),
        }
        classifies = _classifies(frame, label)
        estimator = (
            GradientBoostingClassifier(**settings)
            if classifies
            else GradientBoostingRegressor(**settings)
        )
        y = np.asarray(frame[label], dtype=int if classifies else float)
        estimator.fit(_matrix(frame, names), y)
        return {
            "state": _dump(
                {"estimator": estimator, "features": names, "classifies": classifies}
            ),
            "parameters": [
                _importances(estimator, names),
                sc.Parameter.scalar("Trees fitted", float(estimator.n_estimators_)),
            ],
        }

    def predict(self, state, frame):
        fitted = _load(state)
        features = _matrix(frame, fitted["features"])
        if fitted["classifies"]:
            return _classified(fitted["estimator"], features)
        return fitted["estimator"].predict(features)


@sc.model_provider(
    "sklearn_svm",
    description=(
        "Support vector machine (scikit-learn): a classifier or a regressor "
        "according to the label"
    ),
    config=[sc.Field.column("label", label="Label", required=True)],
    hyperparameters=[
        sc.Field.float("C", label="C (regularisation)", default=1.0),
        sc.Field.string(
            "kernel",
            label="Kernel",
            default="rbf",
            options=["rbf", "linear", "poly", "sigmoid"],
        ),
        sc.Field.string("gamma", label="Gamma", default="scale", options=["scale", "auto"]),
    ],
    outcome=sc.Outcome.supervised("label"),
    # An SVM's kernel is a distance, so an unstandardised fit is dominated by
    # whichever column happens to be measured in larger units.
    standardise=True,
)
class SklearnSVM:
    def fit(self, frame, configuration, hyperparameters):
        label = _label(frame, configuration)
        names = frame.features(label)
        classifies = _classifies(frame, label)
        settings = {
            "C": float(hyperparameters.get("C", 1.0)),
            "kernel": str(hyperparameters.get("kernel", "rbf")),
            "gamma": str(hyperparameters.get("gamma", "scale")),
        }
        # `probability=True` costs an internal cross-validation at fit time and
        # is what makes a class probability available at all — which is most of
        # what a classifier is asked for here.
        estimator = SVC(probability=True, **settings) if classifies else SVR(**settings)
        y = np.asarray(frame[label], dtype=int if classifies else float)
        estimator.fit(_matrix(frame, names), y)
        parameters = [
            sc.Parameter.scalar("Support vectors", float(len(estimator.support_)))
        ]
        # Only a linear kernel has coefficients in the features at all; for any
        # other the separating surface lives in a space the columns do not name,
        # and inventing a table for it would be a picture of nothing.
        if settings["kernel"] == "linear":
            coefficients = np.asarray(estimator.coef_, dtype=float)
            if coefficients.ndim == 2 and coefficients.shape[0] == 1:
                parameters.insert(
                    0,
                    sc.Parameter.table(
                        "Coefficients (standardised features)",
                        ["Feature", "Estimate"],
                        [[n, float(c)] for n, c in zip(names, coefficients[0])],
                    ),
                )
        return {
            "state": _dump(
                {"estimator": estimator, "features": names, "classifies": classifies}
            ),
            "parameters": parameters,
        }

    def predict(self, state, frame):
        fitted = _load(state)
        features = _matrix(frame, fitted["features"])
        if fitted["classifies"]:
            return _classified(fitted["estimator"], features)
        return fitted["estimator"].predict(features)


@sc.model_provider(
    "sklearn_dbscan",
    description=(
        "DBSCAN (scikit-learn): density clustering, which finds its own number "
        "of clusters and labels outliers"
    ),
    hyperparameters=[
        sc.Field.float("eps", label="Neighbourhood radius", default=0.5),
        sc.Field.int("min_samples", label="Minimum neighbours", default=5),
    ],
    outcome=sc.Outcome.cluster(),
    standardise=True,
)
class SklearnDBSCAN:
    """Density clustering, with **cluster 0 meaning noise**.

    DBSCAN labels an outlier `-1`, and a cluster number here is a whole number
    that indexes a group. So every label is shifted by one: 0 is "this row is in
    no cluster", and the clusters themselves are 1, 2, 3. Saying it in the
    parameters rather than only here, because a screen showing "cluster 0: 41
    rows" should not be the first place an admin learns it.

    It also has no `predict`. That is not an omission in scikit-learn: the
    algorithm assigns a *given* set of points, and there is no fitted surface to
    apply to a new one. The standard answer — and the one here — is to assign a
    new row to the cluster of the nearest **core** sample when it is within `eps`
    of one, and to noise otherwise, which is exactly the rule DBSCAN itself used
    for the border points of the fit.
    """

    def fit(self, frame, configuration, hyperparameters):
        names = frame.features()
        features = _matrix(frame, names)
        estimator = DBSCAN(
            eps=float(hyperparameters.get("eps", 0.5)),
            min_samples=int(hyperparameters.get("min_samples", 5)),
        )
        labels = estimator.fit_predict(features)
        sizes = {}
        for label in labels:
            sizes[int(label) + 1] = sizes.get(int(label) + 1, 0) + 1
        return {
            "state": _dump(
                {
                    "features": names,
                    "eps": float(estimator.eps),
                    "cores": features[estimator.core_sample_indices_].tolist(),
                    "core_labels": [
                        int(labels[i]) + 1 for i in estimator.core_sample_indices_
                    ],
                }
            ),
            "parameters": [
                sc.Parameter.scalar(
                    "Clusters found", float(len({v for v in labels if v >= 0}))
                ),
                sc.Parameter.table(
                    "Cluster sizes (0 is noise)",
                    ["Cluster", "Rows"],
                    [[k, v] for k, v in sorted(sizes.items())],
                ),
            ],
        }

    def predict(self, state, frame):
        fitted = _load(state)
        features = _matrix(frame, fitted["features"])
        cores = np.asarray(fitted["cores"], dtype=float)
        labels = fitted["core_labels"]
        if cores.size == 0:
            # A fit that found no core samples clustered nothing, and every row
            # is noise. Answering that is more use than a division by zero.
            return [sc.Prediction.cluster(0) for _ in range(len(frame))]
        out = []
        for row in features:
            distances = np.linalg.norm(cores - row, axis=1)
            nearest = int(np.argmin(distances))
            out.append(
                sc.Prediction.cluster(
                    labels[nearest] if distances[nearest] <= fitted["eps"] else 0
                )
            )
        return out


@sc.model_provider(
    "sklearn_tsne",
    description=(
        "t-SNE (scikit-learn): a two- or three-dimensional embedding of the rows, "
        "for plotting"
    ),
    config=[sc.Field.int("components", label="Dimensions", default=2, required=True)],
    hyperparameters=[
        sc.Field.float("perplexity", label="Perplexity", default=30.0),
        sc.Field.float("learning_rate", label="Learning rate", default=200.0),
    ],
    outcome=sc.Outcome.embedding("components"),
    standardise=True,
)
class SklearnTSNE:
    """A neighbour-preserving embedding, and the one honest caveat in this file.

    t-SNE has **no out-of-sample extension**: the embedding is fitted for the
    points it was given and there is no transform to apply to a new one. So a
    prediction here is the embedding of the *nearest fitted row* — which is a
    documented approximation rather than a model being applied, and it is what
    makes the host's metric pass and the "try a row" box answer at all. If what
    you want is a projection that applies to new rows, the built-in `pca`
    provider is that and this one is not.
    """

    def fit(self, frame, configuration, hyperparameters):
        names = frame.features()
        features = _matrix(frame, names)
        components = int((configuration or {}).get("components") or 2)
        # `perplexity` must be below the number of rows or scikit-learn refuses;
        # clamping is kinder than a fit that fails on a small dataset, and the
        # value actually used is reported.
        perplexity = min(
            float(hyperparameters.get("perplexity", 30.0)), max(len(frame) - 1, 1)
        )
        estimator = TSNE(
            n_components=components,
            perplexity=perplexity,
            learning_rate=float(hyperparameters.get("learning_rate", 200.0)),
            init="pca" if components < features.shape[1] else "random",
        )
        embedding = estimator.fit_transform(features)
        return {
            "state": _dump(
                {
                    "features": names,
                    "points": features.tolist(),
                    "embedding": np.asarray(embedding, dtype=float).tolist(),
                }
            ),
            "parameters": [
                sc.Parameter.scalar("Dimensions", float(components)),
                sc.Parameter.scalar("Perplexity used", perplexity),
                sc.Parameter.scalar("KL divergence", float(estimator.kl_divergence_)),
            ],
        }

    def predict(self, state, frame):
        fitted = _load(state)
        features = _matrix(frame, fitted["features"])
        points = np.asarray(fitted["points"], dtype=float)
        embedding = np.asarray(fitted["embedding"], dtype=float)
        out = []
        for row in features:
            nearest = int(np.argmin(np.linalg.norm(points - row, axis=1)))
            out.append(sc.Prediction.vector(embedding[nearest]))
        return out
