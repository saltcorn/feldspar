// The stat card's form (analytics TODO A6.2): which dataset, what number,
// a filter, the date column its periods come from, the comparison, the
// sparkline and the number's format — with the card drawn as it is filled in.

import { useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Row from "react-bootstrap/Row";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse } from "../client";
import { FormulaInput } from "../datasets/FormulaInput";
import { formulaCompletions, isNumeric, type StageColumn, type Translate } from "../datasets/ops";
import { T, useT } from "../i18n";
import {
  CARD_FUNCTIONS,
  DEFAULT_PERIODS,
  PERIODS,
  needsNumbers,
  newCard,
  tidyCard,
  type CardFunction,
  type Comparison,
  type NumberStyle,
  type Period,
  type StatCard,
} from "../panels/card";
import { makePanel, type Panel } from "../panels/panel";
import { PanelView } from "../panels/PanelView";

type Dataset = ListDatasetsResponse[number];

/** How long the preview waits after the last change. */
const PREVIEW_DELAY_MS = 400;

function functionName(f: CardFunction, t: Translate): string {
  switch (f) {
    case "count":
      return t("Count");
    case "count_distinct":
      return t("Count distinct");
    case "sum":
      return t("Sum");
    case "mean":
      return t("Mean");
    case "median":
      return t("Median");
    case "min":
      return t("Minimum");
    case "max":
      return t("Maximum");
  }
}

function periodName(p: Period, t: Translate): string {
  switch (p) {
    case "day":
      return t("Day");
    case "week":
      return t("Week");
    case "month":
      return t("Month");
    case "quarter":
      return t("Quarter");
    case "year":
      return t("Year");
  }
}

/** The form, for a new card (`panel` absent) or one being edited. */
export function StatCardForm({
  panel,
  onSave,
  onCancel,
}: {
  panel: Extract<Panel, { kind: "stat_card" }> | null;
  onSave: (panel: Panel) => void;
  onCancel: () => void;
}) {
  const { t } = useT();
  const [datasets, setDatasets] = useState<Dataset[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState(panel?.title ?? "");
  const [card, setCard] = useState<StatCard | null>(panel ? panel.content : null);

  useEffect(() => {
    let live = true;
    api
      .listDatasets()
      .then((ds) => {
        if (!live) return;
        setDatasets(ds);
        setCard((c) => c ?? (ds[0] ? newCard(ds[0].id) : null));
      })
      .catch((err: unknown) => live && setError(errorMessage(err, t("Could not load the datasets."))));
    return () => {
      live = false;
    };
  }, [t]);

  const dataset = datasets?.find((d) => d.id === card?.dataset);
  const columns = useMemo(() => (dataset?.columns ?? []) as StageColumn[], [dataset]);
  const dates = columns.filter((c) => c.type === "date" || c.type === "timestamp");
  const completions = useMemo(() => formulaCompletions({ columns, grain: { kind: "derived" } }, null), [columns]);

  // The card drawn a moment after the last change.
  const draft = useMemo(
    () => (card ? makePanel({ kind: "stat_card", content: tidyCard(card) }, title) : null),
    [card, title],
  );
  const [preview, setPreview] = useState<Panel | null>(null);
  useEffect(() => {
    const timer = window.setTimeout(() => setPreview(draft), PREVIEW_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [draft]);

  const change = (c: Partial<StatCard>) => setCard((old) => (old ? { ...old, ...c } : old));
  const format = card?.format ?? {};
  const setFormat = (f: Partial<NonNullable<StatCard["format"]>>) => change({ format: { ...format, ...f } });
  const fn = card?.value.function ?? "count";
  const valueColumns = needsNumbers(fn) ? columns.filter((c) => isNumeric(c.type)) : columns;
  const missingColumn = needsNumbers(fn) && !card?.value.column;
  const missingCurrency = format.style === "currency" && !/^[A-Za-z]{3}$/.test(format.currency ?? "");

  return (
    <Modal show onHide={onCancel} size="xl">
      <Modal.Header closeButton>
        <Modal.Title>{panel ? t("Edit stat card") : t("New stat card")}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {datasets && datasets.length === 0 && (
          <Alert variant="info">
            <T text="There are no datasets yet. Create one on the front page first." />
          </Alert>
        )}
        {card && (
          <Row className="g-3">
            <Col md={7}>
              <Form.Group className="mb-2" controlId="card-title">
                <Form.Label>{t("Title")}</Form.Label>
                <Form.Control value={title} placeholder={t("Incidents")} onChange={(e) => setTitle(e.target.value)} />
              </Form.Group>
              <Form.Group className="mb-2" controlId="card-dataset">
                <Form.Label>{t("Dataset")}</Form.Label>
                <Form.Select
                  value={card.dataset}
                  onChange={(e) => setCard({ ...newCard(e.target.value), format: card.format })}
                >
                  {datasets?.map((d) => (
                    <option key={d.id} value={d.id}>
                      {d.name}
                    </option>
                  ))}
                </Form.Select>
              </Form.Group>
              <Row className="g-2 mb-2">
                <Col>
                  <Form.Group controlId="card-function">
                    <Form.Label>{t("Number")}</Form.Label>
                    <Form.Select
                      value={fn}
                      onChange={(e) => {
                        const f = e.target.value as CardFunction;
                        const keep = card.value.column && (!needsNumbers(f) || valueColumns.some((c) => c.name === card.value.column && isNumeric(c.type)));
                        change({ value: { function: f, column: keep ? card.value.column : undefined } });
                      }}
                    >
                      {CARD_FUNCTIONS.map((f) => (
                        <option key={f} value={f}>
                          {functionName(f, t)}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
                <Col>
                  <Form.Group controlId="card-column">
                    <Form.Label>{t("Of")}</Form.Label>
                    <Form.Select
                      value={card.value.column ?? ""}
                      isInvalid={missingColumn}
                      onChange={(e) => change({ value: { function: fn, column: e.target.value || undefined } })}
                    >
                      {!needsNumbers(fn) && <option value="">{t("rows")}</option>}
                      {needsNumbers(fn) && <option value="">{t("Choose a column…")}</option>}
                      {valueColumns.map((c) => (
                        <option key={c.name} value={c.name}>
                          {c.name}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
              </Row>
              <Form.Group className="mb-2" controlId="card-filter">
                <Form.Label>{t("Only rows where")}</Form.Label>
                <FormulaInput
                  id="card-filter"
                  value={card.filter ?? ""}
                  onChange={(filter) => change({ filter })}
                  completions={completions}
                  placeholder={t("a condition, such as category == \"burglary\"")}
                />
              </Form.Group>
              <Row className="g-2 mb-2">
                <Col>
                  <Form.Group controlId="card-time">
                    <Form.Label>{t("Periods from")}</Form.Label>
                    <Form.Select
                      value={card.time?.column ?? ""}
                      onChange={(e) =>
                        change({
                          time: e.target.value
                            ? { column: e.target.value, period: card.time?.period ?? "month", anchor: card.time?.anchor }
                            : undefined,
                        })
                      }
                    >
                      <option value="">{t("No periods: every row")}</option>
                      {dates.map((c) => (
                        <option key={c.name} value={c.name}>
                          {c.name}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
                <Col>
                  <Form.Group controlId="card-period">
                    <Form.Label>{t("Period")}</Form.Label>
                    <Form.Select
                      value={card.time?.period ?? "month"}
                      disabled={!card.time}
                      onChange={(e) => card.time && change({ time: { ...card.time, period: e.target.value as Period } })}
                    >
                      {PERIODS.map((p) => (
                        <option key={p} value={p}>
                          {periodName(p, t)}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
                <Col>
                  <Form.Group controlId="card-anchor">
                    <Form.Label>{t("Current period")}</Form.Label>
                    <Form.Select
                      value={card.time?.anchor ?? "latest"}
                      disabled={!card.time}
                      onChange={(e) =>
                        card.time && change({ time: { ...card.time, anchor: e.target.value as "latest" | "today" } })
                      }
                    >
                      <option value="latest">{t("The latest")}</option>
                      <option value="today">{t("Today's")}</option>
                    </Form.Select>
                  </Form.Group>
                </Col>
              </Row>
              <Row className="g-2 mb-2 align-items-end">
                <Col>
                  <Form.Group controlId="card-comparison">
                    <Form.Label>{t("Compare with")}</Form.Label>
                    <Form.Select
                      value={card.comparison ?? "none"}
                      onChange={(e) => change({ comparison: e.target.value as Comparison })}
                    >
                      <option value="none">{t("Nothing")}</option>
                      <option value="previous_period" disabled={!card.time}>
                        {t("The previous period")}
                      </option>
                      <option value="unfiltered">{t("Every row (unfiltered)")}</option>
                    </Form.Select>
                  </Form.Group>
                </Col>
                <Col>
                  <Form.Check
                    id="card-better"
                    type="switch"
                    label={t("Higher is better")}
                    checked={card.higher_is_better !== false}
                    onChange={(e) => change({ higher_is_better: e.target.checked })}
                  />
                </Col>
              </Row>
              <Row className="g-2 mb-2 align-items-end">
                <Col>
                  <Form.Check
                    id="card-sparkline"
                    type="switch"
                    label={t("Sparkline")}
                    disabled={!card.time}
                    checked={Boolean(card.sparkline && card.time)}
                    onChange={(e) => change({ sparkline: e.target.checked })}
                  />
                </Col>
                <Col>
                  <Form.Group controlId="card-periods">
                    <Form.Label>{t("Periods shown")}</Form.Label>
                    <Form.Control
                      type="number"
                      min={2}
                      max={120}
                      disabled={!card.sparkline || !card.time}
                      value={card.periods ?? DEFAULT_PERIODS}
                      onChange={(e) => change({ periods: Math.max(2, Math.min(120, Number(e.target.value) || DEFAULT_PERIODS)) })}
                    />
                  </Form.Group>
                </Col>
              </Row>
              <Row className="g-2">
                <Col>
                  <Form.Group controlId="card-style">
                    <Form.Label>{t("Format")}</Form.Label>
                    <Form.Select value={format.style ?? "number"} onChange={(e) => setFormat({ style: e.target.value as NumberStyle })}>
                      <option value="number">{t("Number")}</option>
                      <option value="percent">{t("Percentage")}</option>
                      <option value="currency">{t("Money")}</option>
                    </Form.Select>
                  </Form.Group>
                </Col>
                {format.style === "currency" && (
                  <Col>
                    <Form.Group controlId="card-currency">
                      <Form.Label>{t("Currency")}</Form.Label>
                      <Form.Control
                        value={format.currency ?? ""}
                        placeholder={t("EUR")}
                        maxLength={3}
                        isInvalid={missingCurrency}
                        onChange={(e) => setFormat({ currency: e.target.value.toUpperCase() })}
                      />
                    </Form.Group>
                  </Col>
                )}
                <Col>
                  <Form.Group controlId="card-decimals">
                    <Form.Label>{t("Decimals")}</Form.Label>
                    <Form.Select
                      value={format.decimals ?? ""}
                      onChange={(e) => setFormat({ decimals: e.target.value === "" ? undefined : Number(e.target.value) })}
                    >
                      <option value="">{t("Automatic")}</option>
                      {[0, 1, 2, 3, 4].map((d) => (
                        <option key={d} value={d}>
                          {d}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
                <Col>
                  <Form.Group controlId="card-suffix">
                    <Form.Label>{t("Unit")}</Form.Label>
                    <Form.Control
                      value={format.suffix ?? ""}
                      maxLength={20}
                      placeholder={t("m²")}
                      onChange={(e) => setFormat({ suffix: e.target.value })}
                    />
                  </Form.Group>
                </Col>
              </Row>
              <Form.Check
                className="mt-2"
                id="card-compact"
                type="switch"
                label={t("Compact (1.2K rather than 1,234)")}
                checked={Boolean(format.compact)}
                onChange={(e) => setFormat({ compact: e.target.checked })}
              />
            </Col>
            <Col md={5}>
              <div className="form-label">{t("Preview")}</div>
              <div className="an-card-preview">
                {title.trim() && <div className="an-tile-title mb-1">{title.trim()}</div>}
                {preview && !missingColumn && !missingCurrency ? (
                  <PanelView panel={preview} />
                ) : (
                  <p className="text-secondary small m-0">
                    <T text="Choose what to show." />
                  </p>
                )}
              </div>
            </Col>
          </Row>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button
          variant="primary"
          disabled={!draft || missingColumn || missingCurrency}
          onClick={() => draft && onSave(draft)}
        >
          {panel ? t("Save") : t("Add")}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
