// "Application from a description" (TODO-app-constructor AC5.1): a name and an
// LLM provider, then a React application and its construction are made and
// Plan & build opens on them, where the description is written. The same
// effect the onboarding's Generative AI path ends on.

import { useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";

import { navigate } from "../App";
import type { ListLlmProvidersResponse } from "../client";
import { constructPath } from "../construction";
import { T, useT } from "../i18n";
import { onboardingEffects } from "../onboarding/effects";

/** The provider the dialog starts on: the installation's default, else the first. */
export function initialProvider(providers: ListLlmProvidersResponse): string {
  return (providers.find((p) => p.is_default) ?? providers[0])?.name ?? "";
}

export function FromDescriptionModal({
  providers,
  onClose,
  create = onboardingEffects.constructApplication,
}: {
  /** `null`: the providers could not be loaded. */
  providers: ListLlmProvidersResponse | null;
  onClose: () => void;
  create?: typeof onboardingEffects.constructApplication;
}) {
  const { t } = useT();
  const [name, setName] = useState("My application");
  const [provider, setProvider] = useState(() => initialProvider(providers ?? []));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const none = providers !== null && providers.length === 0;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const app = await create({ name: name.trim(), provider });
      onClose();
      navigate(constructPath(app.id));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };

  return (
    <Modal show onHide={onClose} centered>
      <Form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Application from a description" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <p className="text-secondary">
            <T text="Saltcorn creates a React application and opens Plan & build, where you describe it in a few paragraphs. From that description it plans phases and builds them, one task at a time." />
          </p>
          {error && <Alert variant="danger">{error}</Alert>}
          {providers === null ? (
            <Alert variant="danger">
              <T text="The LLM providers could not be loaded." />
            </Alert>
          ) : none ? (
            <Alert variant="warning">
              <T text="Planning and building needs an LLM provider, and there is none yet." />{" "}
              <a href="#/llm-providers/new" onClick={onClose}>
                <T text="Connect an LLM provider" />
              </a>
            </Alert>
          ) : (
            <>
              <Form.Group className="mb-3" controlId="from-description-name">
                <Form.Label>
                  <T text="Application name" />
                </Form.Label>
                <Form.Control value={name} onChange={(e) => setName(e.target.value)} autoFocus />
              </Form.Group>
              <Form.Group controlId="from-description-provider">
                <Form.Label>
                  <T text="LLM provider" />
                </Form.Label>
                <Form.Select value={provider} onChange={(e) => setProvider(e.target.value)}>
                  {providers.map((p) => (
                    <option key={p.id} value={p.name}>
                      {p.name}
                    </option>
                  ))}
                </Form.Select>
              </Form.Group>
            </>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onClose} disabled={busy}>
            <T text="Cancel" />
          </Button>
          <Button
            type="submit"
            variant="primary"
            disabled={busy || !providers?.length || name.trim() === "" || provider === ""}
          >
            {busy ? t("Creating…") : t("Create application")}
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}
