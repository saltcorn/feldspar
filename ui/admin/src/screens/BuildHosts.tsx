// Settings → Build hosts: the machines that build an application's targets for
// this server, such as a Mac for iOS while the server runs on Linux
// (`TODO-build-hosts.md`).
//
// - **The token is shown once**, when a host is added, with the command that
//   starts the agent. The table keeps only a hash, so a lost token means
//   removing the host and adding it again.
// - **The token goes in a file on the build host**, never on its command line,
//   where `ps` and the shell's history would show it.
// - **Removing a host is how its token is revoked**: its agent is refused at
//   the next heartbeat. Editing one changes its name and targets, and keeps
//   its token.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import {
  type BuildHost,
  TOKEN_FILE,
  agentCommand,
  agentSummary,
  hostBadge,
  hostProblem,
  hostReadiness,
  refusal,
  targetChoices,
} from "../buildHosts";
import { AlertBody } from "../layout";
import { when } from "../mcpTokens";
import { T, useT } from "../i18n";

export function BuildHostsTab() {
  const { t } = useT();
  const [hosts, setHosts] = useState<BuildHost[] | null>(null);
  const [choices, setChoices] = useState<Array<{ name: string; label: string }>>([]);
  const [name, setName] = useState("");
  const [targets, setTargets] = useState<string[]>([]);
  const [token, setToken] = useState<string | null>(null);
  // The host being changed, when the form is editing one rather than adding.
  const [editing, setEditing] = useState<BuildHost | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = async () => setHosts(await api.listBuildHosts());

  useEffect(() => {
    void (async () => {
      try {
        setChoices(targetChoices(await api.listFrameworks()));
        await reload();
      } catch (e) {
        setError(e instanceof Error ? e.message : "Could not load the build hosts.");
      }
    })();
  }, []);

  const problem = hostProblem(name, targets);

  const clear = () => {
    setEditing(null);
    setName("");
    setTargets([]);
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const body = { name: name.trim(), targets };
      if (editing) {
        await api.updateBuildHost(editing.id, body);
      } else {
        setToken((await api.createBuildHost(body)).token);
      }
      clear();
      await reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the build host.");
    } finally {
      setBusy(false);
    }
  };

  const edit = (host: BuildHost) => {
    setEditing(host);
    setName(host.name);
    setTargets(host.targets);
  };

  const remove = async (host: BuildHost) => {
    setError(null);
    try {
      await api.deleteBuildHost(host.id);
      await reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the build host.");
    }
  };

  const origin = typeof window === "undefined" ? "" : window.location.origin;
  const toggle = (target: string, on: boolean) =>
    setTargets((current) =>
      on ? [...current.filter((t) => t !== target), target] : current.filter((t) => t !== target),
    );

  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title"><T text="Build hosts" /></h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="Other machines that build an application's native apps for this server, such as a Mac for iOS. A build host asks this server for work, so it needs no open port." />
          </p>
        </div>
      </div>
      <div className="card-body">
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}

        {token && (
          <Alert variant="success">
            <AlertBody>
              <p className="mb-2">
                <strong><T text="This is the only time this token is shown." /></strong>{" "}
                <T
                  text="On the build host, save it in {file}, readable only by you, then start the agent:"
                  values={{ file: <code>{TOKEN_FILE}</code> }}
                />
              </p>
              <pre className="border rounded p-3 mb-2 text-wrap">{token}</pre>
              <pre className="border rounded p-3 mb-2 text-wrap">{agentCommand(origin)}</pre>
              <Button variant="outline-secondary" size="sm" onClick={() => setToken(null)}>
                <T text="Done" />
              </Button>
            </AlertBody>
          </Alert>
        )}

        {!hosts ? (
          !error && <Spinner animation="border" role="status" size="sm" />
        ) : (
          <>
            <form onSubmit={(e) => void save(e)}>
              <Form.Group className="mb-3" controlId="build-host-name">
                <Form.Label><T text="Name" /></Form.Label>
                <Form.Control
                  type="text"
                  value={name}
                  placeholder={t("Mac mini")}
                  onChange={(e) => setName(e.target.value)}
                />
              </Form.Group>
              <fieldset className="mb-3">
                <legend className="form-label"><T text="It builds" /></legend>
                {choices.length === 0 && (
                  <p className="text-secondary mb-0">
                    <T text="No installed framework declares a build target." />
                  </p>
                )}
                {choices.map((choice) => (
                  <Form.Check
                    key={choice.name}
                    type="checkbox"
                    id={`build-host-target-${choice.name}`}
                    label={choice.label}
                    checked={targets.includes(choice.name)}
                    onChange={(e) => toggle(choice.name, e.target.checked)}
                  />
                ))}
              </fieldset>
              <div className="btn-list">
                <Button type="submit" variant="outline-primary" disabled={busy || !!problem}>
                  {editing ? t("Save changes") : t("Add build host")}
                </Button>
                {editing && (
                  <Button variant="outline-secondary" onClick={clear}>
                    <T text="Cancel" />
                  </Button>
                )}
              </div>
            </form>

            <h4 className="mt-4 mb-2"><T text="Hosts" /></h4>
            {hosts.length === 0 ? (
              <p className="text-secondary mb-0"><T text="No build hosts yet." /></p>
            ) : (
              <div className="table-responsive">
                <table className="table table-vcenter">
                  <thead>
                    <tr>
                      <th><T text="Name" /></th>
                      <th><T text="Targets" /></th>
                      <th><T text="Last seen" /></th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {hosts.map((host) => {
                      const badge = hostBadge(t, host);
                      const turnedAway = refusal(host);
                      return (
                        <tr key={host.id}>
                          <td>
                            {host.name}{" "}
                            <span className={`badge ${badge.tone} ms-1`}>{badge.label}</span>
                            {agentSummary(host) && (
                              <div className="text-secondary small">{agentSummary(host)}</div>
                            )}
                            {turnedAway && <div className="text-danger small">{turnedAway}</div>}
                          </td>
                          <td>
                            {hostReadiness(host).map(({ target, missing }) => (
                              <div key={target}>
                                <code>{target}</code>{" "}
                                {missing === null ? (
                                  <span className="text-secondary small"><T text="not reported yet" /></span>
                                ) : missing.length === 0 ? (
                                  <span className="badge bg-green-lt"><T text="ready" /></span>
                                ) : (
                                  <ul className="small text-danger mb-1">
                                    {missing.map((m) => (
                                      <li key={m}>{m}</li>
                                    ))}
                                  </ul>
                                )}
                              </div>
                            ))}
                          </td>
                          <td>{when(host.last_seen_at)}</td>
                          <td className="text-end">
                            <div className="btn-list justify-content-end">
                              <Button variant="outline-secondary" size="sm" onClick={() => edit(host)}>
                                <T text="Edit" />
                              </Button>
                              <Button variant="outline-danger" size="sm" onClick={() => void remove(host)}>
                                <T text="Remove" />
                              </Button>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
