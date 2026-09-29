// The three plots a posterior is read by (Stan TODO §18): one element's **trace
// per chain** (did the chains mix?), its **histogram** (what is its posterior?),
// and for a one-axis labelled variable the **forest plot** — an interval per
// group, which is how a hierarchical model is read.
//
// Plain SVG, sized to the container, coloured through `admin.css`'s `.viz`
// tokens so both themes have their own steps. Chains are a categorical series
// in the palette's fixed order and are never cycled (a ninth chain is the
// neutral "other"). Every plot has a hover readout, and the numbers are also in
// the summary table beside it, so nothing here is readable by colour alone.

import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";

import {
  formatNumber,
  histogram,
  niceTicks,
  type ChainTrace,
  type ForestRow,
} from "../models";
import { T, useT } from "../i18n";

/** The palette has eight categorical slots; later chains share the neutral. */
const SLOTS = 8;

/** A chain's colour class. */
export function chainClass(chain: number): string {
  return chain >= 1 && chain <= SLOTS ? `viz-s${chain}` : "viz-s0";
}

/** The width of the element `ref` is on, followed as it resizes. */
function useWidth(fallback = 640) {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(fallback);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return undefined;
    setWidth(el.clientWidth || fallback);
    if (typeof ResizeObserver === "undefined") return undefined;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth || fallback));
    observer.observe(el);
    return () => observer.disconnect();
  }, [fallback]);
  return { ref, width };
}

/** A linear scale from `[d0, d1]` onto `[r0, r1]`. */
function scale(d0: number, d1: number, r0: number, r1: number) {
  const span = d1 - d0 || 1;
  return (v: number) => r0 + ((v - d0) / span) * (r1 - r0);
}

/** The finite extent of some numbers, padded a little so a line at the edge is
 * not drawn on the frame. */
function extent(values: number[]): [number, number] {
  let lo = Infinity;
  let hi = -Infinity;
  for (const v of values) {
    if (!Number.isFinite(v)) continue;
    if (v < lo) lo = v;
    if (v > hi) hi = v;
  }
  if (!Number.isFinite(lo)) return [0, 1];
  if (lo === hi) return [lo - 1, hi + 1];
  const pad = (hi - lo) * 0.04;
  return [lo - pad, hi + pad];
}

/** A tooltip at `(x, y)` inside the plot's box. */
function Tooltip({ x, y, width, children }: { x: number; y: number; width: number; children: ReactNode }) {
  // Flipped to the left of the pointer in the right half, so it never runs
  // off the card.
  const left = x > width / 2;
  return (
    <div
      className="viz-tooltip"
      style={left ? { right: width - x + 12, top: y } : { left: x + 12, top: y }}
    >
      {children}
    </div>
  );
}

const MARGIN = { top: 8, right: 12, bottom: 24, left: 52 };

/**
 * One element's draws, chain by chain, against the iteration — the plot that
 * says whether the chains mixed: four hairy caterpillars on top of each other,
 * or one of them somewhere else.
 */
export function TracePlot({ traces, height = 220 }: { traces: ChainTrace[]; height?: number }) {
  const { t } = useT();
  const { ref, width } = useWidth();
  const [hover, setHover] = useState<number | null>(null);
  const length = Math.max(0, ...traces.map((c) => c.values.length));
  const [y0, y1] = useMemo(() => extent(traces.flatMap((c) => c.values)), [traces]);
  const innerW = Math.max(10, width - MARGIN.left - MARGIN.right);
  const innerH = height - MARGIN.top - MARGIN.bottom;
  const x = scale(0, Math.max(1, length - 1), MARGIN.left, MARGIN.left + innerW);
  const y = scale(y0, y1, MARGIN.top + innerH, MARGIN.top);
  const yTicks = niceTicks(y0, y1, 4);
  const xTicks = niceTicks(1, Math.max(1, length), 5).filter((v) => Number.isInteger(v));

  const paths = useMemo(
    () =>
      traces.map((c) => {
        // A NaN draw is a gap in the line, not a zero.
        let d = "";
        let pen = false;
        c.values.forEach((v, i) => {
          if (!Number.isFinite(v)) {
            pen = false;
            return;
          }
          d += `${pen ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`;
          pen = true;
        });
        return { chain: c.chain, d };
      }),
    // `x` and `y` are functions of these, made afresh each render.
    [traces, innerW, innerH, y0, y1, length],
  );

  // The box is rendered even while empty, so its width is measured from the
  // start.
  if (length === 0) return <div className="viz" ref={ref} />;
  const onMove = (e: React.MouseEvent<SVGRectElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const i = Math.round(((e.clientX - box.left) / box.width) * (length - 1));
    setHover(Math.max(0, Math.min(length - 1, i)));
  };

  return (
    <div className="viz" ref={ref}>
      <ChainLegend chains={traces.map((c) => c.chain)} />
      <svg
        height={height}
        role="img"
        aria-label={t("Trace plot: {count} chains of {length} draws", {
          count: traces.length,
          length,
        })}
      >
        {yTicks.map((v) => (
          <g key={`y${v}`}>
            <line className="viz-grid" x1={MARGIN.left} x2={MARGIN.left + innerW} y1={y(v)} y2={y(v)} />
            <text x={MARGIN.left - 6} y={y(v)} dy="0.32em" textAnchor="end">
              {formatNumber(v, 3)}
            </text>
          </g>
        ))}
        {xTicks.map((v) => (
          <text key={`x${v}`} x={x(v - 1)} y={height - 6} textAnchor="middle">
            {v}
          </text>
        ))}
        {paths.map((p) => (
          <path key={p.chain} className={`viz-line ${chainClass(p.chain)}`} d={p.d} />
        ))}
        {hover !== null && (
          <line
            className="viz-crosshair"
            x1={x(hover)}
            x2={x(hover)}
            y1={MARGIN.top}
            y2={MARGIN.top + innerH}
          />
        )}
        <rect
          className="viz-hit"
          x={MARGIN.left}
          y={MARGIN.top}
          width={innerW}
          height={innerH}
          onMouseMove={onMove}
          onMouseLeave={() => setHover(null)}
        />
      </svg>
      {hover !== null && (
        <Tooltip x={x(hover)} y={MARGIN.top + 24} width={width}>
          <div className="text-muted">{t("Draw {n}", { n: hover + 1 })}</div>
          {traces.map((c) => (
            <div key={c.chain}>
              <span className={`viz-key ${chainClass(c.chain)}`} />
              {t("Chain {chain}", { chain: c.chain })}: <strong>{formatNumber(c.values[hover])}</strong>
            </div>
          ))}
        </Tooltip>
      )}
    </div>
  );
}

/** The chains' legend — always there for two or more, because identity by
 * colour alone is not identity. */
export function ChainLegend({ chains }: { chains: number[] }) {
  const { t } = useT();
  if (chains.length < 2) return null;
  return (
    <div className="d-flex flex-wrap gap-3 small text-secondary mb-1">
      {chains.map((chain) => (
        <span key={chain}>
          <span className={`viz-key ${chainClass(chain)}`} />
          {t("Chain {chain}", { chain })}
        </span>
      ))}
    </div>
  );
}

/** The pooled draws of one element as a histogram. */
export function HistogramPlot({ values, height = 220 }: { values: number[]; height?: number }) {
  const { t } = useT();
  const { ref, width } = useWidth();
  const [hover, setHover] = useState<number | null>(null);
  const bins = useMemo(() => histogram(values), [values]);
  if (bins.length === 0) return <div className="viz" ref={ref} />;
  const innerW = Math.max(10, width - MARGIN.left - MARGIN.right);
  const innerH = height - MARGIN.top - MARGIN.bottom;
  const lo = bins[0].x0;
  const hi = bins[bins.length - 1].x1;
  const most = Math.max(...bins.map((b) => b.count));
  const x = scale(lo, hi === lo ? lo + 1 : hi, MARGIN.left, MARGIN.left + innerW);
  const y = scale(0, most, MARGIN.top + innerH, MARGIN.top);
  const slot = innerW / bins.length;
  // A 2px surface gap between touching bars, and no bar wider than 24px.
  const barW = Math.max(1, Math.min(24, slot - 2));
  const baseline = MARGIN.top + innerH;

  return (
    <div className="viz" ref={ref}>
      <svg
        height={height}
        role="img"
        aria-label={t("Histogram of {count} draws", { count: values.length })}
      >
        {niceTicks(0, most, 3).map((v) => (
          <g key={`y${v}`}>
            <line className="viz-grid" x1={MARGIN.left} x2={MARGIN.left + innerW} y1={y(v)} y2={y(v)} />
            <text x={MARGIN.left - 6} y={y(v)} dy="0.32em" textAnchor="end">
              {v}
            </text>
          </g>
        ))}
        {niceTicks(lo, hi, 5).map((v) => (
          <text key={`x${v}`} x={x(v)} y={height - 6} textAnchor="middle">
            {formatNumber(v, 3)}
          </text>
        ))}
        {bins.map((b, i) => {
          const top = y(b.count);
          const h = baseline - top;
          const left = MARGIN.left + i * slot + (slot - barW) / 2;
          // Rounded at the data end, square at the baseline.
          const r = Math.min(4, barW / 2, h);
          const d =
            h <= 0
              ? ""
              : `M${left},${baseline}V${top + r}Q${left},${top} ${left + r},${top}` +
                `H${left + barW - r}Q${left + barW},${top} ${left + barW},${top + r}V${baseline}Z`;
          return (
            <g key={i}>
              {d !== "" && <path className="viz-bar" d={d} />}
              <rect
                className="viz-hit"
                x={MARGIN.left + i * slot}
                y={MARGIN.top}
                width={slot}
                height={innerH}
                onMouseEnter={() => setHover(i)}
                onMouseLeave={() => setHover(null)}
              />
            </g>
          );
        })}
      </svg>
      {hover !== null && (
        <Tooltip x={MARGIN.left + (hover + 0.5) * slot} y={MARGIN.top + 24} width={width}>
          <div className="text-muted">
            {formatNumber(bins[hover].x0)} – {formatNumber(bins[hover].x1)}
          </div>
          <div>
            {t("{count} draws", { count: bins[hover].count })}
          </div>
        </Tooltip>
      )}
    </div>
  );
}

/** How tall one row of the forest plot is. */
const ROW = 16;

/**
 * An interval per element — the 90 % posterior interval with the mean on it —
 * one row per group, labelled. Clicking a row picks that element for the trace
 * and the histogram.
 */
export function ForestPlot({
  rows,
  selected,
  onSelect,
}: {
  rows: ForestRow[];
  selected: number | null;
  onSelect: (row: number) => void;
}) {
  const { t } = useT();
  const { ref, width } = useWidth();
  const [hover, setHover] = useState<number | null>(null);
  if (rows.length === 0) return <div className="viz" ref={ref} />;
  const labelW = Math.min(180, Math.max(60, width * 0.28));
  const left = labelW + 8;
  const innerW = Math.max(10, width - left - MARGIN.right);
  // The axis is at the top: with 85 rows the plot scrolls, and an axis at the
  // bottom would be out of view for most of them.
  const top = 22;
  const height = rows.length * ROW + top + 4;
  const [lo, hi] = extent(rows.flatMap((r) => [r.low, r.high]));
  const x = scale(lo, hi, left, left + innerW);
  const ticks = niceTicks(lo, hi, 5);
  const hovered = rows.find((r) => r.row === hover);
  const hoveredAt = rows.findIndex((r) => r.row === hover);

  return (
    <div className="viz" ref={ref}>
      <div className="viz-scroll">
        <svg
          height={height}
          role="img"
          aria-label={t("Forest plot: the 90% interval and the mean of {count} elements", {
            count: rows.length,
          })}
        >
          {ticks.map((v) => (
            <g key={`x${v}`}>
              <line className="viz-grid" x1={x(v)} x2={x(v)} y1={top - 4} y2={height} />
              <text x={x(v)} y={12} textAnchor="middle">
                {formatNumber(v, 3)}
              </text>
            </g>
          ))}
          {rows.map((r, i) => {
            const cy = top + i * ROW + ROW / 2;
            return (
              <g key={r.row}>
                <rect
                  className={`viz-row-hit${selected === r.row ? " viz-row-selected" : ""}`}
                  x={0}
                  y={cy - ROW / 2}
                  width={width}
                  height={ROW}
                  onMouseEnter={() => setHover(r.row)}
                  onMouseLeave={() => setHover(null)}
                  onClick={() => onSelect(r.row)}
                />
                <text x={labelW} y={cy} dy="0.32em" textAnchor="end" pointerEvents="none">
                  {clip(r.label, Math.floor(labelW / 6.5))}
                </text>
                <line className="viz-interval" x1={x(r.low)} x2={x(r.high)} y1={cy} y2={cy} pointerEvents="none" />
                <circle className="viz-point" cx={x(r.centre)} cy={cy} r={4} pointerEvents="none" />
              </g>
            );
          })}
        </svg>
        {hovered && (
          <Tooltip x={x(hovered.centre)} y={top + hoveredAt * ROW + ROW} width={width}>
            <div className="fw-bold">{hovered.label}</div>
            <div>
              <T text="mean" /> {formatNumber(hovered.centre)}
            </div>
            <div className="text-muted">
              90%: {formatNumber(hovered.low)} – {formatNumber(hovered.high)}
            </div>
          </Tooltip>
        )}
      </div>
    </div>
  );
}

/** A label cut to `n` characters with an ellipsis; the whole of it is in the
 * tooltip and the table. */
function clip(text: string, n: number): string {
  return text.length <= n ? text : `${text.slice(0, Math.max(1, n - 1))}…`;
}
