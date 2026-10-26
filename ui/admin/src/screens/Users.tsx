// Users screen: the accounts, the form that makes and edits one, and the things
// an admin does to an account that are not edits.
//
// Three decisions shape it.
//
// **The form is the users table.** Per design §7.1 the users table is the one
// table an admin is invited to add columns to, so a fixed email/password/role
// form is wrong the moment they do: the fields come from `listFields("users")`
// minus the columns the system owns (`userForm.ts`), and a column added on the
// table page is a box on this form without anything here changing.
//
// **The form is a dialog, like a field's.** It is opened from a row or from "Add
// user" and closes when the save lands, so the list stays the page and the edit
// is the interruption — the same shape the fields list on the table page has.
//
// **What is not an edit is in the row's menu.** Disabling, forcing a logout,
// becoming somebody, resetting a password: each does something a column write
// cannot, so each is an action with its own confirmation rather than a checkbox
// in the form. A reset (and a create with a blank password) ends in the one
// dialog that shows a password in plain text — the only moment it is readable —
// with a copy button, because it is about to be pasted into a message.
//
// **Role grants are beside the role, not in it** (TODO-payments P3.2). The form's
// role is the base role. A subscription, a trial or an admin's comp is a grant
// that lasts a while; the row's menu opens them, and the role column says when
// somebody is served above their base role because of one.
//
// **LLM credits are beside it too** (TODO-payments P6.2): the balance a metered
// agent's model calls are charged to. The row's menu opens the balance, the
// buckets and the latest entries, with a form to grant credits by hand and the
// low-balance threshold an application's auto top-up listens for.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type {
  GetUserCreditsResponse,
  ListFieldsResponse,
  ListUserRoleGrantsResponse,
  ListUsersResponse,
} from "../client";
import {
  bucketSourceLabel,
  bucketState,
  bucketStateBadge,
  creditGrantBody,
  entryLabel,
  entryTokens,
  inDebt,
  newCreditGrantForm,
  thresholdBody,
  type CreditGrantForm,
} from "../credits";
import { navigate } from "../App";
import { IconDots, IconPlus } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { isMultilingual, localeLabel, localeOptions, useLocales, type Locales } from "../locales";
import { OptionalRoleSelect, RoleSelect } from "../roleSelect";
import { roleLabel, useRoles } from "../roles";
import {
  grantBody,
  grantSourceLabel,
  grantState,
  grantStateBadge,
  newGrantForm,
  servedAbove,
  type GrantForm,
} from "../roleGrants";
import {
  adminUserFields,
  credentialsText,
  displayValue,
  editUserForm,
  loginUrl,
  newUserForm,
  userBody,
  type Credentials,
  type UserForm,
  type UserRow,
} from "../userForm";
import { T, useT } from "../i18n";

/** Which user the dialog is editing, or `"new"` for one that does not exist yet. */
type Editing = { mode: "new" } | { mode: "edit"; user: UserRow };

export function Users() {
  const { t } = useT();
  const [users, setUsers] = useState<ListUsersResponse | null>(null);
  const [fields, setFields] = useState<ListFieldsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [credentials, setCredentials] = useState<Credentials | null>(null);
  const [granting, setGranting] = useState<UserRow | null>(null);
  const [crediting, setCrediting] = useState<UserRow | null>(null);
  const [busy, setBusy] = useState(false);
  const roles = useRoles();
  const locales = useLocales();

  const load = async () => {
    try {
      const [list, columns] = await Promise.all([api.listUsers(), api.listFields("users")]);
      setUsers(list);
      setFields(columns);
    } catch {
      setError("Could not load users.");
    }
  };

  useEffect(() => {
    void load();
  }, []);

  /** Run one row action, refresh the list, and surface the server's own refusal. */
  const act = async (fallback: string, action: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
      await load();
    } catch (err) {
      setError(errorMessage(err, fallback));
    } finally {
      setBusy(false);
    }
  };

  const remove = (user: UserRow) => {
    if (
      !window.confirm(
        t("Delete {address}? This cannot be undone.", { address: user.email }),
      )
    ) {
      return;
    }
    void act("Could not delete the user.", () => api.deleteUser(user.id));
  };

  const setDisabled = (user: UserRow, disabled: boolean) =>
    void act(
      disabled ? "Could not disable the user." : "Could not enable the user.",
      () => api.setUserDisabled(user.id, { disabled }),
    );

  const forceLogout = (user: UserRow) =>
    void act("Could not end the sessions.", () => api.forceLogoutUser(user.id));

  const resetPassword = (user: UserRow) => {
    if (
      !window.confirm(
        t("Replace {address}’s password with a random one?", {
          address: user.email,
        }),
      )
    ) {
      return;
    }
    void act("Could not set a new password.", async () => {
      const reset = await api.setRandomPassword(user.id);
      setCredentials({ email: reset.email, password: reset.password, url: loginUrl() });
    });
  };

  /**
   * Become a user: the session this page is being served under is swapped for
   * theirs, so there is nothing to refresh — the admin UI is no longer this
   * caller's to see. Reloading is what makes that plain rather than leaving a
   * screen full of buttons that now all fail.
   */
  const become = (user: UserRow) => {
    if (
      !window.confirm(
        t(
          "Continue as {address}? Your admin session ends — you will have to sign in again.",
          { address: user.email },
        ),
      )
    ) {
      return;
    }
    void act("Could not become that user.", async () => {
      await api.becomeUser(user.id);
      window.location.reload();
    });
  };

  const columns = adminUserFields(fields);

  return (
    <>
      <PageHeader
        pretitle="Access"
        title={t("Users")}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/roles")}>
              <T text="Roles" />
            </Button>
            <Button onClick={() => setEditing({ mode: "new" })}>
              <IconPlus className="icon-2" />
              <T text="Add user" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}

        <Card>
          <Card.Header>
            <h3 className="card-title"><T text="Users" /></h3>
          </Card.Header>
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Email" /></th>
                <th><T text="Role" /></th>
                <th><T text="Status" /></th>
                {columns.map((f) => (
                  <th key={f.name}>{f.label || f.name}</th>
                ))}
                <th className="w-1" />
              </tr>
            </thead>
            <tbody>
              {users?.length === 0 && (
                <tr>
                  <td colSpan={4 + columns.length} className="text-muted">
                    <T text="No users yet." />
                  </td>
                </tr>
              )}
              {users?.map((u) => {
                const bag = (u.extra ?? {}) as Record<string, unknown>;
                return (
                  <tr key={u.id}>
                    <td>{u.email}</td>
                    <td>{roleLabel(u.role, roles)}</td>
                    <td>
                      {u.disabled ? (
                        <StatusBadge tone="red"><T text="Disabled" /></StatusBadge>
                      ) : (
                        <StatusBadge tone="green"><T text="Active" /></StatusBadge>
                      )}
                    </td>
                    {columns.map((f) => (
                      <td key={f.name}>{displayValue(bag[f.name])}</td>
                    ))}
                    <td>
                      <div className="btn-list justify-content-end flex-nowrap">
                        <Button
                          size="sm"
                          variant="outline-secondary"
                          disabled={busy}
                          onClick={() => setEditing({ mode: "edit", user: u })}
                        >
                          <T text="Edit" />
                        </Button>
                        <Dropdown align="end">
                          <Dropdown.Toggle
                            variant="outline-secondary"
                            size="sm"
                            className="btn-icon"
                            id={`user-menu-${u.id}`}
                            disabled={busy}
                            aria-label={`More actions for ${u.email}`}
                          >
                            <IconDots className="icon-2" />
                          </Dropdown.Toggle>
                          {/* Positioned against the viewport, not the table: the
                              rows table scrolls sideways when the admin has added
                              columns, and a menu laid out inside that box is
                              clipped by it. */}
                          <Dropdown.Menu renderOnMount popperConfig={{ strategy: "fixed" }}>
                            <Dropdown.Item onClick={() => resetPassword(u)}>
                              <T text="Set random password" />
                            </Dropdown.Item>
                            <Dropdown.Item onClick={() => forceLogout(u)}>
                              <T text="Force logout" />
                            </Dropdown.Item>
                            <Dropdown.Item onClick={() => become(u)}><T text="Become user" /></Dropdown.Item>
                            <Dropdown.Item onClick={() => setGranting(u)}>
                              <T text="Role grants" />
                            </Dropdown.Item>
                            <Dropdown.Item onClick={() => setCrediting(u)}>
                              <T text="LLM credits" />
                            </Dropdown.Item>
                            <Dropdown.Divider />
                            <Dropdown.Item onClick={() => setDisabled(u, !u.disabled)}>
                              {u.disabled ? "Enable user" : "Disable user"}
                            </Dropdown.Item>
                            <Dropdown.Item className="text-danger" onClick={() => remove(u)}>
                              <T text="Delete user" />
                            </Dropdown.Item>
                          </Dropdown.Menu>
                        </Dropdown>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        </Card>

        <UserDialog
          editing={editing}
          fields={fields}
          roles={roles}
          locales={locales}
          onClose={() => setEditing(null)}
          onSaved={(created) => {
            setEditing(null);
            if (created) setCredentials(created);
            void load();
          }}
        />

        <CredentialsDialog
          credentials={credentials}
          onClose={() => setCredentials(null)}
        />

        <RoleGrantsDialog user={granting} roles={roles} onClose={() => setGranting(null)} />

        <CreditsDialog user={crediting} onClose={() => setCrediting(null)} />
      </PageBody>
    </>
  );
}

/**
 * The add/edit dialog: email, password, role, language, and one input per admin
 * field.
 *
 * The language select is only rendered on an installation that serves more than
 * one (§16.1, D11) — a control with one option is a control that asks a question
 * with one answer.
 */
function UserDialog({
  editing,
  fields,
  roles,
  locales,
  onClose,
  onSaved,
}: {
  editing: Editing | null;
  fields: ListFieldsResponse | null;
  roles: ReturnType<typeof useRoles>;
  locales: Locales;
  onClose: () => void;
  onSaved: (credentials: Credentials | null) => void;
}) {
  const { t } = useT();
  const [form, setForm] = useState<UserForm>(() => newUserForm(roles));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Reset whenever the dialog opens: on the user being edited, or blank (on the
  // default role) for a new one. Keyed on the dialog opening rather than on
  // every render, so typing is not fought.
  useEffect(() => {
    if (!editing) return;
    setError(null);
    setForm(
      editing.mode === "edit" ? editUserForm(editing.user, fields) : newUserForm(roles),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editing]);

  const isEdit = editing?.mode === "edit";
  const columns = adminUserFields(fields);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!editing) return;
    setBusy(true);
    setError(null);
    try {
      if (editing.mode === "edit") {
        await api.updateUser(editing.user.id, userBody(form));
        onSaved(null);
      } else {
        const created = await api.createUser(userBody(form));
        // A blank password was a request for a generated one, and this response
        // is the only place it is ever readable.
        onSaved(
          created.generated_password
            ? {
                email: created.user.email,
                password: created.generated_password,
                url: loginUrl(),
              }
            : null,
        );
      }
    } catch (err) {
      setError(
        errorMessage(err, isEdit ? "Could not save the user." : "Could not create the user."),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal show={editing !== null} onHide={onClose} scrollable>
      {/* The form wraps the whole modal so the footer's button is its submit and
          Return in a text box does what the button does. */}
      <Form onSubmit={save}>
        <Modal.Header closeButton>
          <Modal.Title className="h4">
            {editing?.mode === "edit" ? `Edit ${editing.user.email}` : "Add user"}
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}

          <Form.Group className="mb-3" controlId="userEmail">
            <Form.Label><T text="Email" /></Form.Label>
            <Form.Control
              type="email"
              value={form.email}
              autoComplete="off"
              autoFocus
              required
              onChange={(e) => setForm({ ...form, email: e.target.value })}
            />
          </Form.Group>

          <Form.Group className="mb-3" controlId="userPassword">
            <Form.Label><T text="Password" /></Form.Label>
            <Form.Control
              type="password"
              value={form.password}
              autoComplete="new-password"
              placeholder={isEdit ? "Unchanged" : "Generated"}
              onChange={(e) => setForm({ ...form, password: e.target.value })}
            />
            <Form.Text muted>
              {isEdit
                ? "Leave blank to keep the current password."
                : "Leave blank and a random password is generated, shown once for you to pass on."}
            </Form.Text>
          </Form.Group>

          <RoleSelect
            id="userRole"
            label={t("Role")}
            roles={roles}
            value={form.role}
            onChange={(role) => setForm({ ...form, role })}
          >
            <T text="Lower is more privileged. Add roles on the" /> <a href="#/roles"><T text="Roles" /></a> screen.
          </RoleSelect>

          {isMultilingual(locales) && (
            <Form.Group className="mb-3" controlId="userLanguage">
              <Form.Label><T text="Language" /></Form.Label>
              <Form.Select
                value={form.language}
                onChange={(e) => setForm({ ...form, language: e.target.value })}
              >
                <option value="">
                  {t("Site default ({language})", {
                    language: localeLabel(locales.default),
                  })}
                </option>
                {localeOptions(form.language || null, locales).map((tag) => (
                  <option key={tag} value={tag}>
                    {localeLabel(tag)}
                  </option>
                ))}
              </Form.Select>
              <Form.Text muted>
                <T text="What this account reads the product in. They can change it themselves." />
              </Form.Text>
            </Form.Group>
          )}

          {columns.map((f) => (
            <Form.Group className="mb-3" controlId={`user-${f.name}`} key={f.name}>
              <Form.Label>{f.label || f.name}</Form.Label>
              <Form.Control
                value={form.extra[f.name] ?? ""}
                onChange={(e) =>
                  setForm({ ...form, extra: { ...form.extra, [f.name]: e.target.value } })
                }
              />
              {f.description && <Form.Text muted>{f.description}</Form.Text>}
            </Form.Group>
          ))}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" type="button" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={busy}>
            {isEdit ? "Save changes" : "Create user"}
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}

/**
 * A user's role grants: the base role beside the role they are served at, every
 * grant they have had, and a form to give one by hand.
 *
 * Revoking a subscription's grant is allowed — an admin may need to stop access
 * now — but the provider's next renewal puts it back, which the confirmation
 * says.
 */
function RoleGrantsDialog({
  user,
  roles,
  onClose,
}: {
  user: UserRow | null;
  roles: ReturnType<typeof useRoles>;
  onClose: () => void;
}) {
  const { t } = useT();
  const [grants, setGrants] = useState<ListUserRoleGrantsResponse | null>(null);
  const [form, setForm] = useState<GrantForm>(newGrantForm);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async (id: string) => {
    try {
      setGrants(await api.listUserRoleGrants(id));
    } catch (err) {
      setError(errorMessage(err, "Could not load the role grants."));
    }
  };

  useEffect(() => {
    setGrants(null);
    setError(null);
    setForm(newGrantForm());
    if (user) void load(user.id);
  }, [user]);

  const grant = async (e: FormEvent) => {
    e.preventDefault();
    if (!user) return;
    const body = grantBody(form);
    if (typeof body === "string") {
      setError(body);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.grantUserRole(user.id, body);
      setForm(newGrantForm());
      await load(user.id);
    } catch (err) {
      setError(errorMessage(err, "Could not grant the role."));
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (grantId: string, source: string) => {
    if (!user) return;
    const warning =
      source === "admin"
        ? t("Revoke this grant? It stops counting at once.")
        : t(
            "Revoke this grant? It stops counting at once, but {source} may grant it again — a subscription's next renewal does.",
            { source },
          );
    if (!window.confirm(warning)) return;
    setBusy(true);
    setError(null);
    try {
      await api.revokeUserRoleGrant(user.id, grantId);
      await load(user.id);
    } catch (err) {
      setError(errorMessage(err, "Could not revoke the grant."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal show={user !== null} onHide={onClose} size="lg" scrollable>
      <Modal.Header closeButton>
        <Modal.Title className="h4">
          {t("Role grants for {address}", { address: user?.email ?? "" })}
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {grants && (
          <p>
            {t("Base role: {base}.", { base: roleLabel(grants.base_role, roles) })}{" "}
            {servedAbove(grants)
              ? t("Served as {effective} through a grant in force.", {
                  effective: roleLabel(grants.effective_role, roles),
                })
              : t("No grant in force raises it.")}
          </p>
        )}
        <Table responsive className="table-vcenter">
          <thead>
            <tr>
              <th><T text="Role" /></th>
              <th><T text="Source" /></th>
              <th><T text="From" /></th>
              <th><T text="Until" /></th>
              <th><T text="Status" /></th>
              <th className="w-1" />
            </tr>
          </thead>
          <tbody>
            {grants?.grants.length === 0 && (
              <tr>
                <td colSpan={6} className="text-muted">
                  <T text="No grants yet." />
                </td>
              </tr>
            )}
            {grants?.grants.map((g) => {
              const state = grantState(g);
              const badge = grantStateBadge(state, t);
              return (
                <tr key={g.id}>
                  <td>{roleLabel(g.role, roles)}</td>
                  <td>
                    {grantSourceLabel(g)}
                    {g.note && <div className="text-muted small">{g.note}</div>}
                  </td>
                  <td>{new Date(g.starts_at).toLocaleString()}</td>
                  <td>
                    {g.expires_at ? new Date(g.expires_at).toLocaleString() : t("Until revoked")}
                  </td>
                  <td>
                    <StatusBadge tone={badge.tone}>{badge.label}</StatusBadge>
                  </td>
                  <td>
                    {(state === "in_force" || state === "pending") && (
                      <Button
                        size="sm"
                        variant="outline-danger"
                        disabled={busy}
                        onClick={() => void revoke(g.id, g.source)}
                      >
                        <T text="Revoke" />
                      </Button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </Table>

        <Form onSubmit={grant} className="border-top pt-3">
          <h4><T text="Grant a role" /></h4>
          <OptionalRoleSelect
            id="grantRole"
            label={t("Role")}
            blank={t("Choose a role")}
            roles={(roles ?? []).filter((r) => r.role < 100)}
            value={form.role}
            onChange={(role) => setForm({ ...form, role })}
          />
          <Form.Group className="mb-3" controlId="grantDays">
            <Form.Label><T text="Days" /></Form.Label>
            <Form.Control
              inputMode="numeric"
              value={form.days}
              onChange={(e) => setForm({ ...form, days: e.target.value })}
            />
            <Form.Text muted>
              <T text="How long it lasts from now. Blank lasts until revoked." />
            </Form.Text>
          </Form.Group>
          <Form.Group className="mb-3" controlId="grantNote">
            <Form.Label><T text="Note" /></Form.Label>
            <Form.Control
              value={form.note}
              placeholder={t("Why, for whoever reads this later")}
              onChange={(e) => setForm({ ...form, note: e.target.value })}
            />
          </Form.Group>
          <Button type="submit" disabled={busy}>
            <T text="Grant" />
          </Button>
        </Form>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onClose}>
          <T text="Close" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

/**
 * A user's LLM credits: the balance (and any debt), every bucket, the latest
 * entries, a form to grant credits by hand, and the low-balance threshold.
 *
 * Every answer from the server is the whole view, so the dialog redraws from
 * what the server now holds after each change.
 */
function CreditsDialog({ user, onClose }: { user: UserRow | null; onClose: () => void }) {
  const { t } = useT();
  const [credits, setCredits] = useState<GetUserCreditsResponse | null>(null);
  const [form, setForm] = useState<CreditGrantForm>(newCreditGrantForm);
  const [threshold, setThreshold] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const show = (view: GetUserCreditsResponse) => {
    setCredits(view);
    setThreshold(view.low_balance_threshold ?? "");
  };

  useEffect(() => {
    setCredits(null);
    setError(null);
    setForm(newCreditGrantForm());
    setThreshold("");
    if (!user) return;
    api
      .getUserCredits(user.id)
      .then(show)
      .catch((err) => setError(errorMessage(err, "Could not load the credits.")));
  }, [user]);

  const run = async (change: () => Promise<GetUserCreditsResponse>, failure: string) => {
    setBusy(true);
    setError(null);
    try {
      show(await change());
      return true;
    } catch (err) {
      setError(errorMessage(err, failure));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const grant = async (e: FormEvent) => {
    e.preventDefault();
    if (!user) return;
    const body = creditGrantBody(form, t);
    if (typeof body === "string") {
      setError(body);
      return;
    }
    if (await run(() => api.grantUserCredits(user.id, body), "Could not grant the credits.")) {
      setForm(newCreditGrantForm());
    }
  };

  const saveThreshold = async (e: FormEvent) => {
    e.preventDefault();
    if (!user) return;
    const body = thresholdBody(threshold, t);
    if (typeof body === "string") {
      setError(body);
      return;
    }
    await run(() => api.setUserCreditThreshold(user.id, body), "Could not set the threshold.");
  };

  return (
    <Modal show={user !== null} onHide={onClose} size="lg" scrollable>
      <Modal.Header closeButton>
        <Modal.Title className="h4">
          {t("LLM credits for {address}", { address: user?.email ?? "" })}
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {credits && (
          <p>
            {t("Balance: {balance} credits.", { balance: credits.balance })}{" "}
            {inDebt(credits)
              ? t("That includes a debt of {debt}, which the next grant repays first.", {
                  debt: credits.debt,
                })
              : credits.next_expiry
                ? t("Some expire on {date}.", {
                    date: new Date(credits.next_expiry).toLocaleString(),
                  })
                : null}
          </p>
        )}

        <h4><T text="Buckets" /></h4>
        <Table responsive className="table-vcenter">
          <thead>
            <tr>
              <th><T text="Left" /></th>
              <th><T text="Granted" /></th>
              <th><T text="Source" /></th>
              <th><T text="Expires" /></th>
              <th><T text="Status" /></th>
            </tr>
          </thead>
          <tbody>
            {credits?.buckets.length === 0 && (
              <tr>
                <td colSpan={5} className="text-muted">
                  <T text="No credits granted yet." />
                </td>
              </tr>
            )}
            {credits?.buckets.map((b) => {
              const badge = bucketStateBadge(bucketState(b), t);
              return (
                <tr key={b.id}>
                  <td>{b.remaining}</td>
                  <td>
                    {b.amount}
                    <div className="text-muted small">{b.kind}</div>
                  </td>
                  <td>
                    {bucketSourceLabel(b, t)}
                    {b.note && <div className="text-muted small">{b.note}</div>}
                  </td>
                  <td>{b.expires_at ? new Date(b.expires_at).toLocaleString() : t("Never")}</td>
                  <td>
                    <StatusBadge tone={badge.tone}>{badge.label}</StatusBadge>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </Table>

        <h4><T text="Latest entries" /></h4>
        <Table responsive className="table-vcenter">
          <thead>
            <tr>
              <th><T text="When" /></th>
              <th><T text="What" /></th>
              <th><T text="Change" /></th>
              <th><T text="Balance after" /></th>
            </tr>
          </thead>
          <tbody>
            {credits?.entries.length === 0 && (
              <tr>
                <td colSpan={4} className="text-muted">
                  <T text="Nothing yet." />
                </td>
              </tr>
            )}
            {credits?.entries.map((e) => (
              <tr key={e.id}>
                <td>{new Date(e.created_at).toLocaleString()}</td>
                <td>
                  {entryLabel(e, t)}
                  {entryTokens(e) && <div className="text-muted small">{entryTokens(e)}</div>}
                  {e.note && <div className="text-muted small">{e.note}</div>}
                </td>
                <td>{e.amount}</td>
                <td>{e.balance}</td>
              </tr>
            ))}
          </tbody>
        </Table>

        <Form onSubmit={grant} className="border-top pt-3">
          <h4><T text="Grant credits" /></h4>
          <Form.Group className="mb-3" controlId="creditAmount">
            <Form.Label><T text="Amount" /></Form.Label>
            <Form.Control
              inputMode="decimal"
              value={form.amount}
              onChange={(e) => setForm({ ...form, amount: e.target.value })}
            />
          </Form.Group>
          <Form.Group className="mb-3" controlId="creditDays">
            <Form.Label><T text="Days" /></Form.Label>
            <Form.Control
              inputMode="numeric"
              value={form.days}
              onChange={(e) => setForm({ ...form, days: e.target.value })}
            />
            <Form.Text muted>
              <T text="How long they last from now. Blank never expires." />
            </Form.Text>
          </Form.Group>
          <Form.Group className="mb-3" controlId="creditNote">
            <Form.Label><T text="Note" /></Form.Label>
            <Form.Control
              value={form.note}
              placeholder={t("Why, for whoever reads this later")}
              onChange={(e) => setForm({ ...form, note: e.target.value })}
            />
          </Form.Group>
          <Button type="submit" disabled={busy}>
            <T text="Grant" />
          </Button>
        </Form>

        <Form onSubmit={saveThreshold} className="border-top pt-3 mt-3">
          <h4><T text="Low-balance threshold" /></h4>
          <Form.Group className="mb-3" controlId="creditThreshold">
            <Form.Control
              inputMode="decimal"
              value={threshold}
              onChange={(e) => setThreshold(e.target.value)}
            />
            <Form.Text muted>
              <T text="When an agent's call takes the balance below this, a low_balance entry is written for the application's triggers (an automatic top-up, say). Blank for none." />
            </Form.Text>
          </Form.Group>
          <Button type="submit" variant="outline-primary" disabled={busy}>
            <T text="Save threshold" />
          </Button>
        </Form>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onClose}>
          <T text="Close" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

/**
 * The one dialog that shows a password in plain text.
 *
 * It exists because the password is only readable in the response that made it:
 * it is stored as an argon2 hash, so if this dialog is closed without the
 * password being copied, nobody — including the admin who just created it — can
 * ever read it again, and the only remedy is another reset. Hence the monospace
 * block (so an `l` and a `1` are told apart), the copy button (so it is
 * transcribed exactly), and the sentence saying it will not be shown again.
 */
function CredentialsDialog({
  credentials,
  onClose,
}: {
  credentials: Credentials | null;
  onClose: () => void;
}) {
  const { t } = useT();
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    setCopied(false);
  }, [credentials]);

  const text = credentials ? credentialsText(credentials) : "";

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
    } catch {
      // A clipboard the browser will not hand over is not an error worth a
      // banner: the text is on screen and selectable, which is the fallback.
      setCopied(false);
    }
  };

  return (
    <Modal show={credentials !== null} onHide={onClose}>
      <Modal.Header closeButton>
        <Modal.Title className="h4"><T text="Password set" /></Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <p>
          {t(
            "Send these to {address} over a channel you trust. The password is not stored in a readable form and will not be shown again.",
            { address: credentials?.email ?? "" },
          )}
        </p>
        <pre className="border rounded p-3 mb-3 user-credentials">{text}</pre>
        <Button variant="outline-secondary" onClick={() => void copy()}>
          {copied ? "Copied" : "Copy"}
        </Button>
      </Modal.Body>
      <Modal.Footer>
        <Button onClick={onClose}><T text="Done" /></Button>
      </Modal.Footer>
    </Modal>
  );
}
