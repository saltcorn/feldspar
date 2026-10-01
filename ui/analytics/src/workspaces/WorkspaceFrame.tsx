// One open workspace (analytics TODO A1.15): its name, and its kind's screen
// over the state it was left in.
//
// The frame owns the state's persistence and nothing else. The kind's screen
// is handed the state and a setter; every change is saved a moment later
// (`StateSaver`), and leaving the workspace — another route, a closed tab —
// saves what is pending. So "reopen the workspace and it is as it was" is the
// frame's promise, and no kind has to remember to keep it.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { GetWorkspaceResponse } from "../client";
import { T, useT } from "../i18n";
import { StateSaver, type SaveStatus } from "./saver";

/** How long after the last change the state is saved. */
const SAVE_DELAY_MS = 600;

/** A workspace's state: the kind's own JSON object. */
export type WorkspaceState = Record<string, unknown>;

/** What a kind's screen is handed. */
export type WorkspaceProps = {
  state: WorkspaceState;
  setState: (update: (state: WorkspaceState) => WorkspaceState) => void;
};

export function WorkspaceFrame({ id }: { id: string }) {
  const { t } = useT();
  const [workspace, setWorkspace] = useState<GetWorkspaceResponse | null>(null);
  const [state, setLocal] = useState<WorkspaceState>({});
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<SaveStatus>("saved");

  const saver = useMemo(
    () =>
      new StateSaver<WorkspaceState>(
        async (s) => {
          await api.saveWorkspaceState(id, { state: s });
        },
        SAVE_DELAY_MS,
        (next) => setStatus(next),
      ),
    [id],
  );

  useEffect(() => {
    let live = true;
    api
      .getWorkspace(id)
      .then((ws) => {
        if (!live) return;
        setWorkspace(ws);
        setLocal(isObject(ws.state) ? ws.state : {});
      })
      .catch((err: unknown) => setError(errorMessage(err, t("Could not open the workspace."))));
    return () => {
      live = false;
    };
  }, [id, t]);

  // Leaving: another route unmounts this frame; a closed tab hides the page.
  useEffect(() => {
    const onHide = () => void saver.flush();
    window.addEventListener("pagehide", onHide);
    return () => {
      window.removeEventListener("pagehide", onHide);
      void saver.flush();
    };
  }, [saver]);

  const latest = useRef(state);
  latest.current = state;
  const setState = useCallback(
    (update: (s: WorkspaceState) => WorkspaceState) => {
      const next = update(latest.current);
      latest.current = next;
      setLocal(next);
      saver.update(next);
    },
    [saver],
  );

  if (error) {
    return (
      <div className="an-page">
        <Alert variant="danger">
          {error}{" "}
          <a href="#/">
            <T text="Back to the workspaces" />
          </a>
        </Alert>
      </div>
    );
  }
  if (!workspace) {
    return (
      <div className="an-page">
        <Spinner animation="border" size="sm" />
      </div>
    );
  }
  return (
    <div className="d-flex flex-column h-100">
      <div className="d-flex align-items-center gap-3 px-3 py-2 border-bottom">
        <a href="#/" className="text-secondary">
          ← <T text="Workspaces" />
        </a>
        <strong>{workspace.name}</strong>
        <span className="text-secondary small ms-auto" aria-live="polite">
          {status === "saved" && t("Saved")}
          {(status === "pending" || status === "saving") && t("Saving…")}
          {status === "failed" && t("Not saved — will retry on the next change")}
        </span>
      </div>
      <div className="flex-grow-1" style={{ minHeight: 0 }}>
        <KindScreen kind={workspace.kind} state={state} setState={setState} />
      </div>
    </div>
  );
}

/** The kind's screen, handed the state and its setter. Each kind's arrives with
 * its milestone; none is here before A2's Data explorer. */
function KindScreen(_props: WorkspaceProps & { kind: string }) {
  return (
    <div className="an-page">
      <Alert variant="info">
        <T text="This kind of workspace is not here yet." />
      </Alert>
    </div>
  );
}

function isObject(v: unknown): v is WorkspaceState {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}
