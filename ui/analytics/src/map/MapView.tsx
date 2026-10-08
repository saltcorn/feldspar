// A drawn map (analytics TODO A5.6, A5.8–A5.13): a map spec and its data
// compiled to MapLibre sources and layers (`maplibre.ts`) over the base map of
// Settings → Maps, in a box that fills its parent, with a legend and a tooltip
// showing the row under the pointer. Loaded lazily, so MapLibre is fetched
// only when a map is shown.
//
// What changes often — the selection, a layer's opacity or visibility, its
// order — re-adds the map's layers but keeps its sources: a source is only
// replaced when its data is new, so a layer of thousands of features is not
// sent to MapLibre's worker again when one of them is clicked.
//
// In the Map workspace it is also where a selection starts: a click on a
// feature (`onFeatureClick`), a click on the map (`onMapClick`, for "within a
// distance of here"), and a lasso drawn with the pointer (`onLasso`). A map in
// a report is `still`: drawn once, then shown as an image of itself, which a
// browser prints where it would not print a WebGL canvas.

import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";

import type { SourceSpecification, StyleSpecification } from "@maplibre/maplibre-gl-style-spec";

import { useT } from "../i18n";
import { chartPalette } from "../plot/palette";
import { baseMapSettings, blankStyle, loadStyle, styleUrl } from "./baseMap";
import { lassoPolygon, type ScreenPoint } from "./lasso";
import { compileMap, styleFont, tooltipRows, type CompiledMap, type LegendEntry, type MapNote } from "./maplibre";
import { AttributionControl, MapLibreMap, NavigationControl } from "./runtime";
import { SHAPE_CSS, SHAPE_RATIO, shapeImage, shapeImageName, type ShapeName } from "./shapes";
import { mapBounds, type MapData, type MapSpec, type MapViewport } from "./spec";

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

/** What a pointer on the map does: pick features (and show their rows), draw
 * a lasso, or pick a point. */
export type MapTool = "pick" | "lasso" | "point";

/** Whether two compiled sources draw the same data: a GeoJSON source by the
 * very object, which a new answer replaces; any other by value. */
function sameSourceSpec(a: SourceSpecification | undefined, b: SourceSpecification): boolean {
  if (!a || a.type !== b.type) return false;
  if (a.type === "geojson" && b.type === "geojson") return a.data === b.data;
  return JSON.stringify(a) === JSON.stringify(b);
}

export function MapView({
  spec,
  data,
  theme,
  still = false,
  selection = null,
  tool = "pick",
  onFeatureClick,
  onMapClick,
  onLasso,
  onView,
  initialView,
}: {
  spec: MapSpec;
  data: MapData;
  theme: "light" | "dark";
  /** Not responding to the pointer, and shown as an image (a report's). */
  still?: boolean;
  /** The selected features of one layer, by its place in the spec. */
  selection?: { layer: number; ids: unknown[] } | null;
  tool?: MapTool;
  /** A feature clicked: its layer's place in the spec, its id, and whether a
   * modifier key adds to the selection. A click on nothing is `null`. */
  onFeatureClick?: (hit: { layer: number; id: unknown } | null, add: boolean) => void;
  /** A point picked with the point tool, in degrees. */
  onMapClick?: (lngLat: { lng: number; lat: number }) => void;
  /** A lasso drawn, as a GeoJSON polygon in degrees. */
  onLasso?: (polygon: GeoJSON.Polygon) => void;
  /** Where the map is looked at, after each move. */
  onView?: (view: MapViewport) => void;
  /** Where the map opens, when not `spec.view`: a workspace keeps its view
   * beside its spec, so moving the map does not draw its layers again. */
  initialView?: MapViewport;
}) {
  const { t } = useT();
  const box = useRef<HTMLDivElement>(null);
  const map = useRef<MapLibreMap | null>(null);
  const [ready, setReady] = useState(false);
  const [noWebGl, setNoWebGl] = useState(false);
  const [hover, setHover] = useState<Hover | null>(null);
  const [image, setImage] = useState<string | null>(null);
  const [lasso, setLasso] = useState<ScreenPoint[] | null>(null);
  const base = useBase(theme);
  const missing = t("(missing)");
  const words = useMemo(() => ({ density: t("Density"), low: t("low"), high: t("high") }), [t]);

  const compiled = useMemo<CompiledMap | null>(
    () =>
      base
        ? compileMap(spec, data, {
            theme,
            origin: window.location.origin,
            font: base.font,
            missing,
            selection,
            words,
          })
        : null,
    [spec, data, theme, base, missing, selection, words],
  );

  // The latest callbacks, so the map's own listeners need not be replaced
  // when they change.
  const handlers = useRef({ onFeatureClick, onMapClick, onLasso, onView });
  handlers.current = { onFeatureClick, onMapClick, onLasso, onView };

  // The map itself, made again when the base map (and so the theme) changes.
  const opening = useRef(initialView ?? spec.view);
  useEffect(() => {
    const el = box.current;
    if (!el || !base) return;
    let instance: MapLibreMap;
    try {
      const view = opening.current;
      instance = new MapLibreMap({
        container: el,
        style: base.style,
        interactive: !still,
        attributionControl: false,
        fadeDuration: still ? 0 : 300,
        // A report draws the canvas into an image.
        canvasContextAttributes: { preserveDrawingBuffer: still },
        ...(view ? { center: view.center, zoom: view.zoom } : {}),
      });
    } catch {
      setNoWebGl(true);
      return;
    }
    instance.addControl(new AttributionControl({ compact: true }));
    if (!still) instance.addControl(new NavigationControl({ showCompass: false }), "top-right");
    instance.on("load", () => setReady(true));
    instance.on("moveend", () => {
      const c = instance.getCenter();
      handlers.current.onView?.({ center: [round(c.lng, 6), round(c.lat, 6)], zoom: round(instance.getZoom(), 2) });
    });
    map.current = instance;
    return () => {
      setReady(false);
      map.current = null;
      instance.remove();
    };
  }, [base, still]);

  // The sources and layers: a source replaced only when its data is new; the
  // layers all taken off and put back in order.
  const added = useRef<{ layers: string[]; sources: Record<string, SourceSpecification> }>({ layers: [], sources: {} });
  const fitted = useRef<string | null>(opening.current ? "kept" : null);
  useEffect(() => {
    const m = map.current;
    if (!m || !ready || !compiled) return;
    for (const id of added.current.layers) if (m.getLayer(id)) m.removeLayer(id);
    for (const [id, source] of Object.entries(added.current.sources)) {
      if (!sameSourceSpec(source, compiled.sources[id]) && m.getSource(id)) m.removeSource(id);
    }
    for (const shape of compiled.images) addShape(m, shape);
    for (const [id, source] of Object.entries(compiled.sources)) if (!m.getSource(id)) m.addSource(id, source);
    for (const layer of compiled.layers) m.addLayer(layer);
    added.current = { layers: compiled.layers.map((l) => l.id), sources: compiled.sources };
    // Fitted to the data when it first arrives and whenever its extent moves,
    // unless the map was left somewhere.
    const bounds = mapBounds(data);
    const key = JSON.stringify(bounds);
    if (bounds && fitted.current !== key && fitted.current !== "kept") {
      fitted.current = key;
      m.fitBounds(
        [
          [bounds[0], bounds[1]],
          [bounds[2], bounds[3]],
        ],
        { padding: 32, maxZoom: 15, animate: false },
      );
    } else if (fitted.current === "kept" && bounds) {
      fitted.current = key;
    }
  }, [compiled, ready, data]);
  useEffect(() => {
    // A new map has none of the old one's layers or sources.
    added.current = { layers: [], sources: {} };
  }, [base]);

  // A report's map: once drawn, an image of itself.
  useEffect(() => {
    const m = map.current;
    if (!still || !m || !ready || !compiled) return;
    let live = true;
    setImage(null);
    m.once("idle", () => {
      if (!live) return;
      try {
        setImage(m.getCanvas().toDataURL("image/png"));
      } catch {
        // A tainted canvas stays a canvas.
      }
    });
    m.triggerRepaint();
    return () => {
      live = false;
    };
  }, [still, ready, compiled]);

  // The row under the pointer, and clicks.
  useEffect(() => {
    const m = map.current;
    if (!m || !ready || !compiled || still) return;
    const layers = compiled.interactive.filter((id) => m.getLayer(id));
    const hit = (point: { x: number; y: number }) => {
      const feature = layers.length > 0 ? m.queryRenderedFeatures([point.x, point.y], { layers })[0] : undefined;
      if (!feature) return null;
      const index = compiled.layerOf[String(feature.source)];
      return index === undefined ? null : { feature, index };
    };
    const move = (e: { point: { x: number; y: number } }) => {
      const found = tool === "lasso" ? null : hit(e.point);
      m.getCanvas().style.cursor = tool === "point" ? "crosshair" : found ? "pointer" : "";
      if (!found) {
        setHover(null);
        return;
      }
      const layer = data.layers[found.index]?.data;
      const popup = spec.layers[found.index]?.popup ?? [];
      const all = layer && layer.delivery !== "none" ? layer.properties : [];
      const columns = popup.length > 0 ? popup.map((name) => ({ name })) : all;
      setHover({ x: e.point.x, y: e.point.y, rows: tooltipRows(found.feature.properties ?? {}, columns, missing) });
    };
    const leave = () => setHover(null);
    const click = (e: { point: { x: number; y: number }; lngLat: { lng: number; lat: number }; originalEvent: MouseEvent }) => {
      if (tool === "point") {
        handlers.current.onMapClick?.({ lng: e.lngLat.lng, lat: e.lngLat.lat });
        return;
      }
      if (tool !== "pick") return;
      const found = hit(e.point);
      const add = e.originalEvent.shiftKey || e.originalEvent.ctrlKey || e.originalEvent.metaKey;
      const id = found?.feature.id;
      handlers.current.onFeatureClick?.(found && id !== undefined ? { layer: found.index, id } : null, add);
    };
    m.on("mousemove", move);
    m.on("mouseout", leave);
    m.on("click", click);
    return () => {
      m.off("mousemove", move);
      m.off("mouseout", leave);
      m.off("click", click);
    };
  }, [compiled, ready, still, data, missing, tool, spec.layers]);

  // The lasso: the map holds still while it is drawn.
  useEffect(() => {
    const m = map.current;
    if (!m || !ready || still) return;
    if (tool === "lasso") m.dragPan.disable();
    else m.dragPan.enable();
  }, [tool, ready, still]);
  const lassoDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    e.currentTarget.setPointerCapture(e.pointerId);
    setLasso([[e.clientX - rect.left, e.clientY - rect.top]]);
  };
  const lassoMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!lasso) return;
    const rect = e.currentTarget.getBoundingClientRect();
    setLasso([...lasso, [e.clientX - rect.left, e.clientY - rect.top]]);
  };
  const lassoUp = () => {
    const m = map.current;
    const drawn = lasso;
    setLasso(null);
    if (!m || !drawn) return;
    const polygon = lassoPolygon(drawn, (p) => {
      const ll = m.unproject(p);
      return [ll.lng, ll.lat];
    });
    if (polygon) handlers.current.onLasso?.(polygon);
  };

  const notes = compiled ? mapNotes(compiled.notes, t) : [];
  if (base?.failed) notes.unshift(t("The base map could not be loaded, so the layers are drawn on a plain background."));

  return (
    <div className="an-map-frame">
      <div
        ref={box}
        className={image ? "an-map an-map-drawn" : "an-map"}
        role="img"
        aria-label={t("Map")}
      />
      {still && image && <img className="an-map-image" src={image} alt={t("Map")} />}
      {still && !image && !noWebGl && <span className="an-panel-loading" hidden />}
      {tool === "lasso" && !still && (
        <div
          className="an-map-lasso"
          onPointerDown={lassoDown}
          onPointerMove={lassoMove}
          onPointerUp={lassoUp}
          onPointerCancel={() => setLasso(null)}
        >
          {lasso && lasso.length > 1 && (
            <svg width="100%" height="100%">
              <polygon points={lasso.map((p) => p.join(",")).join(" ")} />
            </svg>
          )}
        </div>
      )}
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

function round(v: number, digits: number): number {
  const f = 10 ** digits;
  return Math.round(v * f) / f;
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

/** The legend: a block per encoded channel, under its layer's name. */
function Legend({ entries, theme }: { entries: LegendEntry[]; theme: "light" | "dark" }) {
  const muted = chartPalette(theme).muted;
  return (
    <div className="an-map-legend">
      {entries.map((e, i) => (
        <div key={`${e.layer}-${e.channel}`} className="an-map-legend-entry">
          {e.title && (i === 0 || entries[i - 1].layer !== e.layer) && (
            <div className="an-map-legend-layer">{e.title}</div>
          )}
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
