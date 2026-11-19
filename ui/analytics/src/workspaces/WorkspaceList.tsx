// The list of workspaces, on the Analytics UI's front page below the datasets
// (analytics TODO A1.15, A1.21).
//
// Every workspace, most recently used first, each opened, renamed or deleted
// from its row; and a new one made by name and kind (the heading's "+ New
// workspace" button opens a dialog asking for both), the kinds not here yet
// listed, disabled and marked not yet available. In a self-serve
// application (A9.3) the server lists its own and the kinds it opens; a
// workspace is renamed, shared and deleted by its owner, and new ones are made
// only where the application allows it.

import { useCallback, useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { AddButton } from "../AddButton";
import { api, errorMessage } from "../api";
import type { ListWorkspacesResponse } from "../client";
import { T, useT } from "../i18n";
import { usePane } from "../panes";
import { workspaceKindName } from "../labels";
import { ShareSelect } from "../share";
import { useShell } from "../shell";
import { firstAvailable, kindOptions, type KindItem } from "./kinds";

type WorkspaceItem = ListWorkspacesResponse[number];

export function WorkspaceList() {
  const { t } = useT();
  const pane = usePane();
  const { application } = useShell();
  const mayCreate = !application || application.createWorkspaces;
  const [kinds, setKinds] = useState<KindItem[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [renaming, setRenaming] = useState<WorkspaceItem | null>(null);
  const [deleting, setDeleting] = useState<WorkspaceItem | null>(null);

  const load = useCallback(async () => {
    try {
      const [k, w] = await Promise.all([api.listWorkspaceKinds(), api.listWorkspaces()]);
      setKinds(k);
      setWorkspaces(w);
    } catch (err) {
      setError(errorMessage(err, t("Could not load the workspaces.")));
    }
  }, [t]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <section className="card mb-4">
      <div className="card-header">
        <h2 className="card-title">
          <T text="Workspaces" />
        </h2>
        <div className="card-actions d-flex gap-2">
          {mayCreate && (
            <AddButton
              label={t("New workspace")}
              onClick={() => setCreating(true)}
            />
          )}
        </div>
      </div>
      {error && (
        <Alert className="m-3" variant="danger" onClose={() => setError(null)} dismissible>
          {error}
        </Alert>
      )}

      {creating && (
        <NewWorkspaceDialog kinds={kinds} onClose={() => setCreating(false)} />
      )}

      {workspaces && workspaces.length === 0 && (
        <p className="card-body text-secondary mb-0">
          <T text="No workspaces yet." />
        </p>
      )}
      {workspaces && workspaces.length > 0 && (
        <Table hover responsive className="card-table">
          <thead>
            <tr>
              <th>
                <T text="Name" />
              </th>
              <th>
                <T text="Kind" />
              </th>
              <th>
                <T text="Last changed" />
              </th>
              <th />
            </tr>
          </thead>
          <tbody>
            {workspaces.map((w) => (
              <tr key={w.id}>
                <td>
                  <a href={pane.href({ name: "workspace", id: w.id })}>{w.name}</a>
                </td>
                <td>{workspaceKindName(w.kind, t)}</td>
                <td className="text-secondary">{new Date(w.updated_at).toLocaleString()}</td>
                <td className="text-end text-nowrap">
                  {application && w.may_change && (
                    <>
                      <ShareSelect
                        value={w.share_role}
                        label={w.name}
                        onShare={async (role) => {
                          try {
                            await api.shareWorkspace(w.id, { share_role: role });
                            await load();
                          } catch (err) {
                            setError(errorMessage(err, t("Could not share the workspace.")));
                          }
                        }}
                      />{" "}
                    </>
                  )}
                  {w.may_change && (
                    <>
                      <Button size="sm" variant="outline-secondary" onClick={() => setRenaming(w)}>
                        <T text="Rename" />
                      </Button>{" "}
                      <Button size="sm" variant="outline-danger" onClick={() => setDeleting(w)}>
                        <T text="Delete" />
                      </Button>
                    </>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}

      {renaming && (
        <RenameDialog
          workspace={renaming}
          onClose={() => setRenaming(null)}
          onDone={() => {
            setRenaming(null);
            void load();
          }}
        />
      )}
      <Modal show={deleting !== null} onHide={() => setDeleting(null)}>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Delete workspace" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <T
            text="Delete “{name}”? Its state goes with it; the datasets it edited stay."
            args={{ name: deleting?.name ?? "" }}
          />
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={() => setDeleting(null)}>
            <T text="Cancel" />
          </Button>
          <Button
            variant="danger"
            onClick={async () => {
              if (!deleting) return;
              try {
                await api.deleteWorkspace(deleting.id);
              } catch (err) {
                setError(errorMessage(err, t("Could not delete the workspace.")));
              }
              setDeleting(null);
              void load();
            }}
          >
            <T text="Delete" />
          </Button>
        </Modal.Footer>
      </Modal>
    </section>
  );
}

/** A new workspace: its name and its kind, asked for in a dialog; made, it opens. */
function NewWorkspaceDialog({ kinds, onClose }: { kinds: KindItem[]; onClose: () => void }) {
  const { t } = useT();
  const pane = usePane();
  const [name, setName] = useState("");
  const [kind, setKind] = useState(() => firstAvailable(kinds));
  const [error, setError] = useState<string | null>(null);
  const noneHere = kinds.length > 0 && firstAvailable(kinds) === "";

  const create = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      const made = await api.createWorkspace({ name: name.trim(), kind });
      pane.go({ name: "workspace", id: made.id });
    } catch (err) {
      setError(errorMessage(err, t("Could not create the workspace.")));
    }
  };

  return (
    <Modal show onHide={onClose}>
      <Form onSubmit={create}>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="New workspace" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          <Form.Group controlId="workspace-name" className="mb-3">
            <Form.Label>
              <T text="Name" />
            </Form.Label>
            <Form.Control
              autoFocus
              value={name}
              placeholder={t("Houses data")}
              onChange={(e) => setName(e.target.value)}
            />
          </Form.Group>
          <Form.Group controlId="workspace-kind">
            <Form.Label>
              <T text="Kind" />
            </Form.Label>
            <Form.Select value={kind} onChange={(e) => setKind(e.target.value)}>
              {kindOptions(kinds, t).map((option) => (
                <option key={option.value} value={option.value} disabled={option.disabled}>
                  {option.label}
                </option>
              ))}
            </Form.Select>
          </Form.Group>
          {noneHere && (
            <Form.Text muted>
              <T text="No kind of workspace is available yet." />
            </Form.Text>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={name.trim() === "" || kind === ""}>
            <T text="Create" />
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}

function RenameDialog({
  workspace,
  onClose,
  onDone,
}: {
  workspace: WorkspaceItem;
  onClose: () => void;
  onDone: () => void;
}) {
  const { t } = useT();
  const [name, setName] = useState(workspace.name);
  const [error, setError] = useState<string | null>(null);
  const save = async (e: FormEvent) => {
    e.preventDefault();
    try {
      await api.updateWorkspace(workspace.id, { name: name.trim() });
      onDone();
    } catch (err) {
      setError(errorMessage(err, t("Could not rename the workspace.")));
    }
  };
  return (
    <Modal show onHide={onClose}>
      <Form onSubmit={save}>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Rename workspace" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          <Form.Control
            autoFocus
            value={name}
            aria-label={t("Name")}
            onChange={(e) => setName(e.target.value)}
          />
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={name.trim() === ""}>
            <T text="Rename" />
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}
