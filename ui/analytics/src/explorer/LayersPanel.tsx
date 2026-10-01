// The layers panel (analytics TODO A2.10): the advanced level of the plot
// interface, which most people never open. It adds and removes layers,
// changes a layer's mark and stat, and sets the scales, reference lines and
// coordinates — each a change to the explorer's `Extras`, laid over the spec
// the drop zones made (`composeSpec`), so the drop zones keep working under
// it.

import { useState } from "react";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";

import { T, useT } from "../i18n";
import { layerKindName, markName, statName, summaryName } from "../labels";
import { AGGREGATES, MARKS, statOf, type Coord, type Mark, type PlotSpec, type Scale, type Stat } from "../plot/spec";
import {
  LAYER_KINDS,
  addLayer,
  addReference,
  defaultStat,
  removeLayer,
  removeReference,
  setScale,
  updateLayer,
  type Extras,
} from "./state";

/** The stats a layer can be given here; whether one suits the layer is the
 * server's to say. */
const STAT_KINDS: Stat["kind"][] = ["identity", "count", "aggregate", "summary", "boxplot", "quantiles", "density", "smooth"];

export function LayersPanel({
  spec,
  extras,
  onChange,
  onClose,
}: {
  /** The plot as drawn: the explorer's layers then the added ones. */
  spec: PlotSpec | null;
  extras: Extras;
  onChange: (extras: Extras) => void;
  onClose: () => void;
}) {
  const { t } = useT();
  const added = extras.layers.length;
  const own = spec ? spec.layers.length - added : 0;
  return (
    <aside className="an-layers" aria-label={t("Layers")}>
      <div className="d-flex align-items-center mb-2">
        <strong>
          <T text="Layers" />
        </strong>
        <Button size="sm" variant="link" className="ms-auto" onClick={onClose}>
          <T text="Close" />
        </Button>
      </div>
      {spec?.layers.map((layer, i) => {
        const mine = i >= own;
        const extraIndex = i - own;
        return (
          <div key={i} className="an-layer">
            <div className="d-flex align-items-center gap-2 mb-1">
              <span className="text-secondary small">{i + 1}.</span>
              {mine ? (
                <Form.Select
                  size="sm"
                  aria-label={t("Mark")}
                  value={layer.mark}
                  onChange={(e) => onChange(updateLayer(extras, extraIndex, { mark: e.target.value as Mark }))}
                >
                  {MARKS.map((m) => (
                    <option key={m} value={m}>
                      {markName(m, t)}
                    </option>
                  ))}
                </Form.Select>
              ) : (
                <span>{markName(layer.mark, t)}</span>
              )}
              {mine && (
                <Button
                  size="sm"
                  variant="outline-danger"
                  className="ms-auto"
                  aria-label={t("Remove layer")}
                  onClick={() => onChange(removeLayer(extras, extraIndex))}
                >
                  ×
                </Button>
              )}
            </div>
            <StatEditor
              stat={statOf(layer)}
              chosen={i === 0 ? extras.stat !== undefined : true}
              allowAuto={i === 0}
              onChange={(stat) =>
                i === 0
                  ? onChange({ ...extras, stat })
                  : mine && onChange(updateLayer(extras, extraIndex, { stat }))
              }
              readOnly={!mine && i !== 0}
            />
            {mine && spec.layers[0]?.encoding.color && (
              <Form.Check
                type="switch"
                id={`layer-color-${i}`}
                className="small mt-1"
                label={t("One per colour group")}
                checked={extras.layers[extraIndex]?.encoding?.color !== null}
                onChange={(e) => {
                  const encoding = { ...(extras.layers[extraIndex]?.encoding ?? {}) };
                  if (e.target.checked) delete encoding.color;
                  else encoding.color = null;
                  onChange(updateLayer(extras, extraIndex, { encoding }));
                }}
              />
            )}
          </div>
        );
      })}
      <Dropdown className="mb-3">
        <Dropdown.Toggle size="sm" variant="outline-primary" disabled={!spec}>
          <T text="Add layer" />
        </Dropdown.Toggle>
        <Dropdown.Menu>
          {LAYER_KINDS.map((k) => (
            <Dropdown.Item key={k.id} onClick={() => onChange(addLayer(extras, k.id))}>
              {layerKindName(k.id, t)}
            </Dropdown.Item>
          ))}
        </Dropdown.Menu>
      </Dropdown>

      <strong className="d-block mb-1">
        <T text="Scales" />
      </strong>
      {(["x", "y"] as const).map((c) => (
        <ScaleEditor key={c} channel={c} scale={extras.scales[c]} onChange={(s) => onChange(setScale(extras, c, s))} />
      ))}
      <Form.Group className="mb-3">
        <Form.Label className="small mb-0">
          <T text="Colour scale" />
        </Form.Label>
        <Form.Select
          size="sm"
          value={extras.scales.color?.scheme ?? ""}
          onChange={(e) =>
            onChange(setScale(extras, "color", e.target.value ? { ...(extras.scales.color ?? {}), scheme: e.target.value } : undefined))
          }
        >
          <option value="">{t("As chosen")}</option>
          <option value="sequential">{t("One hue, light to dark")}</option>
          <option value="diverging">{t("Diverging, through grey")}</option>
        </Form.Select>
      </Form.Group>

      <References extras={extras} onChange={onChange} />

      <Form.Group className="mb-3">
        <Form.Label className="small mb-0 fw-bold">
          <T text="Coordinates" />
        </Form.Label>
        <Form.Select
          size="sm"
          value={extras.coord ?? ""}
          onChange={(e) => onChange({ ...extras, coord: (e.target.value || undefined) as Coord | undefined })}
        >
          <option value="">{t("As chosen")}</option>
          <option value="cartesian">{t("X across, Y up")}</option>
          <option value="flipped">{t("Flipped: X up, Y across")}</option>
          <option value="polar">{t("Polar: X around, Y outwards")}</option>
        </Form.Select>
      </Form.Group>
    </aside>
  );
}

function StatEditor({
  stat,
  chosen,
  allowAuto,
  onChange,
  readOnly,
}: {
  stat: Stat;
  chosen: boolean;
  allowAuto: boolean;
  onChange: (stat: Stat | undefined) => void;
  readOnly: boolean;
}) {
  const { t } = useT();
  if (readOnly) return <div className="small text-secondary">{statName(stat.kind, t)}</div>;
  return (
    <div className="d-flex gap-1">
      <Form.Select
        size="sm"
        aria-label={t("Stat")}
        value={allowAuto && !chosen ? "" : stat.kind}
        onChange={(e) => onChange(e.target.value ? defaultStat(e.target.value as Stat["kind"]) : undefined)}
      >
        {allowAuto && <option value="">{t("As chosen ({stat})", { stat: statName(stat.kind, t) })}</option>}
        {STAT_KINDS.map((k) => (
          <option key={k} value={k}>
            {statName(k, t)}
          </option>
        ))}
      </Form.Select>
      {stat.kind === "smooth" && (
        <Form.Select
          size="sm"
          aria-label={t("Method")}
          value={stat.method ?? "linear"}
          onChange={(e) => onChange({ ...stat, method: e.target.value as "linear" | "loess" })}
        >
          <option value="linear">{t("Linear")}</option>
          <option value="loess">{t("Loess")}</option>
        </Form.Select>
      )}
      {stat.kind === "aggregate" && (
        <Form.Select
          size="sm"
          aria-label={t("Summary")}
          value={stat.function}
          onChange={(e) => onChange({ ...stat, function: e.target.value as typeof stat.function })}
        >
          {AGGREGATES.map((f) => (
            <option key={f} value={f}>
              {summaryName(f, t)}
            </option>
          ))}
        </Form.Select>
      )}
    </div>
  );
}

function ScaleEditor({
  channel,
  scale,
  onChange,
}: {
  channel: "x" | "y";
  scale: Scale | undefined;
  onChange: (scale: Scale | undefined) => void;
}) {
  const { t } = useT();
  const s = scale ?? {};
  const set = (change: Partial<Scale>) => {
    const next: Scale = { ...s, ...change };
    for (const k of Object.keys(next) as (keyof Scale)[]) if (next[k] === undefined || next[k] === false) delete next[k];
    onChange(Object.keys(next).length > 0 ? next : undefined);
  };
  return (
    <div className="d-flex align-items-center gap-1 mb-2">
      <span className="small" style={{ width: "1.2rem" }}>
        {channel.toUpperCase()}
      </span>
      <Form.Select
        size="sm"
        aria-label={t("{channel} scale", { channel: channel.toUpperCase() })}
        value={s.kind ?? "linear"}
        onChange={(e) => set({ kind: e.target.value === "linear" ? undefined : (e.target.value as Scale["kind"]) })}
      >
        <option value="linear">{t("Linear")}</option>
        <option value="log">{t("Log")}</option>
        <option value="sqrt">{t("Square root")}</option>
      </Form.Select>
      <Form.Select
        size="sm"
        aria-label={t("{channel} starts at zero", { channel: channel.toUpperCase() })}
        value={s.zero === undefined ? "" : String(s.zero)}
        onChange={(e) => set({ zero: e.target.value === "" ? undefined : e.target.value === "true" })}
      >
        <option value="">{t("Zero: as chosen")}</option>
        <option value="true">{t("From zero")}</option>
        <option value="false">{t("Fit the data")}</option>
      </Form.Select>
      <Form.Check
        type="checkbox"
        id={`reverse-${channel}`}
        label={t("Reverse")}
        className="small text-nowrap"
        checked={Boolean(s.reverse)}
        onChange={(e) => set({ reverse: e.target.checked || undefined })}
      />
    </div>
  );
}

function References({ extras, onChange }: { extras: Extras; onChange: (e: Extras) => void }) {
  const { t } = useT();
  const [channel, setChannel] = useState<"x" | "y">("y");
  const [value, setValue] = useState("");
  const [label, setLabel] = useState("");
  return (
    <div className="mb-3">
      <strong className="d-block mb-1">
        <T text="Reference lines" />
      </strong>
      {extras.references.map((r, i) => (
        <div key={i} className="d-flex align-items-center gap-2 small mb-1">
          <span>
            {r.channel.toUpperCase()} = {String(r.value)}
            {r.label ? ` (${r.label})` : ""}
          </span>
          <Button
            size="sm"
            variant="link"
            className="ms-auto p-0"
            aria-label={t("Remove reference line")}
            onClick={() => onChange(removeReference(extras, i))}
          >
            ×
          </Button>
        </div>
      ))}
      <Form
        className="d-flex gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          onChange(addReference(extras, channel, value, label));
          setValue("");
          setLabel("");
        }}
      >
        <Form.Select size="sm" style={{ width: "4rem" }} aria-label={t("Axis")} value={channel} onChange={(e) => setChannel(e.target.value as "x" | "y")}>
          <option value="x">X</option>
          <option value="y">Y</option>
        </Form.Select>
        <Form.Control size="sm" placeholder={t("at")} aria-label={t("Value")} value={value} onChange={(e) => setValue(e.target.value)} />
        <Form.Control size="sm" placeholder={t("label")} aria-label={t("Label")} value={label} onChange={(e) => setLabel(e.target.value)} />
        <Button size="sm" type="submit" variant="outline-primary" disabled={value.trim() === ""}>
          +
        </Button>
      </Form>
    </div>
  );
}
