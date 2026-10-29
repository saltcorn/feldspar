// The Backup tab's last panel: **Clear all**, back to an empty installation.
//
// The button only opens a dialog; nothing is cleared until its OK. The dialog
// lists every file store with the directory it occupies, each ticked, because a
// store's files outlive its definition unless they are removed too, and which
// directories may go is a question only the admin can answer — a local store is
// often a folder that was there before Saltcorn was.
//
// Everything goes, the accounts included — the admin pressing OK as well. So
// the dialog says so, and after the clear it shows what happened with a
// Continue button that reloads the admin UI, which finds no user and shows the
// create-first-user screen, as a fresh installation does.

import { useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import { type ClearAllStore, clearAllBody, toggleKept } from "../clearAll";
import type { ClearAllResponse } from "../client";
import { AlertBody } from "../layout";
import { T, useT } from "../i18n";

export function ClearAllPanel() {
  const { t } = useT();
  const [stores, setStores] = useState<ClearAllStore[] | null>(null);
  const [kept, setKept] = useState<Set<string>>(new Set());
  const [opening, setOpening] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<ClearAllResponse | null>(null);

  const open = async () => {
    setOpening(true);
    setError(null);
    setResult(null);
    try {
      const preview = await api.getClearAllPreview();
      setKept(new Set());
      setStores(preview.file_stores);
    } catch (e) {
      setError(
        e instanceof Error ? e.message : "Could not read the file stores.",
      );
    } finally {
      setOpening(false);
    }
  };

  const confirm = async () => {
    if (!stores) return;
    setBusy(true);
    setError(null);
    try {
      setResult(await api.clearAll(clearAllBody(stores, kept)));
    } catch (e) {
      setError(
        e instanceof Error ? e.message : "Could not clear the installation.",
      );
      setStores(null);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card mb-4 border-danger">
      <div className="card-header">
        <div>
          <h3 className="card-title">
            <T text="Clear all" />
          </h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="Reset this installation to the empty state: every table in the primary database is dropped, and everything else is deleted — applications, file stores, connections, agents, triggers, models, modules, settings and every user account, yours included." />
          </p>
        </div>
      </div>
      <div className="card-body">
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        <Button
          variant="danger"
          disabled={opening || busy}
          onClick={() => void open()}
        >
          {opening && <Spinner size="sm" className="me-2" />}
          <T text="Clear all" />
        </Button>
      </div>

      {stores && (
        <Modal
          show
          onHide={() =>
            result ? window.location.reload() : !busy && setStores(null)
          }
          centered
          backdrop={result ? "static" : true}
        >
          <Modal.Header closeButton={!busy && !result}>
            <Modal.Title>
              <T text="Clear all" />
            </Modal.Title>
          </Modal.Header>
          {result ? (
            <>
              <Modal.Body>
                <ul className="mb-3">
                  {result.cleared.map((line) => (
                    <li key={line}>{line}</li>
                  ))}
                  {result.warnings.map((line) => (
                    <li key={line} className="text-danger">
                      {line}
                    </li>
                  ))}
                </ul>
                <p className="mb-0">
                  <T text="Every user account has been deleted. Continue to create the first administrator account." />
                </p>
              </Modal.Body>
              <Modal.Footer>
                <Button
                  variant="primary"
                  onClick={() => window.location.reload()}
                >
                  <T text="Continue" />
                </Button>
              </Modal.Footer>
            </>
          ) : (
            <>
              <Modal.Body>
                <p>
                  <T text="Every table in the primary database will be dropped, and every application, file store, connection, agent, trigger, model, module, setting and user account deleted — yours included, so you will be signed out and asked to create the first administrator again. This cannot be undone." />
                </p>
                {stores.length > 0 ? (
                  <>
                    <p className="mb-2">
                      <T text="Also delete these file stores' files from disk:" />
                    </p>
                    {stores.map((store) => (
                      <Form.Check
                        key={store.name}
                        id={`clear-all-store-${store.name}`}
                        className="mb-2"
                        checked={!kept.has(store.name)}
                        disabled={busy}
                        onChange={() =>
                          setKept((k) => toggleKept(k, store.name))
                        }
                        label={
                          <>
                            <strong>{store.name}</strong>{" "}
                            <span className="text-secondary">
                              ({store.backend})
                            </span>
                            <div className="small text-secondary font-monospace">
                              {store.directory ?? t("no directory on disk")}
                            </div>
                          </>
                        }
                      />
                    ))}
                  </>
                ) : (
                  <p className="text-secondary mb-0">
                    <T text="There are no file stores." />
                  </p>
                )}
              </Modal.Body>
              <Modal.Footer>
                <Button
                  variant="secondary"
                  disabled={busy}
                  onClick={() => setStores(null)}
                >
                  <T text="Cancel" />
                </Button>
                <Button
                  variant="danger"
                  disabled={busy}
                  onClick={() => void confirm()}
                >
                  {busy && <Spinner size="sm" className="me-2" />}
                  <T text="OK" />
                </Button>
              </Modal.Footer>
            </>
          )}
        </Modal>
      )}
    </div>
  );
}
