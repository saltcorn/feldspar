// The list of workspaces, on the Analytics UI's front page below the datasets
// (analytics TODO A1.15, A1.21).
//
// Every workspace, most recently used first, each opened, renamed or deleted
// from its row; and a new one made by name and kind, the kinds not here yet
// listed and disabled with the milestone that brings them.

import { useCallback, useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListWorkspacesResponse } from "../client";
import { T, useT } from "../i18n";
import { navigate } from "../router";
import { workspaceKindName } from "../labels";
import { firstAvailable, kindOptions, type KindItem } from "./kinds";

type WorkspaceItem = ListWorkspacesResponse[number];

export function WorkspaceList() {
  const { t } = useT();
  const [kinds, setKinds] = useState<KindItem[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [kind, setKind] = useState("");
  const [renaming, setRenaming] = useState<WorkspaceItem | null>(null);
  const [deleting, setDeleting] = useState<WorkspaceItem | null>(null);

  const load = useCallback(async () => {
    try {
      const [k, w] = await Promise.all([api.listWorkspaceKinds(), api.listWorkspaces()]);
      setKinds(k);
      setKind((current) => current || firstAvailable(k));
      setWorkspaces(w);
    } catch (err) {
      setError(errorMessage(err, t("Could not load the workspaces.")));
    }
  }, [t]);

  useEffect(() => {
    void load();
  }, [load]);

  const create = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      const made = await api.createWorkspace({ name: name.trim(), kind });
      navigate({ name: "workspace", id: made.id });
    } catch (err) {
      setError(errorMessage(err, t("Could not create the workspace.")));
    }
  };

  const noneHere = kinds.length > 0 && firstAvailable(kinds) === "";

  return (
    <section className="mb-5">
      <h2 className="h3 mb-3">
        <T text="Workspaces" />
      </h2>
      {error && (
        <Alert variant="danger" onClose={() => setError(null)} dismissible>
          {error}
        </Alert>
      )}

      <Card className="mb-4">
        <Card.Body>
          <Form onSubmit={create} className="d-flex gap-2 flex-wrap align-items-end">
            <Form.Group controlId="workspace-name">
              <Form.Label>
                <T text="New workspace" />
              </Form.Label>
              <Form.Control
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
            <Button type="submit" disabled={name.trim() === "" || kind === ""}>
              <T text="Create" />
            </Button>
          </Form>
          {noneHere && (
            <Form.Text muted>
              <T text="No kind of workspace can be created yet: each arrives with the milestone it names." />
            </Form.Text>
          )}
        </Card.Body>
      </Card>

      {workspaces && workspaces.length === 0 && (
        <p className="text-secondary">
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
                  <a href={`#/w/${encodeURIComponent(w.id)}`}>{w.name}</a>
                </td>
                <td>{workspaceKindName(w.kind, t)}</td>
                <td className="text-secondary">{new Date(w.updated_at).toLocaleString()}</td>
                <td className="text-end text-nowrap">
                  <Button size="sm" variant="outline-secondary" onClick={() => setRenaming(w)}>
                    <T text="Rename" />
                  </Button>{" "}
                  <Button size="sm" variant="outline-danger" onClick={() => setDeleting(w)}>
                    <T text="Delete" />
                  </Button>
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
