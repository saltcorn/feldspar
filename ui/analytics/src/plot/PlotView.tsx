// A drawn plot (analytics TODO A2.7): the spec and its data compiled to an
// ECharts option and shown in a box that fills its parent, redrawn when the
// option, the colour scheme or the box's size changes.

import { useEffect, useMemo, useRef } from "react";

import { useT } from "../i18n";
import { toOption } from "./echarts";
import { echarts } from "./runtime";
import type { PlotData, PlotSpec } from "./spec";

export function PlotView({
  spec,
  data,
  theme,
  categorical,
  renderer = "canvas",
  still = false,
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
}) {
  const { t } = useT();
  const box = useRef<HTMLDivElement>(null);
  const chart = useRef<ReturnType<typeof echarts.init> | null>(null);
  const option = useMemo(
    () => toOption(spec, data, { theme, missing: t("(missing)"), countLabel: t("count"), categorical, still }),
    [spec, data, theme, t, categorical, still],
  );

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

  return <div ref={box} className="an-plot" role="img" aria-label={t("Plot")} />;
}
