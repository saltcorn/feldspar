// What the Simulation workspace and its two panels share (analytics TODO
// A7.6, A7.8): the input controls, the prediction, the profile curves and the
// scenarios compared, and the hooks that ask the server for them.
//
// A control's change is sent a moment after the last one (`SETTLE_MS`), and
// the curves stay on the screen, faded, until the new ones come — so dragging
// a slider updates the prediction and every curve together without each step
// of the drag being a request.

import { useEffect, useMemo, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import type { Option } from "../plot/echarts";
import { chartPalette } from "../plot/palette";
import { echarts } from "../plot/runtime";
import type { Theme } from "../theme";
import { comparisonOption, curveOption, type ChartLook } from "./charts";
import {
  currentValue,
  formatValue,
  intervalText,
  isMoved,
  levelKey,
  parseControl,
  probabilityText,
  sliderStep,
  type Comparison,
  type Input,
  type Prediction,
  type Profile,
  type Scenario,
} from "./simulation";

/** How long after the last change the server is asked. */
const SETTLE_MS = 150;

/** One ECharts chart filling its box, redrawn when the option changes. */
export function EChart({
  option,
  renderer = "canvas",
  className = "an-sim-chart",
  label,
}: {
  option: Option;
  renderer?: "canvas" | "svg";
  className?: string;
  label: string;
}) {
  const box = useRef<HTMLDivElement>(null);
  const chart = useRef<ReturnType<typeof echarts.init> | null>(null);
  const latest = useRef(option);
  latest.current = option;
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const instance = echarts.init(el, undefined, { renderer });
    instance.setOption(latest.current, { notMerge: true });
    chart.current = instance;
    const observer = new ResizeObserver(() => instance.resize());
    observer.observe(el);
    return () => {
      observer.disconnect();
      instance.dispose();
      chart.current = null;
    };
  }, [renderer]);
  useEffect(() => {
    chart.current?.setOption(option, { notMerge: true });
  }, [option]);
  return <div ref={box} className={className} role="img" aria-label={label} />;
}

/** The look of the charts in a colour scheme. */
export function useChartLook(theme: Theme, still = false): ChartLook {
  const { t } = useT();
  return useMemo(
    () => ({
      palette: chartPalette(theme),
      still,
      probabilityLabel: t("probability"),
      densityLabel: t("density"),
    }),
    [theme, still, t],
  );
}

// --- asking the server --------------------------------------------------------------

/** What a simulation asks with: the model, the fit and the level. */
export type Target = { model: string; fit?: string; level?: number };

/** The profile at `values`, asked again a moment after they change. The last
 * answer is kept while the next is on its way (`stale`). */
export function useProfile(target: Target | null, values: Record<string, unknown>, version = 0) {
  const { t } = useT();
  const [profile, setProfile] = useState<Profile | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState(false);
  const key = target ? JSON.stringify([target, values]) : "";
  useEffect(() => {
    if (!target) {
      setProfile(null);
      setError(null);
      return;
    }
    let live = true;
    setStale(true);
    const timer = window.setTimeout(() => {
      api
        .profileModel(target.model, { fit: target.fit ?? null, level: target.level ?? null, values })
        .then((answer) => {
          if (!live) return;
          setProfile(answer as unknown as Profile);
          setError(null);
        })
        .catch((err: unknown) => live && setError(errorMessage(err, t("Could not simulate the model."))))
        .finally(() => live && setStale(false));
    }, SETTLE_MS);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
    // The target and the values are compared by value (`key`).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, version, t]);
  return { profile, error, stale };
}

/** The scenarios compared, asked again when they change. */
export function useComparison(target: Target | null, scenarios: Scenario[], version = 0) {
  const { t } = useT();
  const [comparison, setComparison] = useState<Comparison | null>(null);
  const [error, setError] = useState<string | null>(null);
  const key = target && scenarios.length > 0 ? JSON.stringify([target, scenarios]) : "";
  useEffect(() => {
    if (!target || scenarios.length === 0) {
      setComparison(null);
      setError(null);
      return;
    }
    let live = true;
    const timer = window.setTimeout(() => {
      api
        .compareScenarios(target.model, {
          fit: target.fit ?? null,
          level: target.level ?? null,
          scenarios: scenarios.map((s) => ({ name: s.name, values: s.values })),
        })
        .then((answer) => {
          if (!live) return;
          setComparison(answer as unknown as Comparison);
          setError(null);
        })
        .catch((err: unknown) => live && setError(errorMessage(err, t("Could not compare the scenarios."))));
    }, SETTLE_MS);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, version, t]);
  return { comparison, error };
}

// --- the inputs -----------------------------------------------------------------

/** A control per input, by its kind: a slider with a box for a number, a
 * date picker, a drop-down of the levels, a switch. A moved input can be put
 * back at its typical value. */
export function InputControls({
  inputs,
  values,
  onChange,
}: {
  inputs: Input[];
  values: Record<string, unknown>;
  /** `undefined` puts the input back at its typical value. */
  onChange: (input: string, value: unknown) => void;
}) {
  const { t } = useT();
  return (
    <div className="an-sim-inputs">
      {inputs.map((input) => (
        <div key={input.name} className="an-sim-input">
          <div className="d-flex align-items-baseline gap-2">
            <Form.Label htmlFor={`sim-${input.name}`} className="mb-0 fw-semibold small text-truncate">
              {input.name}
            </Form.Label>
            {isMoved(input, values) ? (
              <button
                type="button"
                className="btn btn-link btn-sm p-0 ms-auto small"
                onClick={() => onChange(input.name, undefined)}
                title={t("Back to the typical value, {value}", { value: formatValue(input.typical) })}
              >
                <T text="typical" />
              </button>
            ) : (
              <span className="ms-auto small text-secondary">
                <T text="typical" />
              </span>
            )}
          </div>
          <Control input={input} value={currentValue(input, values)} onChange={(v) => onChange(input.name, v)} />
        </div>
      ))}
    </div>
  );
}

function Control({ input, value, onChange }: { input: Input; value: unknown; onChange: (value: unknown) => void }) {
  const { t } = useT();
  const id = `sim-${input.name}`;
  switch (input.kind) {
    case "number":
    case "integer": {
      const min = Number(input.min);
      const max = Number(input.max);
      const ranged = Number.isFinite(min) && Number.isFinite(max) && max > min;
      return (
        <div className="d-flex align-items-center gap-2">
          {ranged && (
            <Form.Range
              id={id}
              min={min}
              max={max}
              step={sliderStep(input)}
              value={Number(value)}
              onChange={(e) => {
                const v = parseControl(input, e.target.value);
                if (v !== undefined) onChange(v);
              }}
            />
          )}
          <NumberBox input={input} value={value} onChange={onChange} id={ranged ? undefined : id} />
        </div>
      );
    }
    case "date":
      return (
        <Form.Control
          id={id}
          size="sm"
          type="date"
          value={typeof value === "string" ? value.slice(0, 10) : ""}
          min={typeof input.min === "string" ? input.min.slice(0, 10) : undefined}
          max={typeof input.max === "string" ? input.max.slice(0, 10) : undefined}
          onChange={(e) => {
            const v = parseControl(input, e.target.value);
            if (v !== undefined) onChange(v);
          }}
        />
      );
    case "boolean":
      return (
        <Form.Check
          id={id}
          type="switch"
          checked={value === true}
          label={value === true ? t("yes") : t("no")}
          onChange={(e) => onChange(e.target.checked)}
        />
      );
    case "category":
      return (
        <Form.Select
          id={id}
          size="sm"
          value={levelKey(value)}
          onChange={(e) => {
            const v = parseControl(input, e.target.value);
            if (v !== undefined) onChange(v);
          }}
        >
          {(input.levels ?? []).map((l) => (
            <option key={levelKey(l)} value={levelKey(l)}>
              {formatValue(l)}
            </option>
          ))}
        </Form.Select>
      );
  }
}

/** A number typed in: sent when it reads as one, kept as typed until then. */
function NumberBox({
  input,
  value,
  onChange,
  id,
}: {
  input: Input;
  value: unknown;
  onChange: (value: unknown) => void;
  id?: string;
}) {
  const shown = typeof value === "number" ? String(Math.round(value * 1e6) / 1e6) : "";
  const [text, setText] = useState(shown);
  useEffect(() => setText(shown), [shown]);
  return (
    <Form.Control
      id={id}
      size="sm"
      className="an-sim-number"
      inputMode="decimal"
      value={text}
      onChange={(e) => {
        setText(e.target.value);
        const v = parseControl(input, e.target.value);
        if (v !== undefined) onChange(v);
      }}
      aria-label={input.name}
    />
  );
}

// --- the prediction and the curves ---------------------------------------------------

/** The prediction at the current inputs, large, with its interval. */
export function PredictionView({ prediction, outcome }: { prediction: Prediction; outcome: string }) {
  const { t, locale } = useT();
  const interval = intervalText(prediction, t, locale);
  const probability = outcome === "classification" ? probabilityText(prediction, t) : null;
  return (
    <div className="an-sim-prediction" aria-live="polite">
      <div className="small text-secondary">
        <T text="Predicted" />
      </div>
      <div className="an-sim-value">{formatValue(prediction.value, locale)}</div>
      {(interval ?? probability) && <div className="small text-secondary">{interval ?? probability}</div>}
    </div>
  );
}

/** A small chart per input: the prediction along it, the others held. */
export function ProfileCurves({
  profile,
  values,
  look,
  renderer,
}: {
  profile: Profile;
  values: Record<string, unknown>;
  look: ChartLook;
  renderer?: "canvas" | "svg";
}) {
  const { t } = useT();
  return (
    <div className="an-sim-curves">
      {profile.curves.map((curve) => {
        const input = profile.inputs.find((i) => i.name === curve.input);
        if (!input) return null;
        return (
          <CurveChart
            key={curve.input}
            option={curveOption(curve, input, values, look)}
            renderer={renderer}
            label={t("The prediction as {input} changes", { input: curve.input })}
          />
        );
      })}
    </div>
  );
}

function CurveChart({ option, renderer, label }: { option: Option; renderer?: "canvas" | "svg"; label: string }) {
  return (
    <div className="an-sim-curve">
      <EChart option={option} renderer={renderer} label={label} />
    </div>
  );
}

/** The fit a simulation used, and whether its dataset changed since. */
export function FitNote({ profile }: { profile: Pick<Profile, "fit"> }) {
  const { t } = useT();
  return (
    <div className="small text-secondary">
      {t("Fit {fit}", { fit: profile.fit.name })}
      {profile.fit.dataset_changed && (
        <span className="text-warning ms-2">{t("The dataset has changed since this fit.")}</span>
      )}
    </div>
  );
}

// --- the scenarios compared --------------------------------------------------------

/** The scenarios' distributions on one chart, and a table of where they
 * differ and what each predicts. */
export function ComparisonView({
  comparison,
  look,
  renderer,
}: {
  comparison: Comparison;
  look: ChartLook;
  renderer?: "canvas" | "svg";
}) {
  const { t, locale } = useT();
  const option = useMemo(() => comparisonOption(comparison, look), [comparison, look]);
  // The inputs some scenario sets differently from another.
  const differing = comparison.inputs.filter((input) => {
    const seen = new Set(comparison.scenarios.map((s) => JSON.stringify(s.values[input.name])));
    return seen.size > 1;
  });
  return (
    <div className="an-sim-compare">
      <div className="an-sim-compare-chart">
        <EChart option={option} renderer={renderer} label={t("The scenarios' predicted distributions")} />
      </div>
      <Table size="sm" className="mb-0 small an-sim-table">
        <thead>
          <tr>
            <th>
              <T text="Scenario" />
            </th>
            {differing.map((i) => (
              <th key={i.name}>{i.name}</th>
            ))}
            <th>
              <T text="Predicted" />
            </th>
            <th>{comparison.outcome === "classification" ? t("Probability") : t("Interval")}</th>
          </tr>
        </thead>
        <tbody>
          {comparison.scenarios.map((s) => (
            <tr key={s.name}>
              <td>{s.name}</td>
              {differing.map((i) => (
                <td key={i.name}>{formatValue(s.values[i.name], locale)}</td>
              ))}
              <td className="fw-semibold">{formatValue(s.prediction.value, locale)}</td>
              <td className="text-secondary">
                {(comparison.outcome === "classification"
                  ? probabilityText(s.prediction, t)
                  : intervalText(s.prediction, t, locale)) ?? "—"}
              </td>
            </tr>
          ))}
        </tbody>
      </Table>
    </div>
  );
}

/** An error from the server, in place of what it stopped. */
export function SimError({ children }: { children: string }) {
  return (
    <Alert variant="warning" className="py-2 small mb-2">
      {children}
    </Alert>
  );
}
