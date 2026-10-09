// A drawn plot (analytics TODO A2.7): the spec and its data compiled to an
// ECharts option and shown in a box that fills its parent, redrawn when the
// option, the colour scheme or the box's size changes.
//
// Given `onPick` (a dashboard's tiles, A6.3), a click on an item and a range
// brushed along a continuous axis are read back as the values of the columns
// the plot's selections name (`select.ts`) and handed over.

import { useEffect, useMemo, useRef } from "react";

import { useT } from "../i18n";
import { selectionInfo, toOption, type Option } from "./echarts";
import { echarts } from "./runtime";
import { brushPicks, brushType, clickPicks, type BrushArea, type ClickParams, type Picked } from "./select";
import type { PlotData, PlotSpec } from "./spec";

/** How a pick was made: a click (with a modifier key, adding to what was
 * picked), or a brush (an empty one clears the last). */
export type PickHow = { by: "click"; add: boolean } | { by: "brush" };

export function PlotView({
  spec,
  data,
  theme,
  categorical,
  renderer = "canvas",
  still = false,
  onPick,
}: {
  spec: PlotSpec;
  data: PlotData;
  theme: "light" | "dark";
  /** The columns that are categories though their values are numbers. */
  categorical?: string[];
  /** Vector graphics, for print (a report's plots). */
  renderer?: "canvas" | "svg";
  /** Not responding to the pointer: no tooltips or highlighting (a report's). */
  still?: boolean;
  /** What a click or a brush picked (A6.3). */
  onPick?: (picks: Picked[], how: PickHow) => void;
}) {
  const { t } = useT();
  const box = useRef<HTMLDivElement>(null);
  const chart = useRef<ReturnType<typeof echarts.init> | null>(null);
  const picking = Boolean(onPick) && !still;
  const compile = useMemo(
    () => ({ theme, missing: t("(missing)"), countLabel: t("count"), categorical, still }),
    [theme, t, categorical, still],
  );
  const info = useMemo(() => (picking ? selectionInfo(spec, data, compile) : null), [picking, spec, data, compile]);
  const brush = useMemo(() => (info ? brushType(spec, info) : null), [info, spec]);
  const option = useMemo(() => {
    const o: Option = toOption(spec, data, compile);
    if (!brush) return o;
    const axes = brush === "lineX" ? { xAxisIndex: "all" } : brush === "lineY" ? { yAxisIndex: "all" } : { xAxisIndex: "all", yAxisIndex: "all" };
    return {
      ...o,
      brush: {
        ...axes,
        toolbox: [],
        brushType: brush,
        brushMode: "single",
        transformable: false,
        throttleType: "debounce",
        throttleDelay: 300,
        brushStyle: { borderWidth: 1, color: "rgba(120,140,180,0.15)", borderColor: "rgba(120,140,180,0.8)" },
        outOfBrush: { colorAlpha: 0.35 },
      },
    };
  }, [spec, data, compile, brush]);

  const latest = useRef({ option, spec, info, brush, onPick });
  latest.current = { option, spec, info, brush, onPick };

  /** Draw the option, and hold the brush ready, which a new option drops. */
  const draw = (instance: ReturnType<typeof echarts.init>) => {
    const { option: o, brush: b } = latest.current;
    instance.setOption(o, { notMerge: true });
    if (b) instance.dispatchAction({ type: "takeGlobalCursor", key: "brush", brushOption: { brushType: b, brushMode: "single" } });
  };

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const instance = echarts.init(el, undefined, { renderer });
    draw(instance);
    chart.current = instance;
    instance.on("click", (params: unknown) => {
      const { onPick: pick, info: i, spec: s } = latest.current;
      if (!pick || !i) return;
      const p = params as ClickParams & { event?: { event?: MouseEvent } };
      const picks = clickPicks(s, i, p);
      if (picks.length === 0) return;
      const e = p.event?.event;
      pick(picks, { by: "click", add: Boolean(e && (e.shiftKey || e.ctrlKey || e.metaKey)) });
    });
    instance.on("brushEnd", (params: unknown) => {
      const { onPick: pick, info: i, spec: s } = latest.current;
      if (!pick || !i) return;
      const areas = ((params as { areas?: BrushArea[] }).areas ?? []) as BrushArea[];
      pick(brushPicks(s, i, areas), { by: "brush" });
    });
    const observer = new ResizeObserver(() => instance.resize());
    observer.observe(el);
    return () => {
      observer.disconnect();
      instance.dispose();
      chart.current = null;
    };
    // `draw` reads the latest option through a ref.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [renderer]);

  useEffect(() => {
    if (chart.current) draw(chart.current);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [option, brush]);

  return <div ref={box} className={picking ? "an-plot an-plot-picking" : "an-plot"} role="img" aria-label={t("Plot")} />;
}
