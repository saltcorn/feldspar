// The admin UI's home page: every entity in one list (Saltcorn 1's "All
// entities").
//
// Tables, file stores, triggers, agents and applications, each a row with a
// link to its configuration screen and a Run link where running one means
// something. The five most recently edited come first, under their own heading,
// because "the thing I was working on" is what an admin opening the admin UI is
// most often looking for.
//
// The search matches names, descriptions and kinds; ticking **Deep search**
// fetches the listing again with every definition in it, and matches inside
// those too. The type buttons narrow the list to the kinds switched on, and
// with none switched on every kind is shown. What is kept and how it is
// ordered is `entities.ts`'s.
//
// The **Diagram** switch in the page header swaps the list for an entity
// relationship diagram of the tables the search and filter keep, each linking
// to its table page (`EntityDiagram.tsx`).
//
// The screen has the viewport, as Saltcorn 1's did: the list scrolls inside
// the card, and the search above it and the "Create …" buttons below it stay
// in view however many entities there are (`.entities-page` in `admin.css`).

import { useEffect, useMemo, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import {
  ENTITY_KINDS,
  KIND_LABELS,
  configHref,
  matchesFilter,
  recentlyEdited,
  runAction,
  sortEntities,
  timeAgo,
  toggleKind,
  type Entity,
  type EntityKind,
} from "../entities";
import {
  IconApps,
  IconBolt,
  IconFilter,
  IconFolder,
  IconPlus,
  IconRobot,
  IconSearch,
  IconTable,
} from "../icons";
import { PageBody, PageHeader } from "../layout";
import { T, useT } from "../i18n";
import { testRunFailure, testRunOutcome, type TestRunOutcome } from "../triggerTestRun";
import { TestRunToast } from "./Triggers";
import { EntityDiagram } from "./EntityDiagram";

/** Each kind's icon and Tabler colour, for its badge and its filter button. */
const KIND_LOOK: Record<EntityKind, { icon: ReactNode; color: string }> = {
  table: { icon: <IconTable />, color: "blue" },
  file_store: { icon: <IconFolder />, color: "cyan" },
  trigger: { icon: <IconBolt />, color: "yellow" },
  agent: { icon: <IconRobot />, color: "green" },
  application: { icon: <IconApps />, color: "purple" },
};

/** Where each "Create …" button at the foot of the list goes. A table is
 * created from the tables list, whose dialog offers the ways to make one. */
const CREATE: { kind: EntityKind; label: string; href: string }[] = [
  { kind: "table", label: "Create table", href: "#/tables" },
  { kind: "file_store", label: "Create file store", href: "#/file-stores/new" },
  { kind: "trigger", label: "Create trigger", href: "#/triggers/new" },
  { kind: "agent", label: "Create agent", href: "#/agents/new" },
  { kind: "application", label: "Create application", href: "#/applications/new" },
];

export function Entities() {
  const { t } = useT();
  const [shallow, setShallow] = useState<Entity[] | null>(null);
  /** The listing with definitions, fetched the first time Deep search is
   * ticked and kept: it is the bigger answer, and most searches never need it. */
  const [deepList, setDeepList] = useState<Entity[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [deep, setDeep] = useState(false);
  const [diagram, setDiagram] = useState(false);
  const [kinds, setKinds] = useState<EntityKind[]>([]);
  const [ran, setRan] = useState<TestRunOutcome | null>(null);
  const [running, setRunning] = useState<string | null>(null);

  useEffect(() => {
    api
      .listEntities()
      .then(setShallow)
      .catch((err) => setError(errorMessage(err, "Could not load the entities.")));
  }, []);

  useEffect(() => {
    if (!deep || deepList) return;
    api
      .listEntities({ deep: true })
      .then(setDeepList)
      .catch((err) => setError(errorMessage(err, "Could not load the definitions.")));
  }, [deep, deepList]);

  // A deep search before the definitions have arrived searches the names, so
  // the list does not empty and refill while they load.
  const entities = (deep ? deepList : null) ?? shallow;
  const searchDeep = deep && deepList !== null;
  const kept = useMemo(
    () =>
      (entities ?? []).filter((e) =>
        matchesFilter(e, { query, kinds, deep: searchDeep }),
      ),
    [entities, query, kinds, searchDeep],
  );
  const recent = useMemo(() => recentlyEdited(kept), [kept]);
  const all = useMemo(() => sortEntities(kept), [kept]);
  const tables = useMemo(
    () => all.filter((e) => e.kind === "table").map((e) => e.id),
    [all],
  );

  const testRun = async (entity: Entity) => {
    // A trigger's first detail is its event, which is what the outcome needs
    // to know whether a row was due.
    const trigger = { name: entity.name, when: entity.details[0] ?? "none" };
    setRan(null);
    setRunning(entity.id);
    try {
      setRan(testRunOutcome(trigger, await api.testRunTrigger(entity.id, {})));
    } catch (err) {
      setRan(testRunFailure(trigger, errorMessage(err, "The trigger could not be run.")));
    } finally {
      setRunning(null);
    }
  };

  const row = (entity: Entity, showAge: boolean) => (
    <EntityRow
      key={`${entity.kind}:${entity.id}`}
      entity={entity}
      showAge={showAge}
      running={running === entity.id}
      onTestRun={() => void testRun(entity)}
    />
  );

  return (
    <div className="entities-page">
      <PageHeader
        pretitle={t("Overview")}
        title={t("All entities")}
        actions={
          <Form.Check
            type="switch"
            id="entities-diagram"
            className="mb-0"
            checked={diagram}
            onChange={(e) => setDiagram(e.target.checked)}
            label={t("Diagram")}
            title={t("Show the tables as an entity relationship diagram")}
          />
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {ran && <TestRunToast outcome={ran} onClose={() => setRan(null)} />}

        <div className="card entities-card">
          <div className="card-body border-bottom">
            <div className="input-group mb-3">
              <span className="input-group-text">
                <IconSearch />
              </span>
              <Form.Control
                type="search"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder={t("Search entities by name or type…")}
                aria-label={t("Search entities")}
                autoFocus
              />
              <span className="input-group-text">
                <Form.Check
                  id="entities-deep-search"
                  className="mb-0"
                  checked={deep}
                  onChange={(e) => setDeep(e.target.checked)}
                  label={t("Deep search")}
                  title={t("Search inside each entity's whole definition")}
                />
              </span>
            </div>
            <div className="d-flex align-items-center flex-wrap gap-2">
              <span className="text-secondary d-inline-flex align-items-center gap-1">
                <IconFilter />
                <T text="Types:" />
              </span>
              <div className="btn-group btn-group-sm flex-wrap" role="group" aria-label={t("Types")}>
                {ENTITY_KINDS.map((kind) => {
                  const on = kinds.includes(kind);
                  return (
                    <button
                      key={kind}
                      type="button"
                      className={on ? "btn btn-primary" : "btn btn-outline-primary"}
                      aria-pressed={on}
                      onClick={() => setKinds((current) => toggleKind(current, kind))}
                    >
                      {KIND_LOOK[kind].icon}
                      {t(KIND_LABELS[kind].many)}
                    </button>
                  );
                })}
              </div>
            </div>
          </div>

          {diagram ? (
            entities !== null && <EntityDiagram tables={tables} />
          ) : (
          <div className="entities-scroll">
          <Table hover className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Type" /></th>
                <th><T text="Name" /></th>
                <th><T text="Run" /></th>
                <th><T text="Details" /></th>
                <th><T text="Access" /></th>
              </tr>
            </thead>
            <tbody>
              {entities === null && !error && (
                <tr>
                  <td colSpan={5} className="text-secondary"><T text="Loading…" /></td>
                </tr>
              )}
              {recent.length > 0 && (
                <>
                  <SectionRow><T text="Recently edited" /></SectionRow>
                  {recent.map((e) => row(e, true))}
                </>
              )}
              {entities !== null && (
                <SectionRow><T text="All entities" /></SectionRow>
              )}
              {entities !== null && all.length === 0 && (
                <tr>
                  <td colSpan={5} className="text-secondary">
                    {entities.length === 0 ? (
                      <T text="Nothing has been created yet." />
                    ) : (
                      <T text="No entity matches." />
                    )}
                  </td>
                </tr>
              )}
              {all.map((e) => row(e, false))}
            </tbody>
          </Table>
          </div>
          )}

          <div className="card-footer">
            <div className="btn-list">
              {CREATE.map((create) => (
                <Button key={create.kind} variant="outline-secondary" href={create.href}>
                  <IconPlus className="icon-2" />
                  {t(create.label)}
                </Button>
              ))}
            </div>
          </div>
        </div>
      </PageBody>
    </div>
  );
}

/** A section heading inside the table: "Recently edited", "All entities". */
function SectionRow({ children }: { children: ReactNode }) {
  return (
    <tr className="entities-section">
      <td colSpan={5} className="text-uppercase small fw-medium text-secondary py-1">
        {children}
      </td>
    </tr>
  );
}

function EntityRow({
  entity,
  showAge,
  running,
  onTestRun,
}: {
  entity: Entity;
  showAge: boolean;
  running: boolean;
  onTestRun: () => void;
}) {
  const { t } = useT();
  const kind = entity.kind as EntityKind;
  const look = KIND_LOOK[kind];
  const run = runAction(entity);
  return (
    <tr>
      <td>
        <span className={`badge bg-${look?.color ?? "secondary"}-lt d-inline-flex align-items-center gap-1`}>
          {look?.icon}
          {t(KIND_LABELS[kind]?.one ?? entity.kind)}
        </span>
      </td>
      <td>
        <a href={configHref(entity)} className="fw-medium">
          {entity.name}
        </a>
        {entity.description && (
          <div className="text-secondary small">{entity.description}</div>
        )}
      </td>
      <td>
        {run?.type === "link" && (
          <a
            href={run.href}
            {...(run.external ? { target: "_blank", rel: "noreferrer" } : {})}
          >
            {t(run.label)}
          </a>
        )}
        {run?.type === "test-run" && (
          <button type="button" className="btn btn-link p-0" disabled={running} onClick={onTestRun}>
            {running ? <T text="Running…" /> : t(run.label)}
          </button>
        )}
      </td>
      <td>
        <div className="d-flex flex-wrap align-items-center gap-1">
          {entity.details.map((detail, n) =>
            n === 0 ? (
              <span key={n} className="badge bg-secondary-lt">{detail}</span>
            ) : (
              <span key={n} className="text-secondary small">{detail}</span>
            ),
          )}
          {showAge && entity.updated_at && (
            <span
              className="badge bg-muted-lt"
              title={new Date(entity.updated_at).toLocaleString()}
            >
              {timeAgo(entity.updated_at)}
            </span>
          )}
        </div>
      </td>
      <td>{entity.access}</td>
    </tr>
  );
}
