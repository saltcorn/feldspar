// One layer's settings in the Map workspace (analytics TODO A5.8–A5.9): its
// name, where its geometry comes from, a filter over its rows, its style, the
// columns on Color, Size, Shape and Label, the fields its popup shows, its
// opacity and its legend.
//
// The style is the map's classification over the plot encodings: one colour,
// categories, graduated colours in classes (quantile, equal interval or
// natural breaks, computed on the server), proportional symbols, a heatmap.
// Choosing one fills the channel it needs when it is empty.

import { useEffect, useState } from "react";
import Form from "react-bootstrap/Form";

import { T, useT } from "../i18n";
import type { Classification, MapEncoding, MapLayer, SourceChoice, StyleKind } from "./spec";
import { isMeasure, setChannel, setStyle, sourceOptions, type ColumnInfo } from "./workspace";

const STYLE_KINDS: StyleKind[] = ["auto", "single", "categories", "graduated", "proportional", "heatmap"];
const METHODS: Classification[] = ["natural_breaks", "quantile", "equal_interval"];
const CHANNELS: (keyof MapEncoding)[] = ["color", "size", "shape", "label"];

export function styleName(kind: StyleKind, t: (s: string) => string): string {
  switch (kind) {
    case "auto":
      return t("Automatic");
    case "single":
      return t("Single symbol");
    case "categories":
      return t("Categories");
    case "graduated":
      return t("Graduated colours");
    case "proportional":
      return t("Proportional symbols");
    case "heatmap":
      return t("Heatmap");
  }
}

function methodName(m: Classification, t: (s: string) => string): string {
  switch (m) {
    case "natural_breaks":
      return t("Natural breaks");
    case "quantile":
      return t("Quantiles");
    case "equal_interval":
      return t("Equal intervals");
  }
}

function channelName(c: keyof MapEncoding, t: (s: string) => string): string {
  switch (c) {
    case "color":
      return t("Color");
    case "size":
      return t("Size");
    case "shape":
      return t("Shape");
    case "label":
      return t("Label");
  }
}

/** The columns a channel offers: a number on Size; anything but geometry
 * elsewhere. */
export function channelColumns(channel: keyof MapEncoding, columns: ColumnInfo[]): ColumnInfo[] {
  const drawable = columns.filter((c) => c.type !== "geometry" && c.type !== "json" && c.type !== "bytes");
  return channel === "size" ? drawable.filter(isMeasure) : drawable;
}

export function LayerSettings({
  layer,
  columns,
  sources,
  onChange,
}: {
  layer: MapLayer;
  columns: ColumnInfo[];
  /** Where the dataset's rows can get their geometry, as `suggestMap` says. */
  sources: SourceChoice[];
  onChange: (change: (l: MapLayer) => MapLayer) => void;
}) {
  const { t } = useT();
  const style = layer.style ?? { kind: "auto" as const };
  const [filter, setFilter] = useState(layer.filter ?? "");
  const [name, setName] = useState(layer.name ?? "");
  useEffect(() => setFilter(layer.filter ?? ""), [layer.id, layer.filter]);
  useEffect(() => setName(layer.name ?? ""), [layer.id, layer.name]);
  const id = (what: string) => `layer-${layer.id}-${what}`;
  const options = sourceOptions(layer.geometry, sources);
  const sourceIndex = options.findIndex((o) => JSON.stringify(o.source) === JSON.stringify(layer.geometry));
  const applyFilter = () =>
    onChange((l) => {
      const next = { ...l };
      if (filter.trim() === "") delete next.filter;
      else next.filter = filter.trim();
      return next;
    });

  return (
    <div className="an-layer-settings">
      <Form.Group className="mb-2">
        <Form.Label htmlFor={id("name")} className="small mb-0">
          <T text="Name" />
        </Form.Label>
        <Form.Control
          id={id("name")}
          size="sm"
          value={name}
          onChange={(e) => setName(e.target.value)}
          onBlur={() => onChange((l) => ({ ...l, name: name.trim() || l.name }))}
        />
      </Form.Group>

      <Form.Group className="mb-2">
        <Form.Label htmlFor={id("geometry")} className="small mb-0">
          <T text="Geometry" />
        </Form.Label>
        <Form.Select
          id={id("geometry")}
          size="sm"
          value={sourceIndex}
          onChange={(e) => {
            const picked = options[Number(e.target.value)];
            if (picked) onChange((l) => ({ ...l, geometry: picked.source }));
          }}
        >
          {options.map((o, i) => (
            <option key={o.label} value={i}>
              {o.label.replace(/`/g, "")}
            </option>
          ))}
        </Form.Select>
      </Form.Group>

      <Form.Group className="mb-2">
        <Form.Label htmlFor={id("filter")} className="small mb-0">
          <T text="Filter" />
        </Form.Label>
        <Form.Control
          id={id("filter")}
          size="sm"
          className="font-monospace"
          placeholder={t("A condition, e.g. category == \"burglary\"")}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onBlur={applyFilter}
          onKeyDown={(e) => {
            if (e.key === "Enter") applyFilter();
          }}
        />
      </Form.Group>

      <Form.Group className="mb-2">
        <Form.Label htmlFor={id("style")} className="small mb-0">
          <T text="Style" />
        </Form.Label>
        <Form.Select
          id={id("style")}
          size="sm"
          value={style.kind}
          onChange={(e) => onChange((l) => setStyle(l, e.target.value as StyleKind, columns))}
        >
          {STYLE_KINDS.map((k) => (
            <option key={k} value={k}>
              {styleName(k, t)}
            </option>
          ))}
        </Form.Select>
      </Form.Group>
      {style.kind === "single" && (
        <Form.Group className="mb-2 d-flex align-items-center gap-2">
          <Form.Label htmlFor={id("colour")} className="small mb-0">
            <T text="Colour" />
          </Form.Label>
          <Form.Control
            id={id("colour")}
            type="color"
            size="sm"
            value={style.color ?? "#2a78d6"}
            onChange={(e) => onChange((l) => ({ ...l, style: { kind: "single", color: e.target.value } }))}
          />
        </Form.Group>
      )}
      {style.kind === "graduated" && (
        <div className="d-flex gap-2 mb-2">
          <Form.Select
            size="sm"
            aria-label={t("Classes by")}
            value={style.method}
            onChange={(e) =>
              onChange((l) => ({ ...l, style: { ...style, method: e.target.value as Classification } }))
            }
          >
            {METHODS.map((m) => (
              <option key={m} value={m}>
                {methodName(m, t)}
              </option>
            ))}
          </Form.Select>
          <Form.Select
            size="sm"
            aria-label={t("Classes")}
            style={{ width: "6rem" }}
            value={style.classes}
            onChange={(e) => onChange((l) => ({ ...l, style: { ...style, classes: Number(e.target.value) } }))}
          >
            {[2, 3, 4, 5, 6, 7].map((n) => (
              <option key={n} value={n}>
                {t("{count} classes", { count: n })}
              </option>
            ))}
          </Form.Select>
        </div>
      )}
      {style.kind === "heatmap" && (
        <Form.Group className="mb-2">
          <Form.Label htmlFor={id("radius")} className="small mb-0">
            {t("Radius: {px} px", { px: style.radius ?? 20 })}
          </Form.Label>
          <Form.Range
            id={id("radius")}
            min={2}
            max={60}
            value={style.radius ?? 20}
            onChange={(e) => onChange((l) => ({ ...l, style: { kind: "heatmap", radius: Number(e.target.value) } }))}
          />
        </Form.Group>
      )}

      <div className="an-layer-channels mb-2">
        {CHANNELS.map((channel) => {
          // A single colour or a heatmap draws nothing on Color.
          const unused =
            channel === "color" && (style.kind === "single" || style.kind === "heatmap");
          return (
            <Form.Group key={channel} className="d-flex align-items-center gap-2">
              <Form.Label htmlFor={id(channel)} className={unused ? "small mb-0 text-secondary" : "small mb-0"}>
                {style.kind === "heatmap" && channel === "size" ? t("Weight") : channelName(channel, t)}
              </Form.Label>
              <Form.Select
                id={id(channel)}
                size="sm"
                disabled={unused}
                value={layer.encoding?.[channel]?.field ?? ""}
                onChange={(e) => onChange((l) => setChannel(l, channel, e.target.value || null))}
              >
                <option value="">{t("(none)")}</option>
                {channelColumns(channel, columns).map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name}
                  </option>
                ))}
              </Form.Select>
            </Form.Group>
          );
        })}
      </div>

      <Form.Group className="mb-2">
        <Form.Label htmlFor={id("opacity")} className="small mb-0">
          {t("Opacity: {percent}%", { percent: Math.round((layer.opacity ?? 1) * 100) })}
        </Form.Label>
        <Form.Range
          id={id("opacity")}
          min={0}
          max={100}
          value={Math.round((layer.opacity ?? 1) * 100)}
          onChange={(e) =>
            onChange((l) => {
              const next: MapLayer = { ...l, opacity: Number(e.target.value) / 100 };
              if (next.opacity === 1) delete next.opacity;
              return next;
            })
          }
        />
      </Form.Group>
      <Form.Check
        type="switch"
        id={id("legend")}
        className="small mb-2"
        label={t("In the legend")}
        checked={layer.legend !== false}
        onChange={(e) =>
          onChange((l) => {
            const next = { ...l };
            if (e.target.checked) delete next.legend;
            else next.legend = false;
            return next;
          })
        }
      />

      <details className="small">
        <summary>
          <T text="Popup fields" />
          {(layer.popup?.length ?? 0) > 0 && ` (${layer.popup?.length})`}
        </summary>
        <p className="text-secondary mb-1">
          <T text="What the popup over a feature shows; every column when none is ticked." />
        </p>
        {columns
          .filter((c) => c.type !== "geometry")
          .map((c) => (
            <Form.Check
              key={c.name}
              id={id(`popup-${c.name}`)}
              label={c.name}
              checked={layer.popup?.includes(c.name) ?? false}
              onChange={(e) =>
                onChange((l) => {
                  const current = l.popup ?? [];
                  const popup = e.target.checked
                    ? columns.map((x) => x.name).filter((n) => n === c.name || current.includes(n))
                    : current.filter((n) => n !== c.name);
                  const next: MapLayer = { ...l, popup };
                  if (popup.length === 0) delete next.popup;
                  return next;
                })
              }
            />
          ))}
      </details>
    </div>
  );
}
