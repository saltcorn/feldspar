// A fit's report (the model editor's "Report" modal): everything that says
// what a fit is and how good it is — what was fitted, to which data, when,
// with what settings; its warnings; its rows; its outputs; its classes and its
// hyperparameter search — laid out as a Report workspace's state, so the modal
// shows exactly what "Save as report workspace" stores.
//
// The facts are **written into text blocks** when the report is built: a fit
// never changes, so they cannot go stale, and the report reads the same after
// the model has been edited. The outputs are **panels** (`outputPanel`): a plot
// is its spec over the fit's output data and a table is `{ fit, output }`, so
// they draw from the fit wherever the report is opened — and say so, if the fit
// is deleted.
//
// Pure, so that what goes into a report is tested without a screen.

import type { Translate } from "../datasets/ops";
import { newPanelId, outputPanel } from "../panels/panel";
import { DEFAULT_PAGE } from "../report/pages";
import type { Block, ReportState } from "../report/state";
import type { BindReport } from "./ModelBindings";
import {
  formatNumber,
  formatTimestamp,
  instanceLabel,
  metricRows,
  orderWarnings,
  outcomeSummary,
  printGridValue,
  readEncoding,
  readMetrics,
  readOutcome,
  readRowCounts,
  readSearch,
  type ClassMetrics,
  type GridPoint,
  type InstanceDetail,
  type Metrics,
  type SplitMetrics,
  type WarningKind,
} from "./models";
import type { ModelOutput } from "./outputs";

/** What a report is built from. */
export type FitReportInput = {
  model: { name: string; description: string; provider: string };
  /** The provider's description, when the editor has it. */
  providerDescription?: string | null;
  /** The name of the model's dataset. */
  datasetName?: string | null;
  instance: InstanceDetail;
  /** The outputs to show, in order: the fit's own, then the optional plots
   * chosen (`shownOutputs`). */
  outputs: ModelOutput[];
  t: Translate;
};

/** The longest heading a report keeps (`sc_analytics::panel::MAX_HEADING`). */
const MAX_HEADING = 1_000;

/** Text as Markdown shows it literally: its markup and a table's pipes
 * escaped, its line breaks made spaces. */
export function mdText(value: unknown): string {
  const s = value === null || value === undefined ? "" : String(value);
  return s.replace(/\s*\n\s*/g, " ").replace(/([\\`*_[\]|<>#])/g, "\\$1");
}

/** A Markdown pipe table. */
export function mdTable(header: string[], rows: string[][]): string {
  const line = (cells: string[]) => `| ${cells.join(" | ")} |`;
  return [line(header), line(header.map(() => "---")), ...rows.map(line)].join("\n");
}

/** The heading each kind of posterior warning is read under. */
export function warningTitle(kind: WarningKind, t: Translate): string {
  switch (kind) {
    case "rhat":
      return t("The chains disagree");
    case "divergent":
      return t("Divergent transitions");
    case "ebfmi":
      return t("The sampler moved poorly between energy levels");
    case "ess":
      return t("Too few effective draws");
    case "treedepth":
      return t("Trajectories were cut short");
    case "other":
      return t("Worth knowing");
  }
}

/** The name a saved report is offered under. */
export function fitReportName(modelName: string, instance: { name: string; created: string }, t: Translate): string {
  return t("{model} — {fit} report", { model: modelName, fit: instanceLabel(instance) });
}

/** The report of one fit, as a Report workspace's state. */
export function buildFitReport(input: FitReportInput): ReportState {
  const { model, instance, t } = input;
  const blocks: Block[] = [];
  const heading = (text: string, level: 1 | 2 | 3 = 2) =>
    blocks.push({ id: newPanelId(), kind: "heading", text: text.slice(0, MAX_HEADING), level });
  const text = (markdown: string) => {
    if (markdown.trim() !== "") blocks.push({ id: newPanelId(), kind: "text", markdown });
  };

  const outcome = readOutcome(instance.outcome);
  const posterior = outcome?.outcome === "posterior";

  // --- what this is ------------------------------------------------------------
  heading(t("{model} — {fit}", { model: model.name, fit: instanceLabel(instance) }), 1);
  if (model.description.trim() !== "") text(mdText(model.description.trim()));
  text(mdTable([t("About this fit"), ""], factRows(input).map(([k, v]) => [mdText(k), v])));

  // --- what to read first ------------------------------------------------------
  const notices: string[] = [];
  if (instance.error) notices.push(`> **${mdText(t("The fit failed."))}** ${mdText(instance.error)}`);
  if (instance.dataset_changed) {
    notices.push(`> ${mdText(t("The dataset has changed since this fit. What is reported here is about the rows it read."))}`);
  }
  if (posterior && instance.program_changed) {
    notices.push(`> ${mdText(t("The program has changed since this fit. What is reported here is about the copy it ran."))}`);
  }
  text(notices.join("\n\n"));
  if (posterior && instance.warnings.length > 0) {
    heading(t("Warnings"));
    text(
      orderWarnings(instance.warnings)
        .map((w) => `- **${mdText(warningTitle(w.kind, t))}.** ${mdText(w.text)}`)
        .join("\n"),
    );
  }

  // --- the data ----------------------------------------------------------------
  const rows = readRowCounts(instance.rows);
  if (rows && !posterior) {
    heading(t("Rows"));
    const counts = [rows.selected, rows.train, rows.validation, rows.test, rows.dropped].map(String);
    let body = mdTable([t("Selected"), t("Train"), t("Validation"), t("Test"), t("Dropped")], [counts]);
    if (rows.dropped > 0) {
      body += `\n\n${mdText(
        t("{dropped} rows could not be represented by the encoding — a null in a feature, most often — and were dropped.", {
          dropped: rows.dropped,
        }),
      )}`;
    }
    text(body);
  }
  const binding = (instance.binding ?? null) as BindReport | null;
  if (posterior && binding) {
    heading(t("Data"));
    const parts: string[] = [];
    const dims = Object.entries(binding.dimensions ?? {});
    if (dims.length > 0) parts.push(mdTable([t("Dimension"), t("Size")], dims.map(([n, s]) => [`\`${n}\``, String(s)])));
    const datasets = binding.datasets ?? [];
    if (datasets.length > 0) {
      parts.push(
        mdTable(
          [t("Dataset"), t("Rows read"), t("Rows bound")],
          datasets.map((d) => [mdText(d.name), String(d.read), String(d.bound)]),
        ),
      );
    }
    for (const d of binding.drops ?? []) parts.push(`> ${mdText(d.sentence)}`);
    for (const w of binding.warnings ?? []) parts.push(`> ${mdText(w)}`);
    text(parts.join("\n\n"));
  }

  // --- how good it is ----------------------------------------------------------
  const train = readMetrics(instance.metrics).train ?? null;
  if (posterior && train && metricRows(train).length > 0) {
    heading(t("Diagnostics"));
    text(mdTable([t("Diagnostic"), t("Value")], metricRows(train).map((r) => [mdText(r.label), mdText(r.value)])));
  }

  // --- the outputs -------------------------------------------------------------
  const shown = input.outputs.filter((o) => !o.error);
  const failed = input.outputs.filter((o) => o.error);
  if (shown.length > 0 || failed.length > 0) heading(t("Outputs"));
  for (const output of shown) {
    const panel = outputPanel(output, instance.id, undefined, t);
    if (panel) blocks.push({ id: newPanelId(), kind: "panel", panel });
  }
  text(failed.map((o) => `> **${mdText(o.label)}:** ${mdText(o.error)}`).join("\n\n"));

  const classes = classesText(readMetrics(instance.metrics), t);
  if (classes) {
    heading(classes.title);
    text(classes.markdown);
  }

  const search = readSearch(instance.search);
  if (search.length > 0) {
    heading(t("Hyperparameter search"));
    text(searchText(search, instance.hyperparameters, t));
  }

  return { blocks, page: DEFAULT_PAGE };
}

/** The facts the report opens with: what, which data, when, and how. */
function factRows({ model, providerDescription, datasetName, instance, t }: FitReportInput): [string, string][] {
  const outcome = readOutcome(instance.outcome);
  const facts: [string, string][] = [];
  facts.push([t("Model"), mdText(model.name)]);
  const provider = providerDescription?.trim()
    ? `\`${mdText(model.provider)}\` — ${mdText(providerDescription.trim())}`
    : `\`${mdText(model.provider)}\``;
  facts.push([t("Type"), provider]);
  facts.push([t("Outcome"), mdText(outcomeSummary(outcome))]);
  if (datasetName) facts.push([t("Dataset"), mdText(datasetName)]);
  if (instance.name.trim() !== "") facts.push([t("Fit"), mdText(instance.name)]);
  if (instance.description.trim() !== "") facts.push([t("Description"), mdText(instance.description)]);
  facts.push([t("Fitted"), mdText(formatTimestamp(instance.created))]);
  facts.push([t("Status"), mdText(instance.active ? `${instance.status}, ${t("active")}` : instance.status)]);
  if (outcome && (outcome.outcome === "regression" || outcome.outcome === "classification")) {
    facts.push([t("Predicts"), `\`${mdText(outcome.label)}\``]);
  }
  const features = (readEncoding(instance.encoding)?.columns ?? []).map((c) => c.column);
  if (features.length > 0) facts.push([t("Features"), features.map((f) => `\`${mdText(f)}\``).join(", ")]);
  const hyper = Object.entries((instance.hyperparameters ?? {}) as Record<string, unknown>);
  if (hyper.length > 0) {
    facts.push([t("Hyperparameters"), hyper.map(([k, v]) => `\`${mdText(k)}\` = ${mdText(printGridValue(v))}`).join(", ")]);
  }
  facts.push([t("Fit id"), `\`${instance.id}\``]);
  return facts;
}

/** A classification's per-class scores and confusion matrix, off the held-out
 * rows where there are any. */
function classesText(metrics: SplitMetrics, t: Translate): { title: string; markdown: string } | null {
  let set: { part: string; metrics: Metrics } | null = null;
  for (const part of ["test", "validation", "train"] as const) {
    const m = metrics[part];
    if (m) {
      set = { part, metrics: m };
      break;
    }
  }
  if (!set || set.metrics.metrics !== "classification") return null;
  const c = set.metrics;
  const header = [
    t("Class"),
    t("Precision"),
    t("Recall"),
    "F1",
    t("Support"),
    ...c.classes.map((k: ClassMetrics) => mdText(t("predicted {class_}", { class_: k.class }))),
  ];
  const rows = c.classes.map((k: ClassMetrics, i: number) => [
    mdText(k.class),
    formatNumber(k.precision),
    formatNumber(k.recall),
    formatNumber(k.f1),
    String(k.support),
    ...c.classes.map((_, j) => (i === j ? `**${c.confusion[i]?.[j] ?? 0}**` : String(c.confusion[i]?.[j] ?? 0))),
  ]);
  return {
    title: t("Classes ({part})", { part: set.part }),
    markdown: `${mdTable(header.map((h) => (h === "F1" ? h : mdText(h))), rows)}\n\n${mdText(
      t("The right-hand block is the confusion matrix: each row is what the rows actually were, each column what this fit said they are."),
    )}`,
  };
}

/** Every point a search tried, the chosen one marked. */
function searchText(search: GridPoint[], chosen: unknown, t: Translate): string {
  const keys: string[] = [];
  for (const point of search) for (const k of Object.keys(point.hyperparameters)) if (!keys.includes(k)) keys.push(k);
  const picked = (chosen ?? {}) as Record<string, unknown>;
  const same = (p: Record<string, unknown>) =>
    [...new Set([...Object.keys(p), ...Object.keys(picked)])].every((k) => JSON.stringify(p[k]) === JSON.stringify(picked[k]));
  const rows = search.map((p) => [
    ...keys.map((k) => `\`${mdText(printGridValue(p.hyperparameters[k]))}\``),
    p.error ? "—" : formatNumber(p.score),
    mdText(p.error ?? (same(p.hyperparameters) ? t("chosen") : "")),
  ]);
  return `${mdTable([...keys.map((k) => `\`${mdText(k)}\``), mdText(t("Validation score")), ""], rows)}\n\n${mdText(
    t("Every point tried, scored on the validation rows. The chosen one was refitted and is what the metrics are of."),
  )}`;
}
