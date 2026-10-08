// A drawn map (analytics TODO A5.6): a map spec and its data compiled to
// MapLibre sources and layers (`maplibre.ts`) over the base map of Settings →
// Maps, in a box that fills its parent, with a legend and a tooltip showing
// the row under the pointer. Loaded lazily, so MapLibre is fetched only when a
// map is shown.

import { useEffect, useMemo, useRef, useState } from "react";

import type { StyleSpecification } from "@maplibre/maplibre-gl-style-spec";

import { useT } from "../i18n";
import { chartPalette } from "../plot/palette";
import { baseMapSettings, blankStyle, loadStyle, styleUrl } from "./baseMap";
import { compileMap, styleFont, tooltipRows, type CompiledMap, type LegendEntry, type MapNote } from "./maplibre";
import { AttributionControl, MapLibreMap, NavigationControl } from "./runtime";
import { SHAPE_CSS, SHAPE_RATIO, shapeImage, shapeImageName, type ShapeName } from "./shapes";
import { mapBounds, type MapData, type MapSpec } from "./spec";

/** The base style a map is drawn over, once it is known. */
type Base = { style: StyleSpecification; font: string[] | null; failed: boolean };

function useBase(theme: "light" | "dark"): Base | null {
  const [base, setBase] = useState<Base | null>(null);
  useEffect(() => {
    let live = true;
    void (async () => {
      const url = styleUrl(await baseMapSettings(), theme);
      const style = url ? await loadStyle(url) : null;
      if (!live) return;
      setBase(
        style
          ? { style, font: styleFont(style), failed: false }
          : { style: blankStyle(theme), font: null, failed: url !== null },
      );
    })();
    return () => {
      live = false;
    };
  }, [theme]);
  return base;
}

/** What is under the pointer. */
type Hover = { x: number; y: number; rows: [string, string][] };

export function MapView({
  spec,
  data,
  theme,
  still = false,
}: {
  spec: MapSpec;
  data: MapData;
  theme: "light" | "dark";
  /** Not responding to the pointer (a report's). */
  still?: boolean;
}) {
  const { t } = useT();
  const box = useRef<HTMLDivElement>(null);
  const map = useRef<MapLibreMap | null>(null);
  const [ready, setReady] = useState(false);
  const [noWebGl, setNoWebGl] = useState(false);
  const [hover, setHover] = useState<Hover | null>(null);
  const base = useBase(theme);
  const missing = t("(missing)");

  const compiled = useMemo<CompiledMap | null>(
    () =>
      base
        ? compileMap(spec, data, { theme, origin: window.location.origin, font: base.font, missing })
        : null,
    [spec, data, theme, base, missing],
  );

  // The map itself, made again when the base map (and so the theme) changes.
  useEffect(() => {
    const el = box.current;
    if (!el || !base) return;
    let instance: MapLibreMap;
    try {
      instance = new MapLibreMap({
        container: el,
        style: base.style,
        interactive: !still,
        attributionControl: false,
        fadeDuration: still ? 0 : 300,
        // A report prints the canvas.
        canvasContextAttributes: { preserveDrawingBuffer: still },
      });
    } catch {
      setNoWebGl(true);
      return;
    }
    instance.addControl(new AttributionControl({ compact: true }));
    if (!still) instance.addControl(new NavigationControl({ showCompass: false }), "top-right");
    instance.on("load", () => setReady(true));
    map.current = instance;
    return () => {
      setReady(false);
      map.current = null;
      instance.remove();
    };
  }, [base, still]);

  // The layers: the previous ones taken off, the new ones put on.
  const added = useRef<{ layers: string[]; sources: string[] }>({ layers: [], sources: [] });
  const fitted = useRef<string | null>(null);
  useEffect(() => {
    const m = map.current;
    if (!m || !ready || !compiled) return;
    for (const id of added.current.layers) if (m.getLayer(id)) m.removeLayer(id);
    for (const id of added.current.sources) if (m.getSource(id)) m.removeSource(id);
    for (const shape of compiled.images) addShape(m, shape);
    for (const [id, source] of Object.entries(compiled.sources)) m.addSource(id, source);
    for (const layer of compiled.layers) m.addLayer(layer);
    added.current = { layers: compiled.layers.map((l) => l.id), sources: Object.keys(compiled.sources) };
    // Fitted to the data when it first arrives and whenever its extent moves.
    const bounds = mapBounds(data);
    const key = JSON.stringify(bounds);
    if (bounds && fitted.current !== key) {
      fitted.current = key;
      m.fitBounds(
        [
          [bounds[0], bounds[1]],
          [bounds[2], bounds[3]],
        ],
        { padding: 32, maxZoom: 15, animate: false },
      );
    }
  }, [compiled, ready, data]);
  useEffect(() => {
    // A new map has none of the old one's layers, and is fitted afresh.
    added.current = { layers: [], sources: [] };
    fitted.current = null;
  }, [base]);

  // The row under the pointer.
  useEffect(() => {
    const m = map.current;
    if (!m || !ready || !compiled || still) return;
    const layers = compiled.interactive.filter((id) => m.getLayer(id));
    const move = (e: { point: { x: number; y: number } }) => {
      const feature = layers.length > 0 ? m.queryRenderedFeatures([e.point.x, e.point.y], { layers })[0] : undefined;
      m.getCanvas().style.cursor = feature ? "pointer" : "";
      if (!feature) {
        setHover(null);
        return;
      }
      const index = Number(String(feature.source).replace(/^fd-/, ""));
      const layer = data.layers[index]?.data;
      const columns = layer && layer.delivery !== "none" ? layer.properties : [];
      setHover({ x: e.point.x, y: e.point.y, rows: tooltipRows(feature.properties ?? {}, columns, missing) });
    };
    const leave = () => setHover(null);
    m.on("mousemove", move);
    m.on("mouseout", leave);
    return () => {
      m.off("mousemove", move);
      m.off("mouseout", leave);
    };
  }, [compiled, ready, still, data, missing]);

  const notes = compiled ? mapNotes(compiled.notes, t) : [];
  if (base?.failed) notes.unshift(t("The base map could not be loaded, so the layers are drawn on a plain background."));

  return (
    <div className="an-map-frame">
      <div ref={box} className="an-map" role="img" aria-label={t("Map")} />
      {noWebGl && (
        <div className="an-map-failure">{t("This browser cannot draw maps: WebGL is turned off or not available.")}</div>
      )}
      {compiled && compiled.legend.length > 0 && <Legend entries={compiled.legend} theme={theme} />}
      {hover && hover.rows.length > 0 && (
        <div className="an-map-tooltip" style={{ left: hover.x + 12, top: hover.y + 12 }}>
          <table>
            <tbody>
              {hover.rows.slice(0, 12).map(([k, v]) => (
                <tr key={k}>
                  <th>{k}</th>
                  <td>{v}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {notes.length > 0 && (
        <ul className="an-map-notes">
          {notes.map((n) => (
            <li key={n}>{n}</li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Add a shape's SDF image to the map, once. */
function addShape(m: MapLibreMap, shape: ShapeName) {
  const name = shapeImageName(shape);
  if (m.hasImage(name)) return;
  m.addImage(name, shapeImage(shape), { sdf: true, pixelRatio: SHAPE_RATIO });
}

/** What the reader should know, in sentences. */
export function mapNotes(
  notes: MapNote[],
  t: (text: string, args?: Record<string, string | number>) => string,
): string[] {
  return notes.map((n) => {
    switch (n.kind) {
      case "size-on-polygons":
        return t("Size does not change a region's area; {column} is not drawn by size.", { column: n.field });
      case "shape-not-points":
        return t("Shape applies to points only; {column} is not drawn by shape.", { column: n.field });
      case "labels-need-fonts":
        return t("Labels need a base map with fonts; {column} is not drawn as a label.", { column: n.field });
      case "many-values":
        return t("{column} has {count} values; those past the eighth share one colour.", {
          column: n.field,
          count: n.count,
        });
      case "refused":
        return n.error;
    }
  });
}

/** The legend: a block per encoded channel. */
function Legend({ entries, theme }: { entries: LegendEntry[]; theme: "light" | "dark" }) {
  const muted = chartPalette(theme).muted;
  return (
    <div className="an-map-legend">
      {entries.map((e) => (
        <div key={`${e.layer}-${e.channel}`} className="an-map-legend-entry">
          <div className="an-map-legend-title">{e.field}</div>
          {e.items?.map((item) => (
            <div key={item.label} className="an-map-legend-item">
              <Swatch color={item.color} shape={item.shape} />
              {item.label}
            </div>
          ))}
          {e.gradient && (
            <div className="an-map-legend-item">
              <span>{e.gradient.min}</span>
              <span
                className="an-map-legend-ramp"
                style={{ background: `linear-gradient(to right, ${e.gradient.colors.join(", ")})` }}
              />
              <span>{e.gradient.max}</span>
            </div>
          )}
          {e.sizes && (
            <div className="an-map-legend-item">
              <Swatch color={muted} px={e.sizes.minPx} />
              {e.sizes.min}
              <Swatch color={muted} px={e.sizes.maxPx} />
              {e.sizes.max}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

/** A legend's symbol, drawn as the map draws it. */
function Swatch({ color, shape = "circle", px = SHAPE_CSS }: { color: string; shape?: ShapeName; px?: number }) {
  const path = SWATCH_PATHS[shape];
  return (
    <svg className="an-map-swatch" width={px} height={px} viewBox="-1 -1 2 2" aria-hidden="true">
      {path ? <path d={path} fill={color} /> : <circle r={1} fill={color} />}
    </svg>
  );
}

/** The shapes' outlines as SVG paths in the unit square (`shapes.ts`). */
const SWATCH_PATHS: Record<ShapeName, string | null> = {
  circle: null,
  square: "M-0.886 -0.886H0.886V0.886H-0.886Z",
  triangle: "M0 -1L0.95 0.75H-0.95Z",
  diamond: "M0 -1L1 0L0 1L-1 0Z",
  cross: "M-0.32 -1H0.32V-0.32H1V0.32H0.32V1H-0.32V0.32H-1V-0.32H-0.32Z",
  star: "M0 -1L0.26 -0.36L0.95 -0.31L0.42 0.14L0.59 0.81L0 0.45L-0.59 0.81L-0.42 0.14L-0.95 -0.31L-0.26 -0.36Z",
};
