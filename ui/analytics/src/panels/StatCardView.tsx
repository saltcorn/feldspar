// A stat card, drawn (analytics TODO A6.2): the number large, what it is and
// the period it covers, the comparison with an arrow coloured by whether the
// change is good news, and the sparkline. Plain markup and an SVG line, so it
// prints in a report as it shows in a dashboard.

import { useT } from "../i18n";
import { changeOf, formatValue, periodLabel, sparkPath, type CardData, type StatCard } from "./card";

const SPARK_W = 120;
const SPARK_H = 28;

export function StatCardView({ card, data }: { card: StatCard; data: CardData }) {
  const { t, locale } = useT();
  const change = changeOf(card, data, t, locale);
  const what = data.period ? periodLabel(data.period, t, locale) : data.label;
  const points = data.sparkline ?? [];
  const path = points.length > 1 ? sparkPath(points, SPARK_W, SPARK_H) : "";
  const tone = change?.good === true ? "an-good" : change?.good === false ? "an-bad" : "an-neutral";
  const arrow = change?.direction === "up" ? "▲" : change?.direction === "down" ? "▼" : "";
  return (
    <div className="an-card" data-value={data.value ?? ""}>
      <div className="an-card-value">{formatValue(data.value, card.format, locale)}</div>
      <div className="an-card-label text-secondary">{what}</div>
      {change && (
        <div className={`an-card-change ${tone}`}>
          {arrow && <span aria-hidden>{arrow} </span>}
          <strong>{change.text}</strong> <span className="text-secondary">{change.context}</span>
        </div>
      )}
      {path && (
        <svg
          className="an-card-spark"
          viewBox={`-2 -2 ${SPARK_W + 4} ${SPARK_H + 4}`}
          preserveAspectRatio="none"
          role="img"
          aria-label={t("The last {n} periods", { n: points.length })}
        >
          <path d={path} fill="none" vectorEffect="non-scaling-stroke" />
        </svg>
      )}
    </div>
  );
}
