// Two-factor authentication in the admin UI (design §7.2).
//
// Two places use it:
//
// - **Sign-in.** A password that is right for an account with two-factor
//   authentication on answers with `second_factor: {token, setup}` and no
//   session. `SecondFactorSignIn` asks for the code (or, when the account's
//   role requires a second factor it has not set up, sets one up first) and
//   finishes the sign-in with the token.
// - **Account security** (`#/account/security`, from the button beside "Log
//   out"): the signed-in admin turns it on, off, or makes new recovery codes.
//
// Both talk to the server's `/_feldspar/second-factor` routes through the
// generated client's `secondFactor*` methods.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage, errorStatus } from "../api";
import type { SecondFactorSetup, SecondFactorStatus } from "../client";
import { CenteredPage, PageBody, PageHeader } from "../layout";
import { T, useT } from "../i18n";
import { codeError, normaliseCode } from "../secondFactor";

/** The QR code an authenticator app scans, and the key to type in instead. */
function SetupDetails({ setup }: { setup: SecondFactorSetup }) {
  return (
    <>
      <p className="text-secondary">
        <T text="Scan this with an authenticator app (Google Authenticator, Microsoft Authenticator, 1Password, …), then enter the code it shows." />
      </p>
      {/* The server draws the QR code (black on white, which cameras read
          best); it is an SVG document made from the setup's own URL. */}
      <div
        className="second-factor-qr mb-3"
        dangerouslySetInnerHTML={{ __html: setup.qr_svg }}
      />
      <p className="small text-secondary mb-3">
        <T text="Or enter this key by hand:" /> <code className="user-select-all">{setup.secret}</code>
      </p>
    </>
  );
}

/** The recovery codes, shown the one time they exist in plain text. */
export function RecoveryCodes({ codes }: { codes: string[] }) {
  return (
    <>
      <Alert variant="warning">
        <T text="Keep these recovery codes somewhere safe. Each one signs you in once if you lose your phone. They are not shown again." />
      </Alert>
      <ul className="second-factor-codes list-unstyled font-monospace mb-3">
        {codes.map((code) => (
          <li key={code}>{code}</li>
        ))}
      </ul>
    </>
  );
}

/** One code field, the same wherever a code is asked for. */
function CodeField({
  value,
  onChange,
  allowRecovery,
}: {
  value: string;
  onChange: (value: string) => void;
  allowRecovery: boolean;
}) {
  const { t } = useT();
  return (
    <Form.Group className="mb-3" controlId="secondFactorCode">
      <Form.Label><T text="Code" /></Form.Label>
      <Form.Control
        value={value}
        autoFocus
        required
        autoComplete="one-time-code"
        inputMode={allowRecovery ? "text" : "numeric"}
        placeholder={allowRecovery ? t("123456 or a recovery code") : "123456"}
        onChange={(e) => onChange(e.target.value)}
      />
    </Form.Group>
  );
}

/**
 * The second half of signing in: a code for the challenge a password was
 * answered with, or — `setup` — setting a second factor up first.
 */
export function SecondFactorSignIn({
  challenge,
  onSignedIn,
  onCancel,
}: {
  challenge: { token: string; setup: boolean };
  onSignedIn: () => void;
  onCancel: () => void;
}) {
  const { t } = useT();
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [setup, setSetup] = useState<SecondFactorSetup | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);

  useEffect(() => {
    if (!challenge.setup) return;
    void (async () => {
      try {
        setSetup(await api.secondFactorSetup(challenge.token));
      } catch (err) {
        setError(codeError(errorStatus(err), t));
      }
    })();
  }, [challenge, t]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      if (challenge.setup) {
        const enabled = await api.secondFactorEnable(normaliseCode(code), challenge.token);
        setCodes(enabled.recovery_codes);
      } else {
        await api.secondFactorVerify(challenge.token, normaliseCode(code));
        onSignedIn();
      }
    } catch (err) {
      setError(codeError(errorStatus(err), t));
      setCode("");
    } finally {
      setBusy(false);
    }
  };

  return (
    <CenteredPage title={t("Two-factor authentication")}>
      <Card className="card-md">
        <Card.Body>
          <Card.Title as="h1" className="h3 text-center mb-4">
            {challenge.setup ? (
              <T text="Set up two-factor authentication" />
            ) : (
              <T text="Two-factor authentication" />
            )}
          </Card.Title>
          {error && <Alert variant="danger">{error}</Alert>}
          {codes ? (
            <>
              <RecoveryCodes codes={codes} />
              <Button className="w-100" onClick={onSignedIn}>
                <T text="Continue" />
              </Button>
            </>
          ) : (
            <Form onSubmit={submit}>
              {challenge.setup ? (
                <>
                  <p>
                    <T text="Your role requires a code from an authenticator app to sign in." />
                  </p>
                  {setup ? <SetupDetails setup={setup} /> : <Spinner animation="border" size="sm" />}
                </>
              ) : (
                <p className="text-secondary">
                  <T text="Enter the code from your authenticator app, or one of your recovery codes." />
                </p>
              )}
              <CodeField value={code} onChange={setCode} allowRecovery={!challenge.setup} />
              <div className="form-footer d-grid gap-2">
                <Button type="submit" disabled={busy || (challenge.setup && !setup)}>
                  {challenge.setup ? <T text="Turn on and sign in" /> : <T text="Sign in" />}
                </Button>
                <Button variant="link" onClick={onCancel}>
                  <T text="Back" />
                </Button>
              </div>
            </Form>
          )}
        </Card.Body>
      </Card>
    </CenteredPage>
  );
}

/** Account security: the signed-in admin's own two-factor authentication. */
export function AccountSecurity() {
  const { t } = useT();
  const [status, setStatus] = useState<SecondFactorStatus | null>(null);
  const [setup, setSetup] = useState<SecondFactorSetup | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [asking, setAsking] = useState(false);
  const [action, setAction] = useState<"disable" | "codes" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async () => {
    try {
      setStatus(await api.secondFactorStatus());
    } catch {
      setError(t("Could not load your two-factor authentication."));
    }
  };

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = async (work: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (err) {
      const status = errorStatus(err);
      setError(status === 400 ? errorMessage(err, codeError(status, t)) : codeError(status, t));
    } finally {
      setBusy(false);
    }
  };

  // Setting one up proves the password again: a second factor decides who
  // can sign in to the account.
  const start = (e: FormEvent) => {
    e.preventDefault();
    void run(async () => {
      setCodes(null);
      setCode("");
      setSetup(await api.secondFactorSetup({ password }));
      setPassword("");
      setAsking(false);
    });
  };

  const enable = (e: FormEvent) => {
    e.preventDefault();
    void run(async () => {
      const enabled = await api.secondFactorEnable(normaliseCode(code));
      setSetup(null);
      setCode("");
      setCodes(enabled.recovery_codes);
      await load();
    });
  };

  const confirm = (e: FormEvent) => {
    e.preventDefault();
    void run(async () => {
      if (action === "disable") {
        await api.secondFactorDisable(normaliseCode(code));
        setCodes(null);
      } else {
        setCodes((await api.secondFactorRecoveryCodes(normaliseCode(code))).recovery_codes);
      }
      setAction(null);
      setCode("");
      await load();
    });
  };

  return (
    <>
      <PageHeader pretitle={t("Account")} title={t("Two-factor authentication")} />
      <PageBody>
        <div className="row">
          <div className="col-lg-7">
            {error && <Alert variant="danger">{error}</Alert>}
            {codes && <RecoveryCodes codes={codes} />}
            <Card>
              <Card.Body>
                {!status ? (
                  <Spinner animation="border" size="sm" />
                ) : setup ? (
                  <Form onSubmit={enable}>
                    <SetupDetails setup={setup} />
                    <CodeField value={code} onChange={setCode} allowRecovery={false} />
                    <div className="d-flex gap-2">
                      <Button type="submit" disabled={busy}>
                        <T text="Turn on" />
                      </Button>
                      <Button variant="link" onClick={() => setSetup(null)}>
                        <T text="Cancel" />
                      </Button>
                    </div>
                  </Form>
                ) : status.enabled ? (
                  <>
                    <p>
                      <span className="badge bg-green-lt me-2"><T text="On" /></span>
                      <T text="A code from your authenticator app is asked for after your password." />
                    </p>
                    <p className="text-secondary small">
                      {t("{count} recovery codes left.", { count: status.recovery_codes_left })}
                      {status.required && (
                        <>
                          {" "}
                          <T text="Your role requires it, so it cannot be turned off." />
                        </>
                      )}
                    </p>
                    {action ? (
                      <Form onSubmit={confirm}>
                        <CodeField value={code} onChange={setCode} allowRecovery />
                        <div className="d-flex gap-2">
                          <Button
                            type="submit"
                            variant={action === "disable" ? "danger" : "primary"}
                            disabled={busy}
                          >
                            {action === "disable" ? (
                              <T text="Turn off" />
                            ) : (
                              <T text="Make new recovery codes" />
                            )}
                          </Button>
                          <Button variant="link" onClick={() => setAction(null)}>
                            <T text="Cancel" />
                          </Button>
                        </div>
                      </Form>
                    ) : (
                      <div className="d-flex gap-2 flex-wrap">
                        <Button variant="outline-primary" onClick={() => setAction("codes")}>
                          <T text="New recovery codes" />
                        </Button>
                        {!status.required && (
                          <Button variant="outline-danger" onClick={() => setAction("disable")}>
                            <T text="Turn off" />
                          </Button>
                        )}
                      </div>
                    )}
                  </>
                ) : (
                  <>
                    <p>
                      <span className="badge bg-secondary-lt me-2"><T text="Off" /></span>
                      <T text="Signing in takes your password only." />
                    </p>
                    {asking ? (
                      <Form onSubmit={start}>
                        <Form.Group className="mb-3" controlId="current-password">
                          <Form.Label>
                            <T text="Your current password" />
                          </Form.Label>
                          <Form.Control
                            type="password"
                            autoComplete="current-password"
                            value={password}
                            onChange={(e) => setPassword(e.target.value)}
                            autoFocus
                            required
                          />
                        </Form.Group>
                        <div className="d-flex gap-2">
                          <Button type="submit" disabled={busy || !password}>
                            <T text="Continue" />
                          </Button>
                          <Button variant="link" onClick={() => setAsking(false)}>
                            <T text="Cancel" />
                          </Button>
                        </div>
                      </Form>
                    ) : (
                      <Button onClick={() => setAsking(true)} disabled={busy}>
                        <T text="Set up two-factor authentication" />
                      </Button>
                    )}
                  </>
                )}
              </Card.Body>
            </Card>
          </div>
        </div>
      </PageBody>
    </>
  );
}
