// The hypothesis tests beside the Data explorer's plot (analytics TODO
// A2.12–A2.14): the sentence, the results as a short table, the pairwise
// comparisons after an analysis of variance, and what the assumption checks
// found — once per Wrap group. Paired mode and the value a mean is tested
// against are set here.

import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { T, useT } from "../i18n";
import { effectName, estimateName, testName } from "../labels";
import type { TestsState } from "./state";
import {
  differingPairs,
  estimateText,
  levelLabel,
  notes,
  num,
  pValue,
  sectionHeading,
  sentence,
  type Analysis,
  type Entry,
  type Section,
} from "./tests";

export function TestResults({
  analysis,
  error,
  loading,
  settings,
  yCount,
  onChange,
  onOpenAsModel,
}: {
  analysis: Analysis | null;
  error: string | null;
  loading: boolean;
  settings: TestsState;
  /** How many columns are on Y: paired mode is offered for two. */
  yCount: number;
  onChange: (change: Partial<TestsState>) => void;
  /** Ask the same question as a model (A3.7); absent when the roles are not
   * one response and one factor. */
  onOpenAsModel?: () => void;
}) {
  const { t, locale } = useT();
  return (
    <section className={loading ? "an-tests an-stale" : "an-tests"} aria-label={t("Tests")}>
      <div className="an-tests-head">
        <strong>
          <T text="Tests" />
        </strong>
        {loading && <Spinner animation="border" size="sm" />}
        {onOpenAsModel && (
          <Button
            size="sm"
            variant="outline-primary"
            className="ms-2"
            title={t("A regression of Y on X, opened in the model editor")}
            onClick={onOpenAsModel}
          >
            <T text="Open as model" />
          </Button>
        )}
        {yCount === 2 && (
          <Form.Check
            type="switch"
            id="tests-paired"
            className="ms-auto small"
            label={t("Paired")}
            title={t("The two columns on Y are measurements of the same rows, such as before and after")}
            checked={settings.paired}
            onChange={(e) => onChange({ paired: e.target.checked })}
          />
        )}
        {analysis?.design === "one_number" && (
          <label className="ms-auto small d-flex align-items-center gap-1">
            <T text="Mean against" />
            <Form.Control
              size="sm"
              type="number"
              style={{ width: "6rem" }}
              aria-label={t("The value the mean is tested against")}
              defaultValue={settings.mu}
              key={settings.mu}
              onBlur={(e) => {
                const mu = Number(e.target.value);
                if (Number.isFinite(mu) && mu !== settings.mu) onChange({ mu });
              }}
            />
          </label>
        )}
      </div>
      {error ? (
        <p className="small text-secondary mb-0">{error}</p>
      ) : !analysis ? (
        <p className="small text-secondary mb-0">
          <T text="Put a column on Y, and a factor on X, to test them." />
        </p>
      ) : (
        analysis.sections.map((s, i) => <SectionView key={i} analysis={analysis} section={s} locale={locale} />)
      )}
    </section>
  );
}

function SectionView({ analysis, section: s, locale }: { analysis: Analysis; section: Section; locale: string }) {
  const { t } = useT();
  const heading = sectionHeading(analysis, s, t, locale);
  const said = sentence(analysis, s, t, locale);
  const pairs = differingPairs(s, t, locale);
  const name = (k: Parameters<typeof testName>[0]) => testName(k, t);
  const remarks = notes(analysis, s, t, name, locale);
  return (
    <div className="an-tests-section">
      {heading && <h6 className="mb-1">{heading}</h6>}
      {s.error ? (
        <p className="small text-secondary">{s.error}</p>
      ) : (
        <>
          {said && <p className="an-test-sentence">{said}</p>}
          {pairs.length > 0 && (
            <p className="small mb-2">{t("Pairs that differ: {pairs}.", { pairs: pairs.join(", ") })}</p>
          )}
          <table className="table table-sm an-test-table">
            <thead>
              <tr>
                <th>
                  <T text="Test" />
                </th>
                <th className="an-test-p">
                  <T text="p-value" />
                </th>
              </tr>
            </thead>
            <tbody>
              {s.tests.map((e) => (
                <EntryRow key={e.test} entry={e} preferred={e.test === s.preferred} locale={locale} />
              ))}
            </tbody>
          </table>
          {(s.comparisons ?? []).length > 0 && (
            <details className="small mb-2">
              <summary>
                <T text="Pairwise comparisons (Tukey)" />
              </summary>
              <table className="table table-sm an-test-table mt-1">
                <thead>
                  <tr>
                    <th>
                      <T text="Pair" />
                    </th>
                    <th>
                      <T text="Difference (interval)" />
                    </th>
                    <th>
                      <T text="Adjusted p" />
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {(s.comparisons ?? []).map((c) => (
                    <tr key={`${c.a}-${c.b}`}>
                      <td>
                        {t("{b} − {a}", {
                          a: levelLabel(s.levels[c.a], t, locale),
                          b: levelLabel(s.levels[c.b], t, locale),
                        })}
                      </td>
                      <td>
                        {t("{value} ({lower} to {upper})", {
                          value: num(c.difference, locale),
                          lower: num(c.lower, locale),
                          upper: num(c.upper, locale),
                        })}
                      </td>
                      <td>{pValue(c.p_value, locale)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </details>
          )}
          {remarks.length > 0 && (
            <ul className="an-notes mb-2">
              {remarks.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}

/** One test: its name and p-value, and beneath them the statistic, the
 * estimate with its interval, and the effect size. */
function EntryRow({ entry: e, preferred, locale }: { entry: Entry; preferred: boolean; locale: string }) {
  const { t } = useT();
  const r = e.result;
  const details: string[] = [];
  if (r?.statistic) {
    const df = r.df && r.df.length > 0 ? ` ${t("(df {df})", { df: r.df.map((d) => num(d, locale)).join(", ") })}` : "";
    details.push(`${r.statistic.symbol} = ${num(r.statistic.value, locale)}${df}`);
  }
  if (r?.estimate) details.push(`${estimateName(r.estimate.of, t)} ${estimateText(r, t, locale)}`);
  if (r?.effect) details.push(`${effectName(r.effect.kind, t)} ${num(r.effect.value, locale)}`);
  return (
    <tr className={preferred ? "an-test-preferred" : undefined}>
      <td>
        <div className="an-test-name">
          {testName(e.test, t)}
          {e.role === "alternative" && <span className="an-test-alt">{t("alternative")}</span>}
          {r?.sampled && <span className="an-test-alt">{t("sample")}</span>}
        </div>
        <div className="an-test-detail">{r ? details.join(" · ") : e.error}</div>
      </td>
      <td className="an-test-p">{r ? pValue(r.p_value, locale) : "–"}</td>
    </tr>
  );
}
