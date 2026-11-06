// Create / edit a file store. The point `ApplicationForm` proves for
// frameworks, held to for backends: **no screen knows a specific backend**.
//
// A backend declares two things and this renders both without knowing what
// either means:
//
//   - its **settings**, as `FormField`s (§6.2) — rendered by `SettingsFields`;
//   - its **operations**, as `Operation`s — rendered as buttons here.
//
// The second exists because some backends offer *acts*, not just values. A git
// store generates a deploy key before it is saved, and pulls, pushes and commits
// afterwards. Writing those as `backendName === "git"` branches would have made
// an operation something only a built-in backend could have: a backend supplied
// by a plugin, or through `sc-code` in a guest language, could declare settings
// and never a button. So they are declared data too, and this file contains no
// mention of git.
//
// The two operation **scopes** are why there are two places buttons appear, and
// the distinction is the substance rather than layout:
//
//   - `configure` runs against settings the admin is still editing and can
//     change them — which is what makes "generate a deploy key" possible at all,
//     since the key must exist before saving a git store clones it.
//   - `instance` runs against a saved store. `automatic` ones run when the
//     screen opens, for the operation whose whole job is to report state.
//
// One step past buttons: when an automatic operation's `data` is a
// source-control status (`parseScmStatus`), the store is a working copy and its
// operations are drawn as a VS Code-style Source Control panel instead of a
// column of forms (`SourceControl.tsx`). That is still decided by what the
// backend *answers* — a plugin backend returning the same payload gets the same
// panel — so this file still never asks which backend it is looking at.
//
// Saving does not require the store to be reachable (§1.2): a well-formed
// definition whose directory is missing is saved, and the failure to connect is
// reported. That is deliberate — demanding a reachable directory would make a
// store whose disk was unmounted uneditable, and editing it is the repair.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type {
  CreateFileStoreRequest,
  ListFileStoreBackendsResponse,
  ListFileStoresResponse,
} from "../client";
import { navigate } from "../App";
import { IconArrowLeft, IconFolder, IconRefresh } from "../icons";
import { AlertBody, PageBody, PageHeader } from "../layout";
import { OptionalRoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import {
  SettingsFields,
  buildConfig,
  readConfig,
  type FieldSpec,
} from "../settings";
import { T, useT } from "../i18n";
import { isScmOperation, parseScmStatus, type ScmStatus } from "../sourceControl";
import { IconButton, SourceControl } from "./SourceControl";

type BackendInfo = ListFileStoreBackendsResponse[number];
type OperationInfo = BackendInfo["operations"][number];
type StoreItem = ListFileStoresResponse[number];

/** Values entered for each operation's arguments, keyed by operation name. */
type OperationInputs = Record<string, Record<string, string>>;

export function FileStoreForm({ storeId }: { storeId?: string }) {
  const { t } = useT();
  const roles = useRoles();
  const [backends, setBackends] = useState<BackendInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // A save that succeeded but left the store unconnected: not an error, but the
  // admin needs to know the store is not usable yet and why.
  const [warning, setWarning] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [backendName, setBackendName] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [minRole, setMinRole] = useState<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const list = await api.listFileStoreBackends();
        let existing: StoreItem | undefined;
        if (storeId) {
          existing = (await api.listFileStores()).find((s) => s.id === storeId);
          if (!existing) {
            if (!cancelled) setLoadError("That file store no longer exists.");
            return;
          }
        }
        if (cancelled) return;
        setBackends(list);
        if (existing) {
          setName(existing.name);
          setDescription(existing.description);
          setBackendName(existing.backend);
          setConfig(readConfig(existing.config));
          setMinRole(existing.min_role ?? null);
        } else {
          setBackendName(list[0]?.name ?? "");
        }
      } catch {
        if (!cancelled) setLoadError("Could not load the file-store backends.");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [storeId]);

  const selected = backends?.find((b) => b.name === backendName);
  const spec = selected?.config_spec ?? [];
  const operations = selected?.operations ?? [];

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setWarning(null);
    try {
      const body: CreateFileStoreRequest = {
        name: name.trim(),
        description: description.trim(),
        backend: backendName,
        config: buildConfig(spec, config),
        min_role: minRole,
      };
      const saved = storeId
        ? await api.updateFileStore(storeId, body)
        : await api.createFileStore(body);

      // Saved, but is it usable? The server connects on save and reports the
      // outcome, so the admin finds out now rather than at the next restart.
      if (!saved.connected) {
        setWarning(
          saved.error ??
            "The store was saved but could not be connected. Check its settings.",
        );
        setBusy(false);
        return;
      }
      navigate("/file-stores");
    } catch (err) {
      setError(errorMessage(err, "Could not save the file store."));
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
        pretitle={t("Data")}
        title={storeId ? "Edit file store" : "New file store"}
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/file-stores")}>
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {warning && (
          <Alert variant="warning" onClose={() => setWarning(null)} dismissible>
            <AlertBody>
              <Alert.Heading className="h6"><T text="Saved, but not connected" /></Alert.Heading>
              <div className="text-break">{warning}</div>
              <hr />
              <div className="mb-0 small">
                <T text="The definition is stored and you can keep editing it. Fix the settings and save again, or go back to the list." />
              </div>
            </AlertBody>
          </Alert>
        )}

        <Form onSubmit={submit}>
          <Row>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="storeName">
                <Form.Label><T text="Name" /></Form.Label>
                <Form.Control
                  value={name}
                  required
                  onChange={(e) => setName(e.target.value)}
                />
                
              </Form.Group>
            </Col>
            <Col md={6}>
              <OptionalRoleSelect
                id="storeMinRole"
                label={t("Minimum role to access")}
                value={minRole}
                roles={roles}
                blank="Unrestricted"
                onChange={setMinRole}
              >               
              </OptionalRoleSelect>
            </Col>
          </Row>

          <Form.Group className="mb-3" controlId="storeDescription">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </Form.Group>

          <Card className="mb-3">
            <Card.Header><T text="Backend" /></Card.Header>
            <Card.Body>
              <Form.Group className="mb-3" controlId="storeBackend">
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
              </Form.Group>

              {/* The backend's own settings, rendered from its config_spec — no
                  backend-specific code lives here. `locked` on an existing store
                  is the generic form of "this one was decided when the store was
                  created": a backend declares a setting `create_only` and the
                  control stops being editable, with nothing here knowing which
                  setting or which backend. */}
              <SettingsFields
                spec={spec}
                values={config}
                onChange={(key, v) => setConfig((c) => ({ ...c, [key]: v }))}
                idPrefix="store-cfg"
                locked={Boolean(storeId)}
              />

              {/* Operations that run against these unsaved settings and may fill
                  them in. They belong beside the settings because that is what
                  they change. */}
              <ConfigureOperations
                backend={backendName}
                operations={operations.filter((op) => op.scope === "configure")}
                storeName={name}
                spec={spec}
                config={config}
                onConfig={(patch) =>
                  setConfig((c) => ({ ...c, ...readConfig(patch) }))
                }
              />
            </Card.Body>
          </Card>

          <Button type="submit" disabled={busy}>
            {busy ? "Saving…" : storeId ? "Save changes" : "Create file store"}
          </Button>
        </Form>

        {/* Only for a store that exists: an instance operation has nothing to
            run against until there is a saved store. */}
        {storeId && (
          <InstanceOperations
            storeId={storeId}
            storeName={name}
            operations={operations.filter((op) => op.scope === "instance")}
          />
        )}
      </PageBody>
    </>
  );
}

/** The buttons for `configure`-scope operations, which act on the settings above
 * them and can rewrite them.
 *
 * The whole config is posted with the request, because the operation is being
 * run *against what the admin has typed* — a store that does not exist yet has
 * no stored settings to read. What comes back replaces them. */
function ConfigureOperations({
  backend,
  operations,
  storeName,
  spec,
  config,
  onConfig,
}: {
  backend: string;
  operations: OperationInfo[];
  storeName: string;
  spec: FieldSpec[];
  config: Record<string, string>;
  onConfig: (patch: unknown) => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [output, setOutput] = useState<string | null>(null);
  const [inputs, setInputs] = useState<OperationInputs>({});

  if (operations.length === 0) return null;

  const run = async (op: OperationInfo) => {
    setBusy(op.name);
    setError(null);
    setOutput(null);
    try {
      const res = await api.runBackendOperation(backend, op.name, {
        name: storeName.trim(),
        config: buildConfig(spec, config),
        input: buildConfig(op.input_spec, inputs[op.name] ?? {}),
      });
      onConfig(res.config);
      setOutput(res.output);
    } catch (err) {
      setError(errorMessage(err, `Could not run ${op.label}.`));
    }
    setBusy(null);
  };

  return (
    <div className="mt-3 border-top pt-3">
      {error && <Alert variant="danger">{error}</Alert>}
      <OperationOutput output={output} onClose={() => setOutput(null)} />
      {operations.map((op) => (
        <div key={op.name} className="mb-3">
          <SettingsFields
            spec={op.input_spec}
            values={inputs[op.name] ?? {}}
            onChange={(key, v) =>
              setInputs((all) => ({
                ...all,
                [op.name]: { ...(all[op.name] ?? {}), [key]: v },
              }))
            }
            idPrefix={`op-${op.name}`}
          />
          <Button
            variant="outline-secondary"
            onClick={() => void run(op)}
            disabled={busy !== null}
          >
            {busy === op.name ? "Working…" : op.label}
          </Button>
          {op.description && (
            <Form.Text className="d-block mt-1">{op.description}</Form.Text>
          )}
        </div>
      ))}
    </div>
  );
}

/** The card for `instance`-scope operations on a saved store.
 *
 * `automatic` operations are run on open and their output shown — that is the
 * generic form of "what state is this store in?", which an admin needs in front
 * of them before choosing what to do. The rest are buttons, each rendering
 * whatever arguments it declared. Every operation re-runs the automatic ones
 * afterwards, because every one of them can change what those report. */
function InstanceOperations({
  storeId,
  storeName,
  operations,
}: {
  storeId: string;
  storeName: string;
  operations: OperationInfo[];
}) {
  const { t } = useT();
  const [reports, setReports] = useState<Record<string, string>>({});
  // The working copy, when an automatic operation reported one — which is what
  // turns the store's operations into the source-control panel.
  const [scm, setScm] = useState<ScmStatus | null>(null);
  const [inputs, setInputs] = useState<OperationInputs>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [output, setOutput] = useState<string | null>(null);

  const automatic = operations.filter((op) => op.automatic);
  // The panel draws its own operations; whatever else the backend offers is
  // still a generic button.
  const manual = operations.filter(
    (op) => !op.automatic && !(scm && isScmOperation(op.name)),
  );

  const refresh = async () => {
    for (const op of automatic) {
      try {
        const res = await api.runFileStoreOperation(storeId, op.name, { input: {} });
        setReports((r) => ({ ...r, [op.name]: res.output }));
        const status = parseScmStatus(res.data);
        if (status) setScm(status);
      } catch (err) {
        setReports((r) => ({
          ...r,
          [op.name]: errorMessage(err, `Could not run ${op.label}.`),
        }));
      }
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [storeId, operations.length]);

  if (operations.length === 0) return null;

  const run = async (op: OperationInfo) => {
    setBusy(op.name);
    setError(null);
    setOutput(null);
    try {
      const res = await api.runFileStoreOperation(storeId, op.name, {
        input: buildConfig(op.input_spec, inputs[op.name] ?? {}),
      });
      setOutput(res.output);
      // Arguments are per-run, not settings: clearing them stops a commit
      // message being reused by accident on the next press.
      setInputs((all) => ({ ...all, [op.name]: {} }));
    } catch (err) {
      // The backend's own message, which is the actionable part — "Permission
      // denied (publickey)", "rejected: non-fast-forward" — not a generic
      // failure.
      setError(errorMessage(err, `Could not run ${op.label}.`));
    }
    setBusy(null);
    await refresh();
  };

  return (
    <Card className="mb-3">
      <Card.Header className="d-flex align-items-center">
        {scm ? <T text="Source control" /> : <T text="Operations" />}
        {scm && (
          // Re-reads the working copy, picking up files changed on disk since —
          // through the file manager, the IDE, or anything else.
          <span className="ms-auto">
            <IconButton
              label={t("Refresh")}
              disabled={refreshing}
              onClick={() => {
                setRefreshing(true);
                void refresh().finally(() => setRefreshing(false));
              }}
            >
              {refreshing ? (
                <Spinner animation="border" size="sm" />
              ) : (
                <IconRefresh className="icon-1" />
              )}
            </IconButton>
          </span>
        )}
      </Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        <OperationOutput output={output} onClose={() => setOutput(null)} />

        {scm ? (
          <div className="mb-3">
            <SourceControl
              storeId={storeId}
              status={scm}
              report={automatic.map((op) => reports[op.name] ?? "").join("\n")}
              declared={operations.map((op) => op.name)}
              onStatus={setScm}
            />
          </div>
        ) : (
          automatic.map((op) =>
            reports[op.name] ? (
              <pre
                key={op.name}
                className="small text-break mb-3"
                style={{ whiteSpace: "pre-wrap" }}
              >
                {reports[op.name]}
              </pre>
            ) : null,
          )
        )}

        {manual.map((op) => (
          <div key={op.name} className="mb-3">
            <SettingsFields
              spec={op.input_spec}
              values={inputs[op.name] ?? {}}
              onChange={(key, v) =>
                setInputs((all) => ({
                  ...all,
                  [op.name]: { ...(all[op.name] ?? {}), [key]: v },
                }))
              }
              idPrefix={`op-${op.name}`}
            />
            <Button
              variant="outline-secondary"
              onClick={() => void run(op)}
              disabled={busy !== null || !argumentsGiven(op, inputs[op.name])}
            >
              {busy === op.name ? "Working…" : op.label}
            </Button>
            {op.description && (
              <Form.Text className="d-block mt-1">{op.description}</Form.Text>
            )}
          </div>
        ))}

        <div className="btn-list">
          <Button variant="outline-primary" href={`#/files/${encodeURIComponent(storeName)}`}>
            <IconFolder className="icon-2" />
            <T text="Change files" />
          </Button>
          {!scm && (
            <Button variant="outline-secondary" onClick={() => void refresh()}>
              <T text="Refresh" />
            </Button>
          )}
        </div>
      </Card.Body>
    </Card>
  );
}

/** Whether an operation's required arguments have been filled in — so a button
 * that would fail validation on the server is disabled rather than tried. */
function argumentsGiven(
  op: OperationInfo,
  values: Record<string, string> | undefined,
): boolean {
  return op.input_spec
    .filter((f) => f.required)
    .every((f) => (values?.[f.name] ?? "").trim() !== "");
}

/** Whatever an operation had to say, shown verbatim. Text, because the useful
 * part is usually a command's own words. */
function OperationOutput({
  output,
  onClose,
}: {
  output: string | null;
  onClose: () => void;
}) {
  if (!output) return null;
  return (
    <Alert variant="secondary" onClose={onClose} dismissible>
      <pre className="mb-0 small text-break" style={{ whiteSpace: "pre-wrap" }}>
        {output}
      </pre>
    </Alert>
  );
}
