// A notebook's session as it changes (analytics TODO A10.7): which cell is
// running, how many have run, the fit a cell waits for — off the session's
// socket (`/api/workspaces/{id}/session/live`, A10.6), and by reading
// `getNotebookSession` now and then should the socket not open.

import { useCallback, useEffect, useState } from "react";

import { api, sessionSocketUrl } from "../api";
import { NO_SESSION, type SessionStatus } from "./state";

/** How often the fallback reads the session. */
const POLL_MS = 1500;

/** A socket frame's session, or `null` for anything else. */
export function readSessionFrame(text: string): SessionStatus | null {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!raw || typeof raw !== "object") return null;
  const frame = raw as Record<string, unknown>;
  if (frame.type !== "status") return null;
  return readSession(frame.session);
}

/** A session's status read from anything; no session when it is not one. */
export function readSession(raw: unknown): SessionStatus {
  if (!raw || typeof raw !== "object") return NO_SESSION;
  const s = raw as Record<string, unknown>;
  if (typeof s.state !== "string") return NO_SESSION;
  return {
    state: s.state,
    language: typeof s.language === "string" ? s.language : null,
    cell: typeof s.cell === "string" ? s.cell : null,
    runs: typeof s.runs === "number" ? s.runs : 0,
    started: typeof s.started === "string" ? s.started : null,
    fit: typeof s.fit === "string" ? s.fit : null,
  };
}

/**
 * Follow notebook `workspace`'s session. Answers its status and a setter, for
 * what a run, an interrupt or a restart answers — sooner than the socket.
 */
export function useNotebookSession(workspace: string | undefined): [SessionStatus, (s: SessionStatus) => void] {
  const [status, setStatus] = useState<SessionStatus>(NO_SESSION);
  const set = useCallback((s: SessionStatus) => setStatus(s), []);

  useEffect(() => {
    if (!workspace) return undefined;
    let done = false;
    let timer: number | undefined;
    const poll = () => {
      timer = window.setTimeout(() => {
        void api
          .getNotebookSession(workspace)
          .then((s) => !done && setStatus(readSession(s)))
          .catch(() => undefined)
          .finally(() => !done && poll());
      }, POLL_MS);
    };
    void api
      .getNotebookSession(workspace)
      .then((s) => !done && setStatus(readSession(s)))
      .catch(() => undefined);
    let socket: WebSocket | null = null;
    try {
      socket = new WebSocket(sessionSocketUrl(workspace, window.location));
    } catch {
      poll();
    }
    if (socket) {
      socket.onmessage = (event) => {
        const s = readSessionFrame(String(event.data));
        if (s) setStatus(s);
      };
      // Refused, never upgraded, or gone: read the session instead.
      socket.onclose = () => {
        if (!done) poll();
      };
    }
    return () => {
      done = true;
      window.clearTimeout(timer);
      if (socket) {
        socket.onclose = null;
        socket.close();
      }
    };
  }, [workspace]);

  return [status, set];
}
