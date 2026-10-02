// A running fit's progress, pushed by the server (analytics TODO A3.3, A3.5).
//
// `GET /api/model-instances/{id}/progress` is a WebSocket that sends a
// `progress` frame whenever the fit's stage, a chain's iteration or the cancel
// request changes, then one `finished` frame, then closes (`sc-server`'s
// `fit_progress.rs`). The model editor shows the frames as they come and
// re-reads the fit when it finishes. Should the socket not open — a proxy that
// does not pass upgrades — it falls back to reading the fit's row every
// second and a half, which is what the admin UI's screens did.

import { useEffect, useRef, useState } from "react";

import { api, progressSocketUrl } from "../api";
import { readProgress, type FitProgress } from "./models";

/** One frame off the socket. */
export type ProgressFrame =
  | { type: "progress"; status: string; progress: FitProgress | null; cancelRequested: boolean }
  | { type: "finished"; status: string; error: string | null };

/** A frame's text as a frame, or `null` for anything else. */
export function readFrame(text: string): ProgressFrame | null {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!raw || typeof raw !== "object") return null;
  const frame = raw as Record<string, unknown>;
  const status = typeof frame.status === "string" ? frame.status : "";
  if (frame.type === "progress") {
    return {
      type: "progress",
      status,
      progress: readProgress(frame.progress),
      cancelRequested: frame.cancel_requested === true,
    };
  }
  if (frame.type === "finished") {
    return { type: "finished", status, error: typeof frame.error === "string" ? frame.error : null };
  }
  return null;
}

/** A running fit's stage, as words. */
export function stageText(t: (text: string) => string, stage: string | undefined): string {
  switch (stage) {
    case "queued":
      return t("queued for a process");
    case "compiling":
      return t("compiling");
    case "sampling":
      return t("sampling");
    case "summarising":
      return t("summarising");
    case "reading":
      return t("reading the data");
    case "fitting":
      return t("fitting");
    case "scoring":
      return t("scoring");
    default:
      return "";
  }
}

/** What the editor knows of a running fit. */
export type LiveProgress = { progress: FitProgress | null; cancelRequested: boolean };

/** How often the fallback reads the fit's row. */
const POLL_MS = 1500;

/**
 * Follow the fit `instance` while it runs: its progress as it changes, and
 * `onFinished` once when it is over. `null` follows nothing.
 */
export function useFitProgress(instance: string | null, onFinished: () => void): LiveProgress | null {
  const [live, setLive] = useState<LiveProgress | null>(null);
  const finished = useRef(onFinished);
  finished.current = onFinished;

  useEffect(() => {
    setLive(null);
    if (!instance) return undefined;
    let done = false;
    let timer: number | undefined;
    const finish = () => {
      if (done) return;
      done = true;
      finished.current();
    };
    // The fallback: the row, until it is not `fitting`.
    const poll = () => {
      timer = window.setTimeout(() => {
        void api
          .getModelInstance(instance)
          .then((fit) => {
            if (done) return;
            if (fit.status !== "fitting") return finish();
            setLive({ progress: readProgress(fit.progress), cancelRequested: fit.cancel_requested });
            poll();
          })
          .catch(() => {
            if (!done) poll();
          });
      }, POLL_MS);
    };
    let socket: WebSocket | null = null;
    try {
      socket = new WebSocket(progressSocketUrl(instance, window.location));
    } catch {
      poll();
    }
    let heard = false;
    if (socket) {
      socket.onmessage = (event) => {
        heard = true;
        const frame = readFrame(String(event.data));
        if (frame?.type === "progress") {
          setLive({ progress: frame.progress, cancelRequested: frame.cancelRequested });
        } else if (frame?.type === "finished") {
          finish();
        }
      };
      socket.onclose = () => {
        // Closed without a word (refused, or never upgraded): read the row
        // instead. Closed after frames but before `finished`: the same.
        if (!done) poll();
      };
      socket.onerror = () => {
        if (!heard && !done) socket?.close();
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
  }, [instance]);

  return live;
}
