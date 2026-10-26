// A provider's models (TODO §3a): each model the provider serves is a row of
// its own, with its prices, context window and capability overrides. Shown
// inside the provider's form, once the provider has been saved.
//
// Models have a kind (TODO-agent-skills AS3.1): chat, embedding, image or
// speech. Each kind is listed under its own heading, has its own settings (a
// non-chat model has only its prices) and its own default.
//
// The same discipline as the provider form: **no screen knows a backend**. The
// model settings are the backend's declared `FormField`s, rendered by
// `SettingsFields`. Every setting is optional, and blank means the built-in
// default — which is why each row shows what its settings *resolve to*, so the
// admin can see the default they are leaving in place.
//
// A provider the server's configuration file supplies lists its models
// `readOnly`: the file says which models it may be used with, so there is
// nothing to add, change or delete here — only to test.

import { useCallback, useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListLlmModelSettingsResponse, TestLlmModelResponse } from "../client";
import {
  KIND_HEADINGS,
  capabilitySummary,
  groupByKind,
  guessKind,
  kindOf,
  kindsServedBy,
  namesToOffer,
  priceSummary,
  type Capabilities,
  type ModelItem,
  type ModelKind,
  type Prices,
} from "../llmModels";
import { SettingsFields, buildConfig, readConfig } from "../settings";
import { T, useT } from "../i18n";

type FieldSpec = ListLlmModelSettingsResponse[number];

/** The model being added or edited: `id` is null for a new one. */
type Editing = {
  id: string | null;
  kind: ModelKind;
  name: string;
  description: string;
  isDefault: boolean;
  config: Record<string, string>;
};

export function LlmModels({
  providerId,
  backend,
  providerConfig,
  readOnly = false,
}: {
  providerId: string;
  backend: string;
  /** The provider's config as the form holds it, sentinel included — what a
   * test sends, so an unsaved change to the key or URL is what is tested. */
  providerConfig: Record<string, unknown>;
  /** The models come from the configuration file: test them, change nothing. */
  readOnly?: boolean;
}) {
  const { t } = useT();
  const [models, setModels] = useState<ModelItem[] | null>(null);
  const [specs, setSpecs] = useState<Partial<Record<ModelKind, FieldSpec[]>>>({});
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState<string | null>(null);
  const [test, setTest] = useState<TestLlmModelResponse | null>(null);
  const [fetching, setFetching] = useState(false);
  const [offered, setOffered] = useState<string[] | null>(null);
  const [fetchMessage, setFetchMessage] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      setModels(await api.listLlmModels(providerId));
    } catch (err) {
      setError(errorMessage(err, "Could not load the models."));
    }
  }, [providerId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Each kind the backend serves has its own settings.
  useEffect(() => {
    let cancelled = false;
    setSpecs({});
    for (const kind of kindsServedBy(backend)) {
      api
        .listLlmModelSettings(backend, { kind })
        .then((s) => {
          if (!cancelled) setSpecs((cur) => ({ ...cur, [kind]: s }));
        })
        .catch(() => {
          if (!cancelled) setSpecs((cur) => ({ ...cur, [kind]: [] }));
        });
    }
    return () => {
      cancelled = true;
    };
  }, [backend]);

  const spec = editing ? (specs[editing.kind] ?? []) : [];
  /** Whether the provider has no model of `kind` yet: its first becomes the
   * default of that kind. */
  const noneOf = (kind: ModelKind) => (models ?? []).every((m) => kindOf(m) !== kind);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!editing) return;
    setSaving(true);
    setError(null);
    const body = {
      name: editing.name.trim(),
      description: editing.description.trim(),
      kind: editing.kind,
      is_default: editing.isDefault,
      config: buildConfig(spec, editing.config),
    };
    try {
      if (editing.id) {
        await api.updateLlmModel(editing.id, body);
      } else {
        await api.createLlmModel(providerId, body);
      }
      setEditing(null);
      await reload();
    } catch (err) {
      setError(errorMessage(err, "Could not save the model."));
    }
    setSaving(false);
  };

  /** Save a row as it is, changing only what `change` says. */
  const update = async (m: ModelItem, change: Partial<{ name: string; is_default: boolean }>) => {
    setError(null);
    try {
      await api.updateLlmModel(m.id, {
        name: m.name,
        description: m.description,
        kind: kindOf(m),
        is_default: m.is_default,
        config: m.config,
        ...change,
      });
      await reload();
    } catch (err) {
      setError(errorMessage(err, `Could not change ${m.name}.`));
    }
  };

  const remove = async (m: ModelItem) => {
    if (!window.confirm(t("Delete the model {name}?", { name: m.name }))) return;
    setError(null);
    try {
      await api.deleteLlmModel(m.id);
      await reload();
    } catch (err) {
      // The server names the agents still calling it.
      setError(errorMessage(err, `Could not delete ${m.name}.`));
    }
  };

  const addFetched = async (name: string) => {
    setError(null);
    try {
      // Blank settings: every built-in default. The first model of a kind a
      // provider gets becomes its default, so an agent naming no model works
      // at once. The kind is guessed from the name; Edit cannot change it,
      // but a wrong guess is deleted and added again.
      const kind = kindsServedBy(backend).includes(guessKind(name)) ? guessKind(name) : "chat";
      await api.createLlmModel(providerId, {
        name,
        description: "",
        kind,
        is_default: noneOf(kind),
        config: {},
      });
      setOffered((names) => (names ?? []).filter((n) => n !== name));
      await reload();
    } catch (err) {
      setError(errorMessage(err, `Could not add ${name}.`));
    }
  };

  const fetchModels = async () => {
    setFetching(true);
    setFetchMessage(null);
    setOffered(null);
    try {
      const result = await api.fetchLlmModels(providerId);
      if (result.ok) {
        setOffered(namesToOffer(result.names, models ?? []));
        if (result.names.length === 0) setFetchMessage("Every model the host lists already has a row.");
      } else {
        // A host with no listing says so; the admin types the name instead.
        setFetchMessage(result.message);
      }
    } catch (err) {
      setError(errorMessage(err, "Could not fetch the models."));
    }
    setFetching(false);
  };

  const runTest = async (m: ModelItem) => {
    setTesting(m.id);
    setTest(null);
    setError(null);
    try {
      setTest(
        await api.testLlmModel({
          provider_id: providerId,
          backend,
          config: providerConfig,
          name: m.name,
          model_config: m.config,
          kind: kindOf(m),
        }),
      );
    } catch (err) {
      setError(errorMessage(err, `Could not test ${m.name}.`));
    }
    setTesting(null);
  };

  return (
    <Card className="mb-3">
      <Card.Header className="d-flex align-items-center">
        <Card.Title className="mb-0">
          <T text="Models" />
        </Card.Title>
        {!readOnly && (
          <div className="ms-auto btn-list">
            <Button
              size="sm"
              variant="outline-secondary"
              disabled={fetching}
              onClick={() => void fetchModels()}
            >
              {fetching ? "Fetching…" : "Fetch models"}
            </Button>
            <Button
              size="sm"
              onClick={() =>
                setEditing({
                  id: null,
                  kind: "chat",
                  name: "",
                  description: "",
                  isDefault: noneOf("chat"),
                  config: {},
                })
              }
            >
              <T text="Add model" />
            </Button>
          </div>
        )}
      </Card.Header>
      <Card.Body>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}
        {test && (
          <Alert variant={test.ok ? "success" : "danger"} dismissible onClose={() => setTest(null)}>
            <Alert.Heading className="h6">
              {test.ok ? `${test.model} answered` : `${test.model} did not answer`}
            </Alert.Heading>
            {/* The provider's own words either way — on success what the model
                actually replied, so an admin pointed at the wrong endpoint sees
                a wrong answer rather than a green tick. */}
            <div className="text-break small mb-0">{test.message}</div>
          </Alert>
        )}
        {fetchMessage && (
          <Alert variant="info" dismissible onClose={() => setFetchMessage(null)}>
            {fetchMessage}
          </Alert>
        )}
        {offered && offered.length > 0 && (
          <div className="mb-3">
            <div className="small text-muted mb-1">
              <T text="The host lists these models. Each is added with blank settings, which means the built-in defaults." />
            </div>
            <div className="d-flex flex-wrap gap-1">
              {offered.map((name) => (
                <Button key={name} size="sm" variant="outline-primary" onClick={() => void addFetched(name)}>
                  + {name}
                </Button>
              ))}
            </div>
          </div>
        )}

        {models === null ? (
          <div className="text-center py-3">
            <Spinner animation="border" size="sm" role="status" />
          </div>
        ) : models.length === 0 ? (
          <p className="text-muted mb-0">
            <T text="No models yet. An agent needs a model to call: add one, or fetch the host's list." />
          </p>
        ) : (
          groupByKind(models).map(({ kind, models: ofKind }) => (
            <div key={kind} className="mb-3">
              <h4 className="h5 mt-2">
                <T text={KIND_HEADINGS[kind]} />
              </h4>
              <Table responsive className="card-table table-vcenter mb-0">
                <thead>
                  <tr>
                    <th>
                      <T text="Model" />
                    </th>
                    <th>
                      <T text="Resolves to" />
                    </th>
                    <th>
                      <T text="Prices" />
                    </th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {ofKind.map((m) => {
                    const caps = m.capabilities as Capabilities | null;
                    const prices = m.prices as Prices;
                    return (
                      <tr key={m.id}>
                        <td>
                          <span className="font-monospace">{m.name}</span>
                          {m.is_default && (
                            <Badge bg="primary-lt" className="ms-2">
                              <T text="default" />
                            </Badge>
                          )}
                          {m.description && <div className="small text-muted">{m.description}</div>}
                        </td>
                        <td className="small text-muted">
                          {caps ? capabilitySummary(caps).join(" · ") : "–"}
                        </td>
                        <td className="small">{priceSummary(kind, prices)}</td>
                        <td className="text-end">
                          <div className="btn-list flex-nowrap justify-content-end">
                            {!m.is_default && !readOnly && (
                              <Button
                                size="sm"
                                variant="outline-secondary"
                                onClick={() => void update(m, { is_default: true })}
                              >
                                <T text="Make default" />
                              </Button>
                            )}
                            <Button
                              size="sm"
                              variant="outline-secondary"
                              disabled={testing !== null}
                              onClick={() => void runTest(m)}
                            >
                              {testing === m.id ? "Testing…" : "Test"}
                            </Button>
                            {!readOnly && (
                              <>
                                <Button
                                  size="sm"
                                  variant="outline-secondary"
                                  onClick={() =>
                                    setEditing({
                                      id: m.id,
                                      kind: kindOf(m),
                                      name: m.name,
                                      description: m.description,
                                      isDefault: m.is_default,
                                      config: readConfig(m.config),
                                    })
                                  }
                                >
                                  <T text="Edit" />
                                </Button>
                                <Button size="sm" variant="outline-danger" onClick={() => void remove(m)}>
                                  <T text="Delete" />
                                </Button>
                              </>
                            )}
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </Table>
            </div>
          ))
        )}
        <p className="text-muted small mt-3 mb-0">
          <T text="Testing sends one short prompt to the model, which counts against your account like any other request." />
        </p>
      </Card.Body>

      <Modal show={editing !== null} onHide={() => setEditing(null)} size="lg">
        {editing && (
          <Form onSubmit={save}>
            <Modal.Header closeButton>
              <Modal.Title className="h4">{editing.id ? "Edit model" : "Add model"}</Modal.Title>
            </Modal.Header>
            <Modal.Body>
              <Form.Group className="mb-3" controlId="modelKind">
                <Form.Label>
                  <T text="Kind" />
                </Form.Label>
                <Form.Select
                  value={editing.kind}
                  disabled={editing.id !== null}
                  onChange={(e) => {
                    const kind = e.target.value as ModelKind;
                    // Another kind has other settings, and its own default.
                    setEditing({
                      ...editing,
                      kind,
                      config: {},
                      isDefault: noneOf(kind),
                    });
                  }}
                >
                  {kindsServedBy(backend).map((k) => (
                    <option key={k} value={k}>
                      {t(KIND_HEADINGS[k])}
                    </option>
                  ))}
                </Form.Select>
                <Form.Text muted>
                  <T text="Chat models answer agents. Embedding, image and speech models make vectors, pictures and audio. Fixed once the model is added." />
                </Form.Text>
              </Form.Group>
              <Form.Group className="mb-3" controlId="modelName">
                <Form.Label>
                  <T text="Model name" />
                  <span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control
                  className="font-monospace"
                  value={editing.name}
                  required
                  onChange={(e) => setEditing({ ...editing, name: e.target.value })}
                />
                <Form.Text muted>
                  <T text="The vendor's model id, exactly as the API takes it." />
                </Form.Text>
              </Form.Group>
              <Form.Group className="mb-3" controlId="modelDescription">
                <Form.Label>
                  <T text="Description" />
                </Form.Label>
                <Form.Control
                  value={editing.description}
                  onChange={(e) => setEditing({ ...editing, description: e.target.value })}
                />
              </Form.Group>
              <Form.Group className="mb-3" controlId="modelDefault">
                <Form.Check
                  type="checkbox"
                  label={
                    editing.kind === "chat"
                      ? t("The provider's default model")
                      : t("The provider's default model of this kind")
                  }
                  checked={editing.isDefault}
                  onChange={(e) => setEditing({ ...editing, isDefault: e.target.checked })}
                />
                <Form.Text muted>
                  <T text="What an agent that names this provider and no model calls. Making this the default takes it from any other model." />
                </Form.Text>
              </Form.Group>
              <p className="text-muted small">
                <T text="Leave a setting blank to use the built-in default. A blank price is unknown, not free, so a cost budget cannot be set on an agent using this model." />
              </p>
              <SettingsFields
                spec={spec}
                values={editing.config}
                onChange={(key, v) =>
                  setEditing((cur) => (cur ? { ...cur, config: { ...cur.config, [key]: v } } : cur))
                }
                idPrefix="model-cfg"
              />
            </Modal.Body>
            <Modal.Footer>
              <Button variant="outline-secondary" onClick={() => setEditing(null)}>
                <T text="Cancel" />
              </Button>
              <Button type="submit" disabled={saving}>
                {saving ? "Saving…" : editing.id ? "Save changes" : "Add model"}
              </Button>
            </Modal.Footer>
          </Form>
        )}
      </Modal>
    </Card>
  );
}
