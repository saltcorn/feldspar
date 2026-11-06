// Create / edit a stream: a provider, its settings filled in, and who may
// observe it (TODO "Streams", task 7.3).
//
// The point this screen proves is the one the model form and the trigger form
// prove: **no screen knows a specific provider**. The admin picks a provider and
// the form renders whatever that provider's `config_spec` declares, so a
// provider supplied by a module gets a working form with no change to this
// bundle — and there is no mention of a broker, a topic or a QoS below.
//
// One thing here is not on the model form, and it is the thing GOALS asks for:
// the **element type is a function of the configuration** (§3), so the form
// asks the server what *this* configuration would produce and shows the answer
// under the settings. That is the same arrangement `ModelForm` makes for an
// outcome, debounced for the same reason, and it is what lets an admin see
// "JSON: temperature (float)" before saving rather than discovering on the
// Observe screen that they declared nothing.
//
// Saving reloads the supervisor server-side, so a stream saved enabled is
// connected by the time this navigates away — which is why there is no Start
// button anywhere on this screen.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { SaveStreamRequest } from "../client";
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { OptionalRoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import { SettingsFields, buildConfig, readConfig } from "../settings";
import {
  elementTypeSummary,
  nameProblem,
  providerSpec,
  readElementType,
  type StreamProviderInfo,
} from "../streams";
import { T, useT } from "../i18n";

/** How long the form waits before asking what the typed configuration would
 * produce. Long enough that typing a broker host is one request rather than
 * fourteen (`ModelForm`'s own figure). */
const DEBOUNCE_MS = 400;

export function StreamForm({ streamId }: { streamId?: string }) {
  const { t } = useT();
  const roles = useRoles();
  const [providers, setProviders] = useState<StreamProviderInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [ready, setReady] = useState(false);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [provider, setProvider] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [minRole, setMinRole] = useState<number | null>(null);
  const [enabled, setEnabled] = useState(true);

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const listed = await api.listStreamProviders();
        let existing: Awaited<ReturnType<typeof api.getStream>> | undefined;
        if (streamId) existing = await api.getStream(streamId);
        if (cancelled) return;
        setProviders(listed.providers);
        if (existing) {
          setName(existing.name);
          setDescription(existing.description);
          setProvider(existing.provider);
          // Whatever the server sent, the secret sentinel included: the form's
          // job is to hand it back unchanged unless the admin types over it
          // (§2.3), which is what makes a password survive an edit.
          setConfig(readConfig(existing.configuration));
          setMinRole(existing.min_role ?? null);
          setEnabled(existing.enabled);
        } else {
          setProvider(listed.providers[0]?.name ?? "");
        }
        setReady(true);
      } catch (err) {
        if (!cancelled) setLoadError(errorMessage(err, "Could not load the stream providers."));
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [streamId]);

  const spec = providerSpec(providers, provider);
  // Stringified so the effect below depends on the *values* rather than on a
  // fresh object every render.
  const configJson = JSON.stringify(buildConfig(spec, config));

  // What would this configuration produce? The server answers, because the
  // answer is the provider's (§3) and nothing in this bundle could compute it.
  useEffect(() => {
    if (!ready || provider === "") return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void api
        .listStreamProviders({ provider, configuration: configJson })
        .then((listed) => {
          if (!cancelled) setProviders(listed.providers);
        })
        .catch(() => undefined);
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [provider, configJson, ready]);

  const picked = providers?.find((p) => p.name === provider);
  const elementType = readElementType(picked?.element_type);
  const nameNote = nameProblem(name);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (nameNote) {
      setError(nameNote);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const body: SaveStreamRequest = {
        id: streamId ?? null,
        name: name.trim(),
        description: description.trim(),
        provider,
        configuration: buildConfig(spec, config),
        min_role: minRole,
        enabled,
      };
      await api.saveStream(body);
      navigate("/streams");
    } catch (err) {
      // The server validates the same rules and its message names the setting;
      // this form shapes the question, it does not decide the answer.
      setError(errorMessage(err, "Could not save the stream."));
      setBusy(false);
    }
  };

  if (loadError) {
    return (
      <>
        <PageHeader pretitle={t("Data")} title={t("Stream")} />
        <PageBody>
          <Alert variant="danger">{loadError}</Alert>
        </PageBody>
      </>
    );
  }

  if (!providers) {
    return (
      <PageBody>
        <Spinner animation="border" role="status" />
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle={t("Data")}
        title={streamId ? name || "Stream" : "New stream"}
        actions={
          <Button variant="outline-secondary" href="#/streams">
            <IconArrowLeft className="icon-2" />
            <T text="Streams" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {providers.length === 0 && (
          <Alert variant="info">
            <T text="No stream providers are registered. The built-in MQTT provider is behind a build feature, and a module can supply more." />
          </Alert>
        )}
        <Form onSubmit={(e) => void submit(e)}>
          <Row>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="streamName">
                <Form.Label>
                  <T text="Name" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control value={name} required onChange={(e) => setName(e.target.value)} />
                <Form.Text muted>
                  <T text="It names a trigger's channel and a socket path, so a rename breaks those references deliberately." />
                </Form.Text>
              </Form.Group>
            </Col>
            <Col md={6}>
              <OptionalRoleSelect
                id="streamMinRole"
                label={t("Minimum role to observe")}
                value={minRole}
                roles={roles}
                blank="Admin only"
                onChange={setMinRole}
              />
            </Col>
          </Row>

          <Form.Group className="mb-3" controlId="streamDescription">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </Form.Group>

          <Card className="mb-3">
            <Card.Header><T text="Provider" /></Card.Header>
            <Card.Body>
              <Form.Group className="mb-3" controlId="streamProvider">
                <Form.Label>
                  <T text="Provider" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Select
                  value={provider}
                  onChange={(e) => {
                    setProvider(e.target.value);
                    // A different provider declares different settings, and
                    // carrying the old bag over would submit settings the new
                    // one never asked for.
                    setConfig({});
                  }}
                >
                  <option value="">—</option>
                  {providers.map((p) => (
                    <option key={p.name} value={p.name}>
                      {p.label}
                      {p.module ? ` (${p.module})` : ""}
                    </option>
                  ))}
                </Form.Select>
                {picked?.description && <Form.Text muted>{picked.description}</Form.Text>}
              </Form.Group>

              <SettingsFields
                spec={spec}
                values={config}
                idPrefix="stream"
                onChange={(field, value) =>
                  setConfig((current) => ({ ...current, [field]: value }))
                }
              />

              {/* What this configuration would produce (§3). The error is the
                  sentence the form is waiting to be told — "`payload` is
                  `json` but no keys are declared" — and it is shown while the
                  admin is looking at the settings that caused it. */}
              {picked?.element_type_error ? (
                <Alert variant="warning" className="mb-0">
                  {picked.element_type_error}
                </Alert>
              ) : (
                elementType && (
                  <div className="text-muted">
                    Elements: <span className="font-monospace">{elementTypeSummary(elementType)}</span>
                  </div>
                )
              )}
            </Card.Body>
          </Card>

          <Form.Group className="mb-3" controlId="streamEnabled">
            <Form.Check
              type="checkbox"
              label={t("Enabled")}
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
            />
            <Form.Text muted>
              <T text="Saving an enabled stream connects it: there is no separate Start." />
            </Form.Text>
          </Form.Group>

          <div className="btn-list">
            <Button type="submit" disabled={busy || provider === ""}>
              {busy ? "Saving…" : "Save"}
            </Button>
            <Button variant="outline-secondary" href="#/streams">
              <T text="Cancel" />
            </Button>
          </div>
        </Form>
      </PageBody>
    </>
  );
}
