// "Application from a description" (TODO-app-constructor AC5.1): a name and an
// LLM provider, then a React application and its construction are made and
// Plan & build opens on them, where the description is written. The same
// effect the onboarding's Generative AI path ends on. Plan & build is the
// bundled App Constructor module: when it is not installed, the dialog says so
// and installs it first.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";

import { navigate } from "../App";
import type { ListLlmProvidersResponse } from "../client";
import { constructPath } from "../appConstructor";
import { T, useT } from "../i18n";
import { onboardingEffects } from "../onboarding/effects";

/** The provider the dialog starts on: the installation's default, else the first. */
export function initialProvider(providers: ListLlmProvidersResponse): string {
  return (providers.find((p) => p.is_default) ?? providers[0])?.name ?? "";
}

/** Whether the App Constructor module is there: still asking, installed,
 * missing, or not known (the modules could not be listed). */
export type ConstructorPresence = "checking" | "installed" | "missing" | "unknown";

export function FromDescriptionModal({
  providers,
  onClose,
  create = onboardingEffects.constructApplication,
  installed = onboardingEffects.constructorInstalled,
  install = onboardingEffects.installConstructor,
}: {
  /** `null`: the providers could not be loaded. */
  providers: ListLlmProvidersResponse | null;
  onClose: () => void;
  create?: typeof onboardingEffects.constructApplication;
  installed?: typeof onboardingEffects.constructorInstalled;
  install?: typeof onboardingEffects.installConstructor;
}) {
  const { t } = useT();
  const [presence, setPresence] = useState<ConstructorPresence>("checking");
  useEffect(() => {
    let live = true;
    installed()
      .then((yes) => live && setPresence(yes ? "installed" : "missing"))
      .catch(() => live && setPresence("unknown"));
    return () => {
      live = false;
    };
  }, [installed]);
  const [name, setName] = useState("My application");
  const [provider, setProvider] = useState(() => initialProvider(providers ?? []));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const none = providers !== null && providers.length === 0;

  const installModule = async () => {
    setBusy(true);
    setError(null);
    try {
      await install();
      setPresence("installed");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

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
          {presence === "missing" && (
            <Alert variant="warning">
              <p>
                <T text="Plan & build comes with Saltcorn as the App Constructor module, which is not installed yet. Installing it adds its tables, the Plan & build screen on each React application, and the agents that plan and build." />
              </p>
              <Button variant="primary" onClick={() => void installModule()} disabled={busy}>
                {busy ? t("Installing the App Constructor…") : t("Install the App Constructor")}
              </Button>
            </Alert>
          )}
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
            disabled={
              busy ||
              presence === "missing" ||
              presence === "checking" ||
              !providers?.length ||
              name.trim() === "" ||
              provider === ""
            }
          >
            {busy ? t("Creating…") : t("Create application")}
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}
