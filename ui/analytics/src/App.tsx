// The Analytics UI's shell (analytics TODO A1.14): who is signed in, the
// language and the colour scheme, a header, and the route.
//
// The session is the admin UI's own: the server serves this bundle only to a
// signed-in admin, so reaching this code signed out means the session expired
// while the page was open, and the answer is a link back to sign in.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "./api";
import type { AuthStatusResponse } from "./client";
import { NewDatasetPage } from "./datasets/DatasetList";
import { DatasetPage } from "./datasets/DatasetPage";
import { Home } from "./Home";
import { I18nProvider, T, useT } from "./i18n";
import { parseRoute, type Route } from "./router";
import { useTheme } from "./theme";
import { WorkspaceFrame } from "./workspaces/WorkspaceFrame";

/** The route the hash names, kept current. */
function useRoute(): Route {
  const [route, setRoute] = useState<Route>(() => parseRoute(window.location.hash));
  useEffect(() => {
    const onChange = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);
  return route;
}

export function App() {
  const [status, setStatus] = useState<AuthStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .authStatus()
      .then(setStatus)
      .catch((err: unknown) => setError(errorMessage(err, "Could not reach the server.")));
  }, []);

  if (error) {
    return (
      <div className="an-page">
        <Alert variant="danger">{error}</Alert>
      </div>
    );
  }
  if (!status) {
    return (
      <div className="an-page">
        <Spinner animation="border" size="sm" />
      </div>
    );
  }
  return (
    <I18nProvider locale={status.locales?.current ?? "en"}>
      {status.current_user ? <Shell email={status.current_user.email} /> : <SignedOut />}
    </I18nProvider>
  );
}

function SignedOut() {
  return (
    <div className="an-page">
      <Alert variant="warning">
        <T text="You are not signed in." />{" "}
        <a href="/">
          <T text="Sign in to the admin UI" />
        </a>
      </Alert>
    </div>
  );
}

function Shell({ email }: { email: string }) {
  const { t } = useT();
  const [theme, toggleTheme] = useTheme();
  const route = useRoute();
  return (
    <div className="an-shell">
      <header className="an-header">
        <a className="an-brand" href="#/">
          <T text="Analytics" />
        </a>
        <span className="text-secondary small ms-auto">{email}</span>
        <Button
          size="sm"
          variant="outline-secondary"
          onClick={toggleTheme}
          aria-label={theme === "dark" ? t("Light mode") : t("Dark mode")}
        >
          {theme === "dark" ? t("Light") : t("Dark")}
        </Button>
        <a className="btn btn-sm btn-outline-secondary" href="/">
          <T text="Admin" />
        </a>
      </header>
      <main className="an-main">
        <Page route={route} />
      </main>
    </div>
  );
}

function Page({ route }: { route: Route }) {
  switch (route.name) {
    case "home":
      return <Home />;
    case "workspace":
      return <WorkspaceFrame id={route.id} key={route.id} />;
    case "dataset":
      return <DatasetPage id={route.id} key={route.id} />;
    case "newDataset":
      return <NewDatasetPage table={route.table} />;
    case "notFound":
      return (
        <div className="an-page">
          <Alert variant="warning">
            <T text="There is nothing at {path}." args={{ path: route.path }} />{" "}
            <a href="#/">
              <T text="Back to the front page" />
            </a>
          </Alert>
        </div>
      );
  }
}
