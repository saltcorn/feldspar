// The admin UI's start page: what needs attention, what is here, and — while
// the installation is nearly empty — what to do next (`home.ts` decides each).
//
// Every list is fetched on its own and settles on its own, so a list the
// server refuses (or a slow one) costs only its own card. The full index of
// everything is All entities, one entry below this one in the sidebar.

import { useEffect, useState, type ReactNode } from "react";
import Dropdown from "react-bootstrap/Dropdown";

import { api } from "../api";
import { appUrl } from "../adminAddress";
import type {
  GetSystemInfoResponse,
  ListApplicationsResponse,
  ListBackupSchedulesResponse,
  ListChecklistsResponse,
  ListLlmProvidersResponse,
  ListTriggersResponse,
  ListUsersResponse,
} from "../client";
import {
  configHref,
  kindLabels,
  recentlyEdited,
  timeAgo,
  type Entity,
  type EntityKind,
} from "../entities";
import {
  attention,
  entityCounts,
  gettingStarted,
  stillStarting,
  uncheckedChecks,
  uncheckedMessage,
} from "../home";
import { T, useT } from "../i18n";
import {
  IconAlertTriangle,
  IconApps,
  IconBolt,
  IconCheck,
  IconExternalLink,
  IconFolder,
  IconPlus,
  IconRobot,
  IconTable,
  IconUsers,
} from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { formatBytes, formatUptime, usage } from "../systemInfo";
import { createButtons } from "./Entities";

/** A list as Home holds it: not yet answered, answered, or refused (`null`). */
type Loaded<V> = V | null | undefined;

/** Fetch `load` once, settling to `null` on failure. */
function useLoaded<V>(load: () => Promise<V>): Loaded<V> {
  const [value, setValue] = useState<Loaded<V>>(undefined);
  useEffect(() => {
    let live = true;
    load()
      .then((v) => live && setValue(v))
      .catch(() => live && setValue(null));
    return () => {
      live = false;
    };
    // `load` is a fresh arrow each render; fetching once is the point.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return value;
}

/** `undefined` (still loading) read as "not loaded" for the pure functions. */
const settled = <V,>(value: Loaded<V>): V | null => value ?? null;

export function Home() {
  const { t } = useT();
  const entities = useLoaded<Entity[]>(() => api.listEntities());
  const triggers = useLoaded<ListTriggersResponse>(() => api.listTriggers());
  const schedules = useLoaded<ListBackupSchedulesResponse>(() => api.listBackupSchedules());
  const checklists = useLoaded<ListChecklistsResponse>(() => api.listChecklists());
  const system = useLoaded<GetSystemInfoResponse>(() => api.getSystemInfo());
  const providers = useLoaded<ListLlmProvidersResponse>(() => api.listLlmProviders());
  const applications = useLoaded<ListApplicationsResponse>(() => api.listApplications());
  const users = useLoaded<ListUsersResponse>(() => api.listUsers());

  const loadingAttention = [triggers, schedules, checklists, system].some((v) => v === undefined);
  const attentionInputs = {
    triggers: settled(triggers),
    schedules: settled(schedules),
    checklists: settled(checklists),
    system: settled(system),
  };
  const problems = attention(attentionInputs, t);
  // Only once every list has answered: before that, `null` means "loading".
  const unchecked = loadingAttention
    ? null
    : uncheckedMessage(uncheckedChecks(attentionInputs), t);
  const steps = gettingStarted(
    { entities: settled(entities), providers: settled(providers), schedules: settled(schedules) },
    t,
  );

  return (
    <>
      <PageHeader
        pretitle={t("Overview")}
        title={t("Home")}
        actions={
          <Dropdown align="end">
            <Dropdown.Toggle variant="primary">
              <IconPlus className="icon-2" />
              <T text="Create" />
            </Dropdown.Toggle>
            <Dropdown.Menu>
              {createButtons(t).map((create) => (
                <Dropdown.Item key={create.kind} href={create.href}>
                  {create.label}
                </Dropdown.Item>
              ))}
            </Dropdown.Menu>
          </Dropdown>
        }
      />
      <PageBody>
        <Counts entities={entities} users={users} />
        <div className="row row-cards align-items-start">
          <div className="col-lg-8">
            <div className="d-flex flex-column gap-3 w-100">
              <Attention loading={loadingAttention} items={problems} unchecked={unchecked} />
              <Applications applications={applications} />
            </div>
          </div>
          <div className="col-lg-4">
            <div className="d-flex flex-column gap-3 w-100">
              {stillStarting(steps) && <GettingStarted steps={steps} />}
              <Recent entities={entities} />
              <SystemHealth system={system} schedules={schedules} />
            </div>
          </div>
        </div>
      </PageBody>
    </>
  );
}

/** The kinds counted along the top, with where each count leads. */
const COUNTED: { kind: EntityKind | "user"; icon: ReactNode; color: string; href: string }[] = [
  { kind: "table", icon: <IconTable />, color: "blue", href: "#/tables" },
  { kind: "application", icon: <IconApps />, color: "purple", href: "#/applications" },
  { kind: "trigger", icon: <IconBolt />, color: "yellow", href: "#/triggers" },
  { kind: "agent", icon: <IconRobot />, color: "green", href: "#/agents" },
  { kind: "file_store", icon: <IconFolder />, color: "cyan", href: "#/file-stores" },
  { kind: "user", icon: <IconUsers />, color: "orange", href: "#/users" },
];

function Counts({ entities, users }: { entities: Loaded<Entity[]>; users: Loaded<ListUsersResponse> }) {
  const { t } = useT();
  const counts = entities ? entityCounts(entities) : null;
  const labels = kindLabels(t);
  return (
    <div className="row row-cards mb-3">
      {COUNTED.map((c) => {
        const n = c.kind === "user" ? users?.length : counts?.[c.kind];
        const label = c.kind === "user" ? t("Users") : labels[c.kind].many;
        return (
          <div key={c.kind} className="col-6 col-md-4 col-xl-2">
            <a href={c.href} className="card card-sm card-link">
              <div className="card-body d-flex align-items-center gap-3">
                <span className={`avatar bg-${c.color}-lt`}>{c.icon}</span>
                <div>
                  <div className="h2 mb-0">{n ?? "—"}</div>
                  <div className="text-secondary">{label}</div>
                </div>
              </div>
            </a>
          </div>
        );
      })}
    </div>
  );
}

function Attention({
  loading,
  items,
  unchecked,
}: {
  loading: boolean;
  items: ReturnType<typeof attention>;
  /** Which checks could not run, as a sentence; `null` when all ran. */
  unchecked: string | null;
}) {
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title"><T text="Needs attention" /></h3>
      </div>
      {items.length > 0 && (
        <div className="list-group list-group-flush">
          {items.map((item) => (
            <a key={item.key} href={item.href} className="list-group-item list-group-item-action">
              <div className="d-flex align-items-start gap-2">
                <IconAlertTriangle className={`text-${item.tone} mt-1`} />
                <div className="flex-fill text-truncate">
                  <div className="fw-medium">{item.title}</div>
                  {item.detail && (
                    <div className="text-secondary small text-truncate" title={item.detail}>
                      {item.detail}
                    </div>
                  )}
                </div>
              </div>
            </a>
          ))}
        </div>
      )}
      {/* A check that did not run is said out loud, whether or not others
          found something: silence would read as "nothing wrong" there. */}
      {unchecked && (
        <div className={`card-body d-flex align-items-start gap-2${items.length > 0 ? " border-top" : ""}`}>
          <IconAlertTriangle className="text-yellow mt-1" />
          <span>{unchecked}</span>
        </div>
      )}
      {items.length === 0 && !unchecked && (
        <div className="card-body text-secondary d-flex align-items-center gap-2">
          {loading ? (
            <T text="Checking…" />
          ) : (
            <>
              <IconCheck className="text-green" />
              <T text="Nothing needs your attention: no failing triggers, backups or checklists." />
            </>
          )}
        </div>
      )}
    </div>
  );
}

function Applications({ applications }: { applications: Loaded<ListApplicationsResponse> }) {
  const { t } = useT();
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title"><T text="Applications" /></h3>
        <div className="card-actions">
          <a href="#/applications" className="btn btn-sm btn-ghost-primary">
            <T text="All applications" />
          </a>
        </div>
      </div>
      {applications === undefined ? (
        <div className="card-body text-secondary"><T text="Loading…" /></div>
      ) : applications === null ? (
        <div className="card-body text-secondary"><T text="The applications could not be loaded." /></div>
      ) : applications.length === 0 ? (
        <div className="card-body">
          <p className="text-secondary">
            <T text="An application is what your users see: a site on its own subdomain, made from views and pages or from code." />
          </p>
          <a href="#/applications/new" className="btn btn-primary">
            <IconPlus className="icon-2" />
            <T text="Create application" />
          </a>
        </div>
      ) : (
        <div className="list-group list-group-flush">
          {applications.map((app) => (
            <div key={app.id} className="list-group-item">
              <div className="row align-items-center g-2">
                <div className="col text-truncate">
                  <a href={`#/applications/${encodeURIComponent(app.id)}/edit`} className="fw-medium">
                    {app.name}
                  </a>
                  <div className="text-secondary small text-truncate">
                    {app.description || app.subdomain}
                  </div>
                </div>
                <div className="col-auto">
                  <span className="badge bg-secondary-lt">{app.framework.name}</span>
                </div>
                <div className="col-auto">
                  <a
                    href={appUrl(app.subdomain)}
                    target="_blank"
                    rel="noreferrer"
                    className="btn btn-sm btn-outline-secondary"
                    title={t("Open the application in a new tab")}
                  >
                    <IconExternalLink className="icon-2" />
                    <T text="Open" />
                  </a>
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function GettingStarted({ steps }: { steps: NonNullable<ReturnType<typeof gettingStarted>> }) {
  const done = steps.filter((s) => s.done).length;
  const { t } = useT();
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title"><T text="Getting started" /></h3>
        <div className="card-actions text-secondary small">
          {t("{done} of {count} done", { done, count: steps.length })}
        </div>
      </div>
      <div className="list-group list-group-flush">
        {steps.map((step) => (
          <a
            key={step.key}
            href={step.href}
            className={`list-group-item list-group-item-action${step.done ? " text-secondary" : ""}`}
          >
            <div className="d-flex align-items-start gap-2">
              <span className={`avatar avatar-xs rounded-circle ${step.done ? "bg-green-lt" : "bg-secondary-lt"}`}>
                {step.done ? <IconCheck /> : null}
              </span>
              <div>
                <div className={step.done ? "text-decoration-line-through" : "fw-medium"}>
                  {step.title}
                </div>
                {!step.done && <div className="text-secondary small">{step.why}</div>}
              </div>
            </div>
          </a>
        ))}
      </div>
    </div>
  );
}

function Recent({ entities }: { entities: Loaded<Entity[]> }) {
  const { t } = useT();
  const labels = kindLabels(t);
  const recent = entities ? recentlyEdited(entities) : [];
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title"><T text="Recently edited" /></h3>
        <div className="card-actions">
          <a href="#/entities" className="btn btn-sm btn-ghost-primary">
            <T text="All entities" />
          </a>
        </div>
      </div>
      {recent.length === 0 ? (
        <div className="card-body text-secondary">
          {entities === undefined ? <T text="Loading…" /> : <T text="Nothing has been edited yet." />}
        </div>
      ) : (
        <div className="list-group list-group-flush">
          {recent.map((e) => (
            <a
              key={`${e.kind}:${e.id}`}
              href={configHref(e)}
              className="list-group-item list-group-item-action d-flex align-items-center gap-2"
            >
              <span className="flex-fill text-truncate">{e.name}</span>
              <span className="text-secondary small">{labels[e.kind as EntityKind]?.one ?? e.kind}</span>
              {e.updated_at && (
                <span className="badge bg-muted-lt" title={new Date(e.updated_at).toLocaleString()}>
                  {timeAgo(e.updated_at)}
                </span>
              )}
            </a>
          ))}
        </div>
      )}
    </div>
  );
}

function SystemHealth({
  system,
  schedules,
}: {
  system: Loaded<GetSystemInfoResponse>;
  schedules: Loaded<ListBackupSchedulesResponse>;
}) {
  const { t } = useT();
  const disk = system?.disk ? usage(system.disk.total_bytes, system.disk.available_bytes) : null;
  const memory = system?.memory
    ? usage(system.memory.total_bytes, system.memory.available_bytes)
    : null;
  const lastBackup = (schedules ?? [])
    .map((s) => s.last_success_at)
    .filter((at): at is string => !!at)
    .sort()
    .pop();
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title"><T text="System" /></h3>
        <div className="card-actions">
          <a href="#/settings/system" className="btn btn-sm btn-ghost-primary">
            <T text="Details" />
          </a>
        </div>
      </div>
      <div className="card-body">
        {system === undefined ? (
          <div className="text-secondary"><T text="Loading…" /></div>
        ) : system === null ? (
          <div className="text-secondary"><T text="The system information could not be loaded." /></div>
        ) : (
          <dl className="row mb-0">
            <dt className="col-5 fw-normal text-secondary"><T text="Version" /></dt>
            <dd className="col-7">{system.version}</dd>
            <dt className="col-5 fw-normal text-secondary"><T text="Up for" /></dt>
            <dd className="col-7">{formatUptime(system.uptime_seconds)}</dd>
            <dt className="col-5 fw-normal text-secondary"><T text="Database" /></dt>
            <dd className="col-7 text-truncate" title={system.database.version ?? undefined}>
              {system.database.error ? (
                <StatusBadge tone="red" title={system.database.error}><T text="Unreachable" /></StatusBadge>
              ) : (
                (system.database.product ?? "—")
              )}
            </dd>
            {disk && <Meter label={t("Disk")} used={disk.used} total={disk.total} percent={disk.percent} />}
            {memory && (
              <Meter label={t("Memory")} used={memory.used} total={memory.total} percent={memory.percent} />
            )}
            <dt className="col-5 fw-normal text-secondary"><T text="Last backup" /></dt>
            <dd className="col-7 mb-0">
              {lastBackup ? (
                <span title={new Date(lastBackup).toLocaleString()}>{timeAgo(lastBackup)}</span>
              ) : (
                <a href="#/settings/backup"><T text="None scheduled" /></a>
              )}
            </dd>
          </dl>
        )}
      </div>
    </div>
  );
}

function Meter({
  label,
  used,
  total,
  percent,
}: {
  label: string;
  used: number;
  total: number;
  percent: number;
}) {
  const tone = percent >= 90 ? "red" : percent >= 75 ? "yellow" : "primary";
  return (
    <>
      <dt className="col-5 fw-normal text-secondary">{label}</dt>
      <dd className="col-7">
        <div className="small">
          {formatBytes(used)} / {formatBytes(total)}
        </div>
        <div className="progress progress-sm" role="progressbar" aria-valuenow={percent} aria-valuemin={0} aria-valuemax={100}>
          <div className={`progress-bar bg-${tone}`} style={{ width: `${percent}%` }} />
        </div>
      </dd>
    </>
  );
}
