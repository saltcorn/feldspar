// A module supplying **model providers** — this system's `modelproviders` key
// rather than anything v1 had (TODO "Predictive models" §14).
//
// Deliberately arithmetic and not machine learning: what these tests are about
// is the seam — a declaration crossing into the manifest, a columnar frame
// crossing into `fit`, a state and its parameters crossing back, and a list of
// predictions crossing back again — and a real estimator here would only make
// the assertions about somebody else's numerical library.
//
// Three of the five providers are broken on purpose. A module with one
// mis-declared provider must still supply the others, with a sentence on its
// card saying which one is missing and why; a module that refused to load over a
// typo would take four working estimators down with the fifth.

const Workflow = require("@saltcorn/data/models/workflow");
const Form = require("@saltcorn/data/models/form");

/** One column of a frame, by name. The frame crosses as columns — `{ rows,
 * columns: [{ name, type, values }] }` — which is the whole shape of it. */
const column = (frame, name) => {
  const found = (frame.columns || []).find((c) => c.name === name);
  if (!found) throw new Error(`the frame has no column ${name}`);
  return found.values;
};

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "model",
  // A function of the module's own configuration, which is v1's `withCfg` rule
  // applied to this key like every other: the host has to call it to learn what
  // is here.
  modelproviders: (cfg) => ({
    // A regression, of the least interesting kind there is: it predicts the mean
    // of the label, plus whatever `shift` says. What it exercises is that the
    // label picker's options are filled in from *the dataset*, that a
    // hyperparameter arrives as a number, and that a grid over one is searched
    // by the host rather than here.
    echo_mean: {
      description: "Predict the mean of the label",
      config_fields: [
        {
          name: "label",
          label: "Label",
          type: "String",
          required: true,
          server_query: "dataset_numeric_columns",
        },
      ],
      hyperparameters: [{ name: "shift", label: "Shift", type: "Float", default: 0 }],
      outcome: { kind: "regression", label: "label" },
      fit: async ({ frame, configuration, hyperparameters }) => {
        const values = column(frame, configuration.label);
        const shift = Number(hyperparameters.shift || 0);
        const mean = values.reduce((a, b) => a + Number(b), 0) / (values.length || 1);
        return {
          state: { mean: mean + shift, from: (cfg || {}).endpoint || null },
          parameters: [
            { block: "scalar", name: "Mean", value: mean },
            {
              block: "table",
              name: "Rows seen",
              columns: ["Column", "Rows"],
              rows: [{ cells: [configuration.label, values.length] }],
            },
          ],
        };
      },
      // A bare number per row: the host reads that as a regression's answer
      // rather than demanding `{ prediction: "number", value: … }` for each of
      // fifty thousand rows.
      predict: async ({ state, frame }) => new Array(frame.rows).fill(state.mean),
    },

    // A clustering, to exercise the other direction: a cluster number is not a
    // number a regression predicts, so it is written out in full.
    echo_sign: {
      description: "Cluster rows by the sign of their first column",
      configuration_workflow: () =>
        new Workflow({
          steps: [
            {
              name: "Column",
              form: () =>
                new Form({
                  fields: [{ name: "on", label: "Column", type: "String", required: true }],
                }),
            },
          ],
        }),
      outcome: { kind: "cluster" },
      standardise: true,
      // What a fit shows (analytics TODO A3.2): its rule, and a bar chart over
      // a frame of its own.
      outputs: [
        { name: "rule", label: "Rule", kind: "parameters", block: "Rule" },
        {
          name: "signs",
          label: "Rows by sign",
          kind: "plot",
          data: "signs",
          spec: {
            layers: [
              { mark: "bar", encoding: { x: { field: "sign" }, y: { field: "rows" } } },
            ],
          },
        },
      ],
      fit: async ({ frame, configuration }) => {
        const values = column(frame, configuration.on).map(Number);
        const negative = values.filter((v) => v < 0).length;
        return {
          state: { on: configuration.on },
          parameters: [{ block: "text", name: "Rule", body: "negative is 0, otherwise 1" }],
          outputs: {
            signs: {
              rows: 2,
              columns: [
                { name: "sign", type: "str", values: ["negative", "positive"] },
                { name: "rows", type: "int", values: [negative, values.length - negative] },
              ],
            },
          },
        };
      },
      predict: async ({ state, frame }) =>
        column(frame, state.on).map((v) => ({
          prediction: "cluster",
          cluster: Number(v) < 0 ? 0 : 1,
        })),
    },

    // Reported and skipped: no `fit`, so nothing can be fitted with it.
    echo_unfittable: {
      outcome: { kind: "cluster" },
      predict: async () => [],
    },

    // Reported and skipped: an outcome kind nothing implements.
    echo_nonsense: {
      outcome: { kind: "haruspicy" },
      fit: async () => ({ state: null }),
      predict: async () => [],
    },

    // Reported and skipped: a supervised outcome with no configuration key
    // naming the label, which would make every model of it unresolvable.
    echo_unlabelled: {
      outcome: { kind: "regression" },
      fit: async () => ({ state: null }),
      predict: async () => [],
    },
  }),
};
