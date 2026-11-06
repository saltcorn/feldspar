// Settings → Development → Claude Code: which `claude` the IDE's terminal
// would run, read from the running server (`getClaudeCodeStatus`). Nothing here
// is a setting; the executable's path is the field above it.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { sourceLabel, statusBadge } from "../claudeStatus";
import type { GetClaudeCodeStatusResponse } from "../client";
import { T, useT } from "../i18n";
import { AlertBody } from "../layout";

export function ClaudeCodeStatusPanel() {
  const { t } = useT();
  const [status, setStatus] = useState<GetClaudeCodeStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api
      .getClaudeCodeStatus()
      .then((s) => live && setStatus(s))
      .catch((e) => live && setError(errorMessage(e, t("Could not read the Claude Code status."))));
    return () => {
      live = false;
    };
  }, []);

  const badge = status ? statusBadge(status, t) : null;
  const source = sourceLabel(status?.source, t);

  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title"><T text="Claude Code" /></h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="Which claude the IDE's terminal runs, read from the running server." />
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
              {status.version && <span className="text-secondary">{status.version}</span>}
            </div>
            {status.path && (
              <p className="mb-2">
                <code>{status.path}</code>
                {source && <span className="text-secondary"> — {source}</span>}
              </p>
            )}
            {status.problem && <p className="text-secondary mb-2">{status.problem}</p>}
            {!status.enabled && (
              <p className="text-secondary mb-0">
                <T text="Claude Code in the IDE is not saved as on yet, so no terminal starts." />
              </p>
            )}
          </>
        )}
      </div>
    </div>
  );
}
