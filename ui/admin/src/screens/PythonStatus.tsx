// The Development tab's reading of Python: which state this process is in, and
// the numbers behind it.
//
// It exists because "why does my Python trigger not work" has more than one
// answer and they are not distinguishable from the outside:
//
// - this binary was **built** without Python — no setting changes it, the fix is
//   a different build;
// - it has Python and was started with `--python off` — the fix is a restart;
// - it has Python and has not needed it yet — nothing is wrong;
// - it is running, and then the version is worth having.
//
// So the state is the headline, the server's own sentence sits under it, and
// everything else is only shown when it means something. **The sentence comes
// from the server** (`explanation`): which one is true depends on how the binary
// was built and how the process was started, and only the server knows either.
//
// Nothing here is editable. The two numbers an operator can change are flags
// that need a restart, and a form that pretended otherwise would be worse than
// no form: they are shown with the flag that sets them, which is the actual
// remedy.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import type { GetPythonStatusResponse } from "../client";
import { AlertBody } from "../layout";
import { T, useT } from "../i18n";

/** How each state is labelled, and how loudly. `unavailable` is the process
 * that booted no runtime at all — a test or an admin-only server — and is not
 * one of the three states of the specification, which is why it is not styled
 * as a problem. */
const STATES: Record<string, { label: string; tone: string }> = {
  running: { label: "Running", tone: "bg-green-lt" },
  not_initialised: { label: "Not started yet", tone: "bg-blue-lt" },
  off: { label: "Turned off", tone: "bg-yellow-lt" },
  not_built: { label: "Not built with Python", tone: "bg-secondary-lt" },
  unavailable: { label: "Not available", tone: "bg-secondary-lt" },
};

/** The badge for a state, falling back to the server's own word for a state
 * this build of the SPA has not heard of — a server ahead of its admin UI
 * should still say something true. */
export function stateBadge(state: string): { label: string; tone: string } {
  return STATES[state] ?? { label: state, tone: "bg-secondary-lt" };
}

/** Whether anything below the headline is worth showing.
 *
 * A build with no interpreter has no environment, no packages and no runs, and
 * a table of zeros beside "not built with Python" would read as though those
 * zeros were the problem. */
export function showsDetail(state: string): boolean {
  return state !== "not_built" && state !== "unavailable";
}

export function PythonStatusPanel() {
  const { t } = useT();
  const [status, setStatus] = useState<GetPythonStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        setStatus(await api.getPythonStatus());
      } catch (e) {
        setError(e instanceof Error ? e.message : "Could not read the Python status.");
      }
    })();
  }, []);

  const badge = status ? stateBadge(status.state) : null;

  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title"><T text="Python" /></h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="What this process can do about Python triggers, and what it is doing now. Nothing here is a setting: it is read from the running server." />
          </p>
        </div>
      </div>
      <div className="card-body">
        {error && (
          <Alert variant="danger">
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        {!status && !error && <Spinner animation="border" role="status" size="sm" />}
        {status && badge && (
          <>
            <div className="mb-3">
              <span className={`badge ${badge.tone} me-2`}>{badge.label}</span>
              {status.version && (
                <span className="text-secondary">
                  {t("CPython {version}", { version: status.version })}
                </span>
              )}
            </div>
            <p className="text-secondary">{status.explanation}</p>
            {showsDetail(status.state) && (
              <>
                <dl className="row mb-0">
                  <Reading
                    label={t("Environment")}
                    value={status.dir}
                    absent="This machine has no data directory to put one in, and none was named with --python-dir, so a body may import only the standard library."
                  />
                  <Reading label={t("Packages installed in")} value={status.site_packages} />
                  <Reading
                    label={t("pip runs under")}
                    value={status.bin ?? "python3 (on the PATH)"}
                  />
                  <Reading
                    label={t("Runs in flight")}
                    value={`${status.resident} of ${status.max_inflight} (--python-max-inflight)`}
                  />
                  <Reading
                    label={t("Run threads")}
                    value={`${status.threads} (a finished thread is kept for the next run)`}
                  />
                  <Reading
                    label={t("Threads that never returned")}
                    value={`${status.stuck} of ${status.max_stuck} tolerated (--python-max-stuck)`}
                  />
                </dl>
                {status.env_error && (
                  <Alert variant="danger" className="mt-3">
                    <AlertBody>{status.env_error}</AlertBody>
                  </Alert>
                )}
                {status.stuck > 0 && (
                  <Alert variant="warning" className="mt-3">
                    <AlertBody>
                      {t(
                        "{count} runs never returned. A thread stuck in C code can't be stopped, so this only drops when one finishes. At {cap}, new Python runs are refused until a restart.",
                        { count: status.stuck, cap: status.max_stuck },
                      )}
                    </AlertBody>
                  </Alert>
                )}
                <div className="mt-3">
                  <h4 className="mb-1"><T text="Installed packages" /></h4>
                  {status.packages.length === 0 ? (
                    <p className="text-secondary mb-0">
                      {status.env_error
                        ? "This environment is not being used, so nothing in it is listed."
                        : "Nothing is installed in this server's Python environment. A body may import the standard library; anything else has to be installed here."}
                    </p>
                  ) : (
                    <ul className="list-inline mb-0">
                      {status.packages.map((pkg) => (
                        <li className="list-inline-item" key={pkg.name}>
                          <span className="badge bg-secondary-lt">
                            {pkg.name}
                            {pkg.version ? ` ${pkg.version}` : ""}
                          </span>
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/** One label and its reading, or the sentence that says why there is none. */
function Reading({
  label,
  value,
  absent,
}: {
  label: string;
  value: string | null | undefined;
  absent?: string;
}) {
  return (
    <>
      <dt className="col-sm-4 fw-normal text-secondary">{label}</dt>
      <dd className="col-sm-8">
        {value ? (
          <code>{value}</code>
        ) : (
          <span className="text-secondary">{absent ?? "—"}</span>
        )}
      </dd>
    </>
  );
}
