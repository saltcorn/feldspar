// Create / edit an LLM provider. The same discipline `FileStoreForm` holds to:
// **no screen knows a specific backend**. A backend declares its settings as
// `FormField`s and `SettingsFields` renders them, so there is no mention of
// OpenAI or Anthropic below — including for the API key, which is a password
// input because the field says `secret`, not because this file knows what an
// `api_key` is.
//
// Three things here are not in the file-store form, and all follow from what a
// provider is:
//
//   - **The key round trip.** An existing provider's key arrives as the
//     sentinel, and sending it back unchanged tells the server to keep what it
//     has. So the form does nothing special on save: it submits what it holds,
//     and the sentinel *is* the "unchanged" signal. What it must not do is
//     rewrite the field, which is why the config is passed through
//     `buildConfig` exactly as any other settings bag.
//   - **Its models.** A provider serves several models, and each is a row of
//     its own with its prices and capabilities (`LlmModels`), listed below the
//     provider's settings once the provider is saved.
//   - **Testing.** A provider that saves fine can still be unusable — a
//     revoked key, a retired model, an endpoint that is down — and none of that
//     is knowable without a request. What is tested is one model through one
//     key, so the *Test* button is on each model row, and it sends the
//     provider settings as the form holds them.
//
// A provider the server's configuration file supplies (`from_config_file`) is
// shown, not edited: the fields are disabled, there is no Save, and its models
// can be tested but not changed. The server refuses every write to it anyway;
// this is so the admin is not invited to try.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type {
  CreateLlmProviderRequest,
  ListLlmProviderBackendsResponse,
  ListLlmProvidersResponse,
} from "../client";
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { SettingsFields, buildConfig, readConfig } from "../settings";
import { LlmModels } from "./LlmModels";
import { T } from "../i18n";

type BackendInfo = ListLlmProviderBackendsResponse[number];
type ProviderItem = ListLlmProvidersResponse[number];

export function LlmProviderForm({ providerId }: { providerId?: string }) {
  const [backends, setBackends] = useState<BackendInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [backendName, setBackendName] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [fromConfigFile, setFromConfigFile] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const list = await api.listLlmProviderBackends();
        let existing: ProviderItem | undefined;
        if (providerId) {
          existing = (await api.listLlmProviders()).find((p) => p.id === providerId);
          if (!existing) {
            if (!cancelled) setLoadError("That LLM provider no longer exists.");
            return;
          }
        }
        if (cancelled) return;
        setBackends(list);
        if (existing) {
          setName(existing.name);
          setDescription(existing.description);
          setBackendName(existing.backend);
          setFromConfigFile(existing.from_config_file);
          // Whatever the server sent, sentinel included: the form's job is to
          // hand it back unchanged unless the admin types over it.
          setConfig(readConfig(existing.config));
        } else {
          setBackendName(list[0]?.name ?? "");
        }
      } catch {
        if (!cancelled) setLoadError("Could not load the LLM provider backends.");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [providerId]);

  const selected = backends?.find((b) => b.name === backendName);
  const spec = selected?.config_spec ?? [];

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const body: CreateLlmProviderRequest = {
        name: name.trim(),
        description: description.trim(),
        backend: backendName,
        config: buildConfig(spec, config),
      };
      if (providerId) {
        await api.updateLlmProvider(providerId, body);
      } else {
        await api.createLlmProvider(body);
      }
      navigate("/llm-providers");
    } catch (err) {
      setError(errorMessage(err, "Could not save the LLM provider."));
      setBusy(false);
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!backends) {
    return (
      <PageBody>
        <div className="text-center py-5">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle={<T text="Automation" />}
        title={
          fromConfigFile
            ? "LLM provider"
            : providerId
              ? "Edit LLM provider"
              : "New LLM provider"
        }
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/llm-providers")}>
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {fromConfigFile && (
          <Alert variant="info">
            <T text="This provider is set in the server's configuration file (feldspar.toml), so it cannot be changed here. Agents can use it, and you can make it — or a provider you add — the default." />
          </Alert>
        )}
        <Form onSubmit={submit}>
          <fieldset disabled={fromConfigFile}>
            <Form.Group className="mb-3" controlId="providerName">
              <Form.Label><T text="Name" /></Form.Label>
              <Form.Control value={name} required onChange={(e) => setName(e.target.value)} />
              <Form.Text muted><T text="How an agent refers to this provider." /></Form.Text>
            </Form.Group>

            <Form.Group className="mb-3" controlId="providerDescription">
              <Form.Label><T text="Description" /></Form.Label>
              <Form.Control
                value={description}
                onChange={(e) => setDescription(e.target.value)}
              />
            </Form.Group>

            <Card className="mb-3">
              <Card.Header><T text="Backend" /></Card.Header>
              <Card.Body>
                <Form.Group className="mb-3" controlId="providerBackend">
                  <Form.Label><T text="Backend" /></Form.Label>
                  <Form.Select
                    value={backendName}
                    onChange={(e) => setBackendName(e.target.value)}
                  >
                    {backends.map((b) => (
                      <option key={b.name} value={b.name}>
                        {b.name}
                      </option>
                    ))}
                  </Form.Select>
                  <Form.Text muted>
                    <T text="Any endpoint speaking the same API is reached by changing the base URL — a gateway, a local server, another vendor." />
                  </Form.Text>
                </Form.Group>

                {/* The backend's own settings, rendered from its config_spec. The
                    API key is a password field because the spec says `secret`. */}
                <SettingsFields
                  spec={spec}
                  values={config}
                  onChange={(key, v) => setConfig((c) => ({ ...c, [key]: v }))}
                  idPrefix="provider-cfg"
                />
              </Card.Body>
            </Card>

          </fieldset>

          {!fromConfigFile && (
            <div className="btn-list">
              <Button type="submit" disabled={busy}>
                {busy ? "Saving…" : providerId ? "Save changes" : "Create provider"}
              </Button>
            </div>
          )}
        </Form>

        <div className="mt-4">
          {providerId ? (
            // The saved backend's models. A changed backend is saved first:
            // model settings are declared per backend, so the list would show
            // settings the stored rows were not validated against.
            <LlmModels
              providerId={providerId}
              backend={backendName}
              providerConfig={buildConfig(spec, config)}
              readOnly={fromConfigFile}
            />
          ) : (
            <p className="text-muted small">
              <T text="Save the provider to add its models. Each model it serves is a row of its own, with its own prices and settings." />
            </p>
          )}
        </div>
      </PageBody>
    </>
  );
}
