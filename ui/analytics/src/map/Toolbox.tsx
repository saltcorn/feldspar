// The Map workspace's toolbox (analytics TODO A5.12): the tools in their
// groups, and a tool's form. A tool makes an ordinary dataset and adds it to
// the map as a layer; the form says so, and the dataset opens in the Dataset
// editor afterwards like any other.

import { useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import InputGroup from "react-bootstrap/InputGroup";
import Modal from "react-bootstrap/Modal";

import { errorMessage } from "../api";
import { T, useT } from "../i18n";
import type { MapLayer } from "./spec";
import {
  columnChoices,
  groupTools,
  initialAnswers,
  missingAnswer,
  runParams,
  toolParams,
  type Answers,
  type ToolItem,
} from "./toolForm";
import type { ColumnInfo } from "./workspace";

/** The toolbox's menu, a section per group. */
export function ToolboxMenu({
  tools,
  disabled,
  onPick,
}: {
  tools: ToolItem[];
  disabled: boolean;
  onPick: (tool: ToolItem) => void;
}) {
  const { t } = useT();
  return (
    <Dropdown>
      <Dropdown.Toggle size="sm" variant="outline-primary" disabled={disabled || tools.length === 0}>
        <T text="Toolbox" />
      </Dropdown.Toggle>
      <Dropdown.Menu>
        {groupTools(tools).map(([group, items], i) => (
          <div key={group}>
            {i > 0 && <Dropdown.Divider />}
            <Dropdown.Header>{t(group)}</Dropdown.Header>
            {items.map((tool) => (
              <Dropdown.Item key={tool.id} onClick={() => onPick(tool)} title={tool.description}>
                {t(tool.label)}
                {tool.module && <span className="text-secondary small ms-1">({tool.module})</span>}
              </Dropdown.Item>
            ))}
          </div>
        ))}
      </Dropdown.Menu>
    </Dropdown>
  );
}

/** A tool's form. `onRun` makes the dataset and its layer; the form closes
 * when it has, and shows the sentence when it could not. */
export function ToolDialog({
  tool,
  layers,
  active,
  columnsOf,
  onClose,
  onRun,
}: {
  tool: ToolItem;
  layers: MapLayer[];
  active: string | null;
  columnsOf: (dataset: string) => ColumnInfo[];
  onClose: () => void;
  onRun: (params: Record<string, unknown>, name: string | null) => Promise<void>;
}) {
  const { t } = useT();
  const [answers, setAnswers] = useState<Answers>(() => initialAnswers(tool, layers, active));
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const set = (key: string, value: string) => setAnswers((a) => ({ ...a, [key]: value }));
  const missing = missingAnswer(tool, answers);

  const run = async () => {
    setRunning(true);
    setError(null);
    try {
      await onRun(runParams(tool, answers, layers), name.trim() === "" ? null : name.trim());
      onClose();
    } catch (err) {
      setError(errorMessage(err, t("The tool could not run.")));
    } finally {
      setRunning(false);
    }
  };

  return (
    <Modal show onHide={onClose}>
      <Modal.Header closeButton>
        <Modal.Title>{t(tool.label)}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {tool.description && <p className="small text-secondary">{t(tool.description)}</p>}
        {toolParams(tool).map((p) => {
          const id = `tool-${tool.id}-${p.name}`;
          const value = answers[p.name] ?? "";
          let control;
          switch (p.kind) {
            case "layer":
              control = (
                <Form.Select id={id} size="sm" value={value} onChange={(e) => set(p.name, e.target.value)}>
                  <option value="">{t("Pick a layer…")}</option>
                  {[...layers].reverse().map((l) => (
                    <option key={l.id} value={l.id}>
                      {l.name ?? l.id}
                    </option>
                  ))}
                </Form.Select>
              );
              break;
            case "column":
              control = (
                <Form.Select id={id} size="sm" value={value} onChange={(e) => set(p.name, e.target.value)}>
                  <option value="">{t("Pick a column…")}</option>
                  {columnChoices(p, answers, layers, columnsOf).map((c) => (
                    <option key={c.name} value={c.name}>
                      {c.name}
                    </option>
                  ))}
                </Form.Select>
              );
              break;
            case "number":
              control = (
                <InputGroup size="sm">
                  <Form.Control id={id} type="number" value={value} onChange={(e) => set(p.name, e.target.value)} />
                  {p.unit && <InputGroup.Text>{p.unit === "m" ? t("metres") : p.unit}</InputGroup.Text>}
                </InputGroup>
              );
              break;
            case "choice":
              control = (
                <Form.Select id={id} size="sm" value={value} onChange={(e) => set(p.name, e.target.value)}>
                  {(p.options ?? []).map((o) => (
                    <option key={o.value} value={o.value}>
                      {t(o.label)}
                    </option>
                  ))}
                </Form.Select>
              );
              break;
            default:
              control = (
                <Form.Control id={id} size="sm" value={value} onChange={(e) => set(p.name, e.target.value)} />
              );
          }
          return (
            <Form.Group key={p.name} className="mb-2">
              <Form.Label htmlFor={id} className="small mb-1">
                {t(p.label)}
              </Form.Label>
              {control}
            </Form.Group>
          );
        })}
        <Form.Group className="mb-2">
          <Form.Label htmlFor={`tool-${tool.id}-name`} className="small mb-1">
            <T text="Name of the new dataset" />
          </Form.Label>
          <Form.Control
            id={`tool-${tool.id}-name`}
            size="sm"
            value={name}
            placeholder={t("Named after what it is made of")}
            onChange={(e) => setName(e.target.value)}
          />
        </Form.Group>
        <p className="small text-secondary mb-0">
          <T text="The tool makes a dataset of ordinary operations and adds it as a layer. It opens in the Dataset editor like any other." />
        </p>
        {error && (
          <Alert variant="warning" className="small mt-2 mb-0">
            {error}
          </Alert>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onClose}>
          <T text="Cancel" />
        </Button>
        <Button
          variant="primary"
          disabled={running || missing !== null}
          title={missing ? t("Fill in {field}", { field: t(missing) }) : undefined}
          onClick={() => void run()}
        >
          {running ? t("Running…") : t("Run")}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
