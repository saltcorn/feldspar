// Settings → System: which feldspar this is, what it runs on, and what it is
// doing to the machine — then the two acts on the whole installation, Restart
// and Clear all.
//
// The version card is the answer to "which version is installed?". The build
// time and commit are stamped by `scripts/build-static.sh`; a binary built any
// other way has neither, and the card says it is not a release build rather
// than leaving blanks that look like a fault. The line under it is the one to
// paste into a bug report.
//
// The readings are taken when the tab opens (and on Refresh), not streamed:
// the CPU figure is a quarter-second sample the server takes as it answers.
//
// **Restart** always exits the process, for whatever manages it (systemd,
// Docker, …) to start it again; nothing tries to detect one, since there are
// too many to recognise. After the restart the tab polls until a *different*
// process answers (`restarted`), then reloads the page — sessions are rows, so
// the admin is still signed in.

import { useEffect, useRef, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import type { GetSystemInfoResponse } from "../client";
import { AlertBody } from "../layout";
import { T, useT } from "../i18n";
import {
  formatBytes,
  formatUptime,
  restarted,
  shortCommit,
  usage,
  versionLine,
} from "../systemInfo";
import { ClearAllPanel } from "./ClearAll";

/** How often, and for how long, the tab asks whether the restart is over. */
const POLL_MS = 1000;
const GIVE_UP_MS = 120_000;

export function SystemTab() {
  const { t } = useT();
  const [info, setInfo] = useState<GetSystemInfoResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const load = async () => {
    setLoading(true);
    try {
      setInfo(await api.getSystemInfo());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not read the system information.");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load();
  }, []);

  return (
    <>
      {error && (
        <Alert variant="danger">
          <AlertBody>{error}</AlertBody>
        </Alert>
      )}
      {!info && !error && <Spinner animation="border" role="status" />}
      {info && (
        <>
          <Card
            title={t("Version")}
            subtitle={t("Which build of feldspar this server is running. Quote the line below when reporting a problem.")}
          >
            <dl className="row mb-3">
              <Reading label={t("Version")} value={info.version} />
              <Reading
                label={t("Commit")}
                value={info.commit ? shortCommit(info.commit) : null}
                title={info.commit ?? undefined}
                absent={t("Not a release build")}
              />
              <Reading
                label={t("Built (UTC)")}
                value={info.build_time}
                absent={t("Not a release build")}
              />
              <Reading label={t("Platform")} value={info.platform} />
            </dl>
            <code className="d-block user-select-all">{versionLine(info)}</code>
          </Card>

          <Card title={t("Database")}>
            {info.database.error ? (
              <p className="text-danger mb-0">{info.database.error}</p>
            ) : (
              <dl className="row mb-0">
                <Reading label={t("Type")} value={info.database.product} />
                <Reading label={t("Version")} value={info.database.version} />
              </dl>
            )}
          </Card>

          <Card
            title={t("Process and resources")}
            subtitle={t("Read from the running server when this tab was opened.")}
            action={
              <Button variant="outline-secondary" size="sm" disabled={loading} onClick={() => void load()}>
                {loading && <Spinner size="sm" className="me-2" />}
                <T text="Refresh" />
              </Button>
            }
          >
            <dl className="row mb-0">
              <Reading
                label={t("Started")}
                value={`${new Date(info.started_at).toLocaleString()} (${t("up {uptime}", {
                  uptime: formatUptime(info.uptime_seconds),
                })})`}
              />
              <Reading label={t("Process ID")} value={String(info.pid)} />
              <Reading
                label={t("Memory (this process)")}
                value={info.memory ? formatBytes(info.memory.process_bytes) : null}
                absent={t("Not available on this platform")}
              />
              <Reading
                label={t("Memory (machine)")}
                value={usageText(info.memory?.total_bytes, info.memory?.available_bytes, t("used"))}
                absent={t("Not available on this platform")}
              />
              <Reading
                label={t("CPU")}
                value={
                  info.cpu
                    ? t("{process}% this process, {system}% machine, {cores} cores", {
                        process: info.cpu.process_percent,
                        system: info.cpu.system_percent,
                        cores: info.cpu.cores,
                      })
                    : null
                }
                absent={t("Not available on this platform")}
              />
              {info.cpu && info.cpu.load_average.length > 0 && (
                <Reading label={t("Load average")} value={info.cpu.load_average.join(" · ")} />
              )}
              <Reading
                label={t("Disk")}
                value={
                  info.disk
                    ? `${usageText(info.disk.total_bytes, info.disk.available_bytes, t("used"))} — ${info.disk.path}`
                    : null
                }
                absent={t("Not available on this platform")}
              />
            </dl>
          </Card>

          <RestartPanel info={info} />
        </>
      )}
      <ClearAllPanel />
    </>
  );
}

/** `3.0 GiB of 8.0 GiB used (37.5%)`, or `null` when a number is missing. */
function usageText(
  total: number | null | undefined,
  available: number | null | undefined,
  used: string,
): string | null {
  const u = usage(total, available);
  return u ? `${formatBytes(u.used)} / ${formatBytes(u.total)} ${used} (${u.percent}%)` : null;
}

function RestartPanel({ info }: { info: GetSystemInfoResponse }) {
  const { t } = useT();
  const [confirming, setConfirming] = useState(false);
  const [waiting, setWaiting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const timer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );

  const restart = async () => {
    setConfirming(false);
    setError(null);
    try {
      await api.restartServer();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not restart the server.");
      return;
    }
    setWaiting(true);
    const giveUpAt = Date.now() + GIVE_UP_MS;
    const poll = async () => {
      try {
        if (restarted(info, await api.getSystemInfo())) {
          window.location.reload();
          return;
        }
      } catch {
        // The server is down between the two processes: keep asking.
      }
      if (Date.now() > giveUpAt) {
        setWaiting(false);
        setError(
          t("The server has not come back after two minutes. Check the service on the machine it runs on."),
        );
        return;
      }
      timer.current = window.setTimeout(() => void poll(), POLL_MS);
    };
    timer.current = window.setTimeout(() => void poll(), POLL_MS);
  };

  return (
    <Card
      title={t("Restart")}
      subtitle={t("Stop this server process so that its service manager starts it again. Requests in progress are cut off.")}
    >
      {error && (
        <Alert variant="danger" dismissible onClose={() => setError(null)}>
          <AlertBody>{error}</AlertBody>
        </Alert>
      )}
      {waiting ? (
        <div className="d-flex align-items-center">
          <Spinner size="sm" className="me-2" />
          <T text="Restarting… this page reloads when the server is back." />
        </div>
      ) : (
        <Button variant="warning" onClick={() => setConfirming(true)}>
          <T text="Restart" />
        </Button>
      )}
      <Modal show={confirming} onHide={() => setConfirming(false)} centered>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Restart the server?" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <p className="mb-0">
            <T text="The server process will exit, and every application is unavailable until it is back. If a service manager such as systemd or Docker runs it, it starts again by itself; otherwise it stays stopped until someone starts it on the machine." />
          </p>
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={() => setConfirming(false)}>
            <T text="Cancel" />
          </Button>
          <Button variant="warning" onClick={() => void restart()}>
            <T text="Restart" />
          </Button>
        </Modal.Footer>
      </Modal>
    </Card>
  );
}

function Card({
  title,
  subtitle,
  action,
  children,
}: {
  title: string;
  subtitle?: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title">{title}</h3>
          {subtitle && <p className="card-subtitle text-secondary mb-0">{subtitle}</p>}
        </div>
        {action && <div className="card-actions">{action}</div>}
      </div>
      <div className="card-body">{children}</div>
    </div>
  );
}

/** One `<dt>`/`<dd>` pair, with a muted sentence where there is no value. */
function Reading({
  label,
  value,
  absent = "—",
  title,
}: {
  label: string;
  value: string | null | undefined;
  absent?: string;
  title?: string;
}) {
  return (
    <>
      <dt className="col-sm-4">{label}</dt>
      <dd className="col-sm-8" title={title}>
        {value ?? <span className="text-secondary">{absent}</span>}
      </dd>
    </>
  );
}
