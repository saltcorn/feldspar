// Applications list: every application in `_fd_applications`, with the actions
// that make one real — build (+mount) it, edit its configuration, delete it —
// and a link to each app's own subdomain.
//
// An application is stored configuration; building it is a separate, slow step
// that mounts it live (design §13.2). The server keeps no persisted build status,
// so a freshly loaded (or freshly created) app shows as **not built yet** rather
// than pretending a save deployed anything; a successful build flips it to built
// for this session, and a failed build surfaces the bundler's own diagnostics.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListApplicationsResponse } from "../client";
import { claudeUrl, ideUrl, navigate } from "../App";
import { appUrl } from "../appNav";
import { graphqlMount } from "../graphqlExplorer";
import { IconPlus } from "../icons";
import { AlertBody, PageBody, PageHeader, StatusBadge } from "../layout";
import {
  buildApplication,
  buildStatus,
  deepCleanApplication,
  noteApplicationsChanged,
  showAppOutcome,
  updateApplicationClient,
  useAppActions,
  type BuildStatus,
} from "../appActions";
import { takeNotice } from "../notice";
import { T, useT } from "../i18n";

type AppItem = ListApplicationsResponse[number];

/** The file-manager route for a directory in a store. */
function filesUrl(store: string, path: string): string {
  const dir = path ? `/${path.split("/").map(encodeURIComponent).join("/")}` : "";
  return `#/files/${encodeURIComponent(store)}${dir}`;
}

export function Applications() {
  const { t } = useT();
  const [apps, setApps] = useState<AppItem[] | null>(null);
  // Builds, client updates and the news they leave behind live in a store the
  // sidebar shares (`appActions.ts`): the same buttons are there, for the
  // current application, and a build started from either is one build.
  const actions = useAppActions();
  const { outcome } = actions;
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    try {
      setApps(await api.listApplications());
    } catch {
      setError("Could not load applications.");
    }
  };

  useEffect(() => {
    void load();
    // A scaffold happens on the form, which leaves its message for this screen
    // to pick up — the same banner as a build's, because to an admin they are
    // the same news about the same app.
    const notice = takeNotice();
    if (notice) showAppOutcome(notice);
  }, []);

  // Not destructive — nothing the admin wrote is touched — but slow: a full
  // reinstall is minutes on a small server, and the app is rebuilt after it.
  const deepClean = async (app: AppItem) => {
    if (
      !window.confirm(
        t(
          'Deep clean "{name}"? Its node_modules is deleted and every dependency is downloaded and installed again before it is built. This can take several minutes.',
          { name: app.name },
        ),
      )
    ) {
      return;
    }
    await deepCleanApplication(app);
  };

  const remove = async (app: AppItem) => {
    if (
      !window.confirm(
        t(
          'Delete application "{name}"? The agent created to build it is deleted with it. This cannot be undone.',
          { name: app.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      // The agent goes with the application (§13.3), so say which one went: an
      // admin who edited that agent should not have to notice its absence.
      const result = await api.deleteApplication(app.id);
      await load();
      noteApplicationsChanged();
      if (result.agent) {
        showAppOutcome({
          ok: true,
          title: `Application deleted — ${app.name}`,
          text: `Its builder agent, ${result.agent}, was deleted with it. Its past runs are kept.`,
        });
      }
    } catch (err) {
      setError(errorMessage(err, "Could not delete the application."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Deploy"
        title={t("Applications")}
        actions={
          <Button onClick={() => navigate("/applications/new")}>
            <IconPlus className="icon-2" />
            <T text="New application" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {outcome && (
          <Alert
            variant={outcome.ok ? "success" : "danger"}
            onClose={() => showAppOutcome(null)}
            dismissible
          >
            <AlertBody>
              <Alert.Heading className="h6">{outcome.title}</Alert.Heading>
              <pre className="mb-0 text-break text-pre-wrap">{outcome.text}</pre>
            </AlertBody>
          </Alert>
        )}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Subdomain" /></th>
                <th><T text="Framework" /></th>
                <th><T text="Build" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {apps?.length === 0 && (
                <tr>
                  <td colSpan={5} className="text-muted">
                    <T text="No applications yet." />
                  </td>
                </tr>
              )}
              {apps?.map((app) => {
                const state = buildStatus(actions, app.id);
                return (
                  <tr key={app.id}>
                    <td>
                      {app.name}
                      {app.description && (
                        <div className="text-muted small">{app.description}</div>
                      )}
                    </td>
                    <td>
                      <a href={appUrl(app.subdomain)} target="_blank" rel="noreferrer">
                        {app.subdomain}
                      </a>
                    </td>
                    <td>
                      {app.framework.name}
                      {app.source && (
                        // The loop an admin actually works in is edit-file →
                        // build → view, so the source directory is one click
                        // from the row. Where an app's source *is* comes from
                        // the server (§2.4), so this link works the same for a
                        // framework that states its paths and one that derives
                        // them.
                        <div className="small">
                          <a href={filesUrl(app.source.store, app.source.path)}>
                            {app.source.store}/{app.source.path || ""}
                          </a>{" "}
                          <a href={ideUrl(app.source.store)}><T text="(edit code)" /></a>{" "}
                          {app.claude.available ? (
                            <a href={claudeUrl(app.source.store, app.name)}>
                              <T text="(build with Claude)" />
                            </a>
                          ) : (
                            // Shown anyway, with the server's reason, so the
                            // admin learns what to change.
                            <span
                              className="text-secondary"
                              style={{ cursor: "not-allowed" }}
                              title={app.claude.reason ?? undefined}
                            >
                              <T text="(build with Claude)" />
                            </span>
                          )}
                        </div>
                      )}
                    </td>
                    <td>
                      {app.builds ? (
                        <BuildBadge status={state} />
                      ) : (
                        // Constructed rather than built (Saltcorn UI): saving
                        // is the deployment, so there is no unbuilt state to
                        // show.
                        <span className="text-muted small"><T text="Nothing to build" /></span>
                      )}
                    </td>
                    <td className="text-end">
                      <div className="btn-list justify-content-end flex-nowrap">
                        {/* Only for an app that enables the provider: the
                            explorer has nothing to show for one that does not,
                            and the row should not offer a dead end. */}
                        {graphqlMount(app.apis) && (
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            href={`#/applications/${encodeURIComponent(app.id)}/graphql`}
                            title={t("Run GraphQL queries against this application")}
                          >
                            <T text="GraphQL" />
                          </Button>
                        )}
                        {app.has_views && (
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            href={`#/applications/${encodeURIComponent(app.id)}/views`}
                          >
                            <T text="Views" />
                          </Button>
                        )}
                        <Button
                          size="sm"
                          variant="outline-secondary"
                          href={`#/applications/${encodeURIComponent(app.id)}/edit`}
                        >
                          <T text="Edit" />
                        </Button>
                        {/* Neither has anything to do for an application with
                            no build: it has no generated client and no
                            bundle, and saving it already deployed it. */}
                        {app.builds && (
                          <>
                            <Button
                              size="sm"
                              variant="outline-secondary"
                              disabled={Boolean(actions.updating[app.id])}
                              onClick={() => void updateApplicationClient(app)}
                              title={t("Rewrite this application's generated client, hooks and schema from its current definition — no build")}
                            >
                              {actions.updating[app.id] ? "Updating…" : "Update code"}
                            </Button>
                            <Button
                              size="sm"
                              variant="outline-primary"
                              disabled={state === "building"}
                              onClick={() => void buildApplication(app)}
                            >
                              {state === "building" ? "Building…" : "Build"}
                            </Button>
                            {/* Only where the server installs the dependencies
                                itself (a React app's `npm install`): for any
                                other app they are not the server's to delete. */}
                            {app.installs && (
                              <Button
                                size="sm"
                                variant="outline-warning"
                                disabled={state === "building"}
                                onClick={() => void deepClean(app)}
                                title={t("Delete node_modules, reinstall every dependency and build")}
                              >
                                <T text="Deep clean" />
                              </Button>
                            )}
                          </>
                        )}
                        <Button size="sm" variant="outline-danger" onClick={() => void remove(app)}>
                          <T text="Delete" />
                        </Button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        </div>
      </PageBody>
    </>
  );
}

/** The per-app build state as a coloured badge, including the saved-but-unbuilt
 * state a newly created (or freshly reloaded) app shows. */
function BuildBadge({ status }: { status: BuildStatus }) {
  switch (status) {
    case "built":
      return <StatusBadge tone="green"><T text="Built" /></StatusBadge>;
    case "building":
      return <StatusBadge tone="blue"><T text="Building…" /></StatusBadge>;
    case "failed":
      return <StatusBadge tone="red"><T text="Build failed" /></StatusBadge>;
    default:
      return <StatusBadge tone="secondary"><T text="Not built yet" /></StatusBadge>;
  }
}
