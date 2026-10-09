// A tile's drill path (analytics TODO A6.5): the columns a plot's channel
// shows on the way down, outermost first — the plot's own column, then the
// ones below it.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";

import { api, errorMessage } from "../api";
import type { StageColumn } from "../datasets/ops";
import { T, useT } from "../i18n";
import type { PlotSpec } from "../plot/spec";
import { DRILL_CHANNELS, MAX_LEVELS, defaultChannel, fieldOn, type Drill, type DrillChannel } from "./drill";

function channelName(c: DrillChannel, t: (s: string) => string): string {
  return c === "x" ? t("X") : c === "y" ? t("Y") : t("Color");
}

/** Whether a column can be a level: one whose values group rows. */
function groups(c: StageColumn): boolean {
  return !["geometry", "json", "bytes", "float", "decimal"].includes(c.type) || Boolean(c.key);
}

export function DrillForm({
  spec,
  drill,
  onSave,
  onCancel,
}: {
  spec: PlotSpec;
  drill: Drill | null;
  /** The path, or `null` to have none. */
  onSave: (drill: Drill | null) => void;
  onCancel: () => void;
}) {
  const { t } = useT();
  const channels = DRILL_CHANNELS.filter((c) => fieldOn(spec, c) !== undefined);
  const [channel, setChannel] = useState<DrillChannel>(drill?.channel ?? defaultChannel(spec));
  const top = fieldOn(spec, channel);
  const [path, setPath] = useState<string[]>(drill?.path ?? (top ? [top] : []));
  const [columns, setColumns] = useState<StageColumn[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (spec.data.kind !== "dataset") return;
    const id = spec.data.dataset;
    let live = true;
    api
      .listDatasets()
      .then((ds) => live && setColumns(((ds.find((d) => d.id === id)?.columns ?? []) as StageColumn[]).filter(groups)))
      .catch((err: unknown) => live && setError(errorMessage(err, t("Could not load the datasets."))));
    return () => {
      live = false;
    };
  }, [spec, t]);

  const pickChannel = (c: DrillChannel) => {
    setChannel(c);
    const f = fieldOn(spec, c);
    setPath(f ? [f] : []);
  };
  const move = (i: number, by: number) =>
    setPath((p) => {
      const j = i + by;
      if (j < 1 || j >= p.length || i < 1) return p;
      const next = [...p];
      [next[i], next[j]] = [next[j], next[i]];
      return next;
    });
  const remaining = (columns ?? []).filter((c) => !path.includes(c.name));
  const ok = path.length >= 2;

  return (
    <Modal show onHide={onCancel}>
      <Modal.Header closeButton>
        <Modal.Title>{t("Drill path")}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        <p className="text-secondary small">
          <T text="Clicking a value on the plot shows the next column for that value, with a breadcrumb back. At the last level a click filters the other tiles, as on any plot." />
        </p>
        {channels.length > 1 && (
          <Form.Group className="mb-3">
            <Form.Label>{t("Drill down along")}</Form.Label>
            <Form.Select value={channel} onChange={(e) => pickChannel(e.target.value as DrillChannel)}>
              {channels.map((c) => (
                <option key={c} value={c}>
                  {channelName(c, t)}
                </option>
              ))}
            </Form.Select>
          </Form.Group>
        )}
        <ol className="an-drill-path">
          {path.map((name, i) => (
            <li key={name}>
              <span className="an-drill-level">{name}</span>
              {i === 0 ? (
                <span className="text-secondary small">{t("what the plot shows")}</span>
              ) : (
                <span className="ms-auto">
                  <Button size="sm" variant="link" className="p-0 px-1" disabled={i === 1} aria-label={t("Up")} onClick={() => move(i, -1)}>
                    ↑
                  </Button>
                  <Button
                    size="sm"
                    variant="link"
                    className="p-0 px-1"
                    disabled={i === path.length - 1}
                    aria-label={t("Down")}
                    onClick={() => move(i, 1)}
                  >
                    ↓
                  </Button>
                  <Button
                    size="sm"
                    variant="link"
                    className="p-0 px-1 text-secondary"
                    aria-label={t("Remove {name}", { name })}
                    onClick={() => setPath((p) => p.filter((n) => n !== name))}
                  >
                    ×
                  </Button>
                </span>
              )}
            </li>
          ))}
        </ol>
        {path.length < MAX_LEVELS && remaining.length > 0 && (
          <Form.Select
            size="sm"
            value=""
            aria-label={t("Add a level")}
            onChange={(e) => e.target.value && setPath((p) => [...p, e.target.value])}
          >
            <option value="">{t("+ Add a level…")}</option>
            {remaining.map((c) => (
              <option key={c.name} value={c.name}>
                {c.name}
              </option>
            ))}
          </Form.Select>
        )}
      </Modal.Body>
      <Modal.Footer>
        {drill && (
          <Button variant="outline-danger" className="me-auto" onClick={() => onSave(null)}>
            <T text="Remove the drill path" />
          </Button>
        )}
        <Button variant="secondary" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button variant="primary" disabled={!ok} onClick={() => ok && onSave({ channel, path })}>
          <T text="Save" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
