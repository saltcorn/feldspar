// Roles screen: list the roles that exist, add one, delete one.
//
// A role is a row in `_fd_roles` (design §7.1, §9): a number on the fixed 1–100
// scale, a name, and — in its attributes — whatever role-specific settings
// arrive later. `users.role` is a foreign key onto it, so this screen is a
// prerequisite for the Users screen rather than a decoration: a user cannot hold
// a role that does not exist here. That is also why it is reached from a button
// on Users rather than from the sidebar: nothing but a user has a use for a
// role, so it belongs to that section rather than beside it.
//
// A role can require two-factor authentication (design §7.2): everybody
// holding it is then taken through setting one up at their next sign-in. The
// public role has nobody signing in, so it has no switch.
//
// Admin (1) and Public (100) are marked built in and cannot be deleted. Without
// the first nobody can administer anything; without the second an anonymous
// caller has no role to be. The server refuses either way; the button is hidden
// so an admin is not invited to try.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api } from "../api";
import type { ListRolesResponse } from "../client";
import { PageBody, PageHeader } from "../layout";
import { T, useT } from "../i18n";

export function Roles() {
  const { t } = useT();
  const [roles, setRoles] = useState<ListRolesResponse | null>(null);
  const [number, setNumber] = useState(40);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async () => {
    try {
      setRoles(await api.listRoles());
    } catch {
      setError("Could not load roles.");
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const create = async (e: FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await api.createRole({ role: number, name: name.trim(), description: description.trim() });
      setName("");
      setDescription("");
      await load();
    } catch (e) {
      // The server's message is the useful part here — "role 40 is already used
      // by `Staff`" tells the admin what to change, and a generic sentence
      // would not.
      setError(e instanceof Error ? e.message : "Could not create the role.");
    } finally {
      setBusy(false);
    }
  };

  const requireSecondFactor = async (role: number, required: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.setRoleSecondFactor(role, { required });
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not change the role.");
    } finally {
      setBusy(false);
    }
  };

  const remove = async (role: number) => {
    setBusy(true);
    setError(null);
    try {
      await api.deleteRole(role);
      await load();
    } catch (e) {
      // Most often: users still hold it, and the message says how many.
      setError(e instanceof Error ? e.message : "Could not delete the role.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageHeader pretitle="Users" title={t("Roles")} />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <Row>
          <Col lg={7} className="mb-4">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title"><T text="Roles" /></h3>
              </div>
              <Table hover responsive className="card-table table-vcenter">
                <thead>
                  <tr>
                    <th><T text="Number" /></th>
                    <th><T text="Name" /></th>
                    <th><T text="Description" /></th>
                    <th><T text="Requires two-factor" /></th>
                    <th className="text-end"><T text="Actions" /></th>
                  </tr>
                </thead>
                <tbody>
                  {roles?.map((r) => (
                    <tr key={r.role}>
                      <td>{r.role}</td>
                      <td>{r.name}</td>
                      <td className="text-muted">{r.description}</td>
                      <td>
                        {r.role < 100 && (
                          <Form.Check
                            type="switch"
                            id={`role-${r.role}-second-factor`}
                            aria-label={t("Require two-factor authentication for {name}", { name: r.name })}
                            checked={r.require_second_factor}
                            disabled={busy}
                            onChange={(e) => void requireSecondFactor(r.role, e.target.checked)}
                          />
                        )}
                      </td>
                      <td className="text-end">
                        {r.builtin ? (
                          <span className="text-muted small"><T text="built in" /></span>
                        ) : (
                          <Button
                            size="sm"
                            variant="outline-danger"
                            disabled={busy}
                            onClick={() => void remove(r.role)}
                          >
                            <T text="Delete" />
                          </Button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            </div>
            <p className="text-muted small mt-3">
              <T text="Lower numbers are more privileged: 1 is the administrator, 100 is anyone at all. A table’s access rules name the least-privileged role still allowed." />
            </p>
          </Col>
          <Col lg={5} className="mb-4">
            <Card>
              <Card.Header><T text="Add role" /></Card.Header>
              <Card.Body>
                <Form onSubmit={create}>
                  <Form.Group className="mb-2" controlId="roleNumber">
                    <Form.Label><T text="Number" /></Form.Label>
                    <Form.Control
                      type="number"
                      min={1}
                      max={100}
                      value={number}
                      onChange={(e) => setNumber(Number(e.target.value))}
                    />
                    <Form.Text muted><T text="Between 1 and 100, and not already taken." /></Form.Text>
                  </Form.Group>
                  <Form.Group className="mb-2" controlId="roleName">
                    <Form.Label><T text="Name" /></Form.Label>
                    <Form.Control value={name} onChange={(e) => setName(e.target.value)} />
                  </Form.Group>
                  <Form.Group className="mb-3" controlId="roleDescription">
                    <Form.Label><T text="Description" /></Form.Label>
                    <Form.Control
                      value={description}
                      onChange={(e) => setDescription(e.target.value)}
                    />
                  </Form.Group>
                  <Button type="submit" disabled={busy || !name.trim()}>
                    <T text="Create role" />
                  </Button>
                </Form>
              </Card.Body>
            </Card>
          </Col>
        </Row>
      </PageBody>
    </>
  );
}
