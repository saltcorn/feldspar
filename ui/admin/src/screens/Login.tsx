// Login screen: shown when a user exists but nobody is authenticated. A
// successful login starts a session cookie; a 401 means bad credentials.
//
// Below the password form, a button per external identity provider the admin
// UI offers (Settings → Authentication). Each is a plain link: the sign-in is a
// round of redirects through the provider, and it comes back to this page with
// a session — or with `?oauth_error=…`, which is shown here.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";

import { api, errorStatus } from "../api";
import { SaltcornLogo } from "../icons";
import { CenteredPage } from "../layout";
import {
  oauthErrorFrom,
  readSignInProviders,
  startUrl,
  type SignInProvider,
} from "../oauthProviders";
import { T, useT } from "../i18n";

export function Login({ onLoggedIn }: { onLoggedIn: () => void }) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const { t } = useT();
  const [error, setError] = useState<string | null>(() =>
    typeof window === "undefined" ? null : oauthErrorFrom(window.location.search),
  );
  const [busy, setBusy] = useState(false);
  const [providers, setProviders] = useState<SignInProvider[]>([]);

  useEffect(() => {
    void (async () => {
      try {
        setProviders(readSignInProviders(await api.oauthProviders()));
      } catch {
        // No providers to offer is the password form alone, as it always was.
      }
    })();
    // The error has been read; a reload should not show it again.
    if (typeof window !== "undefined" && oauthErrorFrom(window.location.search)) {
      window.history.replaceState(null, "", window.location.pathname + window.location.hash);
    }
  }, []);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.login({ email, password });
      onLoggedIn();
    } catch (err) {
      setError(
        errorStatus(err) === 401
          ? "Invalid email or password."
          : "Login failed. Please try again.",
      );
      setBusy(false);
    }
  };

  return (
    <CenteredPage>
      <div className="text-center mb-4">
        <span className="navbar-brand d-inline-flex align-items-center gap-2">
          <SaltcornLogo className="h-6" />
            <div className="ms-2 login-logo">
              <div className="saltcorn-label"><T text="Saltcorn" /></div>
              <div className="feldspar-label"><T text="Feldspar" /></div>
            </div>
        </span>
      </div>
      <Card className="card-md">
        <Card.Body>
          <Card.Title as="h1" className="h3 text-center mb-4">
            <T text="Sign in" />
          </Card.Title>
          {error && <Alert variant="danger">{error}</Alert>}
          <Form onSubmit={submit}>
            <Form.Group className="mb-3" controlId="loginEmail">
              <Form.Label><T text="Email" /></Form.Label>
              <Form.Control
                type="email"
                value={email}
                autoComplete="username"
                required
                onChange={(e) => setEmail(e.target.value)}
              />
            </Form.Group>
            <Form.Group className="mb-3" controlId="loginPassword">
              <Form.Label><T text="Password" /></Form.Label>
              <Form.Control
                type="password"
                value={password}
                autoComplete="current-password"
                required
                onChange={(e) => setPassword(e.target.value)}
              />
            </Form.Group>
            <div className="form-footer">
              <Button type="submit" className="w-100" disabled={busy}>
                {busy ? "Signing in…" : "Sign in"}
              </Button>
            </div>
          </Form>
          {providers.length > 0 && (
            <>
              <div className="hr-text">
                <T text="or" />
              </div>
              <div className="d-grid gap-2">
                {providers.map((provider) => (
                  <a
                    key={provider.name}
                    className="btn btn-outline-secondary w-100"
                    href={startUrl(provider, "/")}
                  >
                    {t("Sign in with {provider}", { provider: provider.label })}
                  </a>
                ))}
              </div>
            </>
          )}
        </Card.Body>
      </Card>
    </CenteredPage>
  );
}
