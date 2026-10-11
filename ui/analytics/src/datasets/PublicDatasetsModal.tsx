// "Get datasets" (beside Create on the front page's Datasets card): well-known
// open datasets, each downloaded from its publisher when asked, made into
// tables and opened as a dataset (`sc_api::public_datasets`). An install runs
// on the server as a job; the picker polls it, shows how far it has got, and
// opens the dataset the admin asked for when it is done.

import { useCallback, useEffect, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import ButtonGroup from "react-bootstrap/ButtonGroup";
import Form from "react-bootstrap/Form";
import ListGroup from "react-bootstrap/ListGroup";
import Modal from "react-bootstrap/Modal";
import ProgressBar from "react-bootstrap/ProgressBar";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { useAnnounce } from "../panes";
import {
  CATEGORIES,
  categoryColour,
  categoryName,
  formatBytes,
  jobPercent,
  jobText,
  matches,
  type Category,
  type InstallJob,
  type PublicDataset,
} from "./publicDatasets";

/** How often a running install is asked how it is going. */
const POLL_MS = 1000;

export function PublicDatasetsModal({
  show,
  onHide,
  onOpen,
}: {
  show: boolean;
  onHide: () => void;
  onOpen: (datasetId: string) => void;
}) {
  const { t, locale } = useT();
  const changed = useAnnounce();
  const [entries, setEntries] = useState<PublicDataset[] | null>(null);
  const [jobs, setJobs] = useState<Record<string, InstallJob>>({});
  const [error, setError] = useState<string | null>(null);
  const [category, setCategory] = useState<Category | "all">("all");
  const [query, setQuery] = useState("");
  // The dataset asked for last: opened when its install succeeds.
  const wanted = useRef<string | null>(null);

  const load = useCallback(async () => {
    try {
      const list = await api.listPublicDatasets();
      setEntries(list);
      // An install started earlier (another tab, before a reload) is picked
      // up where it is.
      setJobs((current) => {
        const next = { ...current };
        for (const e of list) if (e.job && !next[e.key]) next[e.key] = e.job;
        return next;
      });
    } catch (err) {
      setError(errorMessage(err, t("Could not load the public datasets.")));
    }
  }, [t]);

  useEffect(() => {
    if (show) void load();
  }, [show, load]);

  const running = Object.values(jobs).filter((j) => j.status === "running");
  const runningKeys = running.map((j) => j.key).join(",");

  // Ask each running install how it is going until none is.
  useEffect(() => {
    if (!runningKeys) return;
    const timer = window.setTimeout(async () => {
      for (const key of runningKeys.split(",")) {
        try {
          const job = await api.getPublicDatasetInstall(key);
          setJobs((current) => ({ ...current, [key]: job }));
          if (job.status === "succeeded" && job.dataset_id) {
            changed("dataset", job.dataset_id);
            void load();
            if (wanted.current === key) {
              wanted.current = null;
              onOpen(job.dataset_id);
            }
          }
        } catch (err) {
          setError(errorMessage(err, t("Could not check on the download.")));
        }
      }
    }, POLL_MS);
    return () => window.clearTimeout(timer);
  }, [runningKeys, jobs, changed, load, onOpen, t]);

  const get = async (entry: PublicDataset) => {
    setError(null);
    try {
      const job = await api.installPublicDataset(entry.key);
      wanted.current = entry.key;
      setJobs((current) => ({ ...current, [entry.key]: job }));
    } catch (err) {
      setError(errorMessage(err, t("Could not start the download.")));
    }
  };

  const shown = (entries ?? []).filter((e) => matches(e, category, query));

  return (
    <Modal show={show} onHide={onHide} size="xl" scrollable>
      <Modal.Header closeButton>
        <Modal.Title>
          <T text="Get datasets" />
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <p className="text-secondary">
          <T text="Well-known open datasets. Getting one downloads it from its publisher, creates its tables in this database and opens it as a dataset. Each comes under its publisher's licence: credit them as they ask when you use it." />
        </p>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}
        <div className="d-flex flex-wrap gap-2 mb-3">
          <ButtonGroup size="sm" aria-label={t("Kind of data")}>
            {(["all", ...CATEGORIES] as const).map((c) => (
              <Button
                key={c}
                variant={category === c ? "primary" : "outline-secondary"}
                onClick={() => setCategory(c)}
              >
                {c === "all" ? t("All") : categoryName(c, t)}
              </Button>
            ))}
          </ButtonGroup>
          <Form.Control
            size="sm"
            className="ms-auto"
            style={{ maxWidth: "16rem" }}
            type="search"
            value={query}
            placeholder={t("Search")}
            aria-label={t("Search the datasets")}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
        {entries === null && !error && <Spinner animation="border" size="sm" />}
        {entries !== null && shown.length === 0 && (
          <p className="text-secondary">
            <T text="No dataset matches." />
          </p>
        )}
        <ListGroup variant="flush">
          {shown.map((entry) => (
            <PublicDatasetItem
              key={entry.key}
              entry={entry}
              job={jobs[entry.key] ?? null}
              locale={locale}
              onGet={() => void get(entry)}
              onOpen={onOpen}
            />
          ))}
        </ListGroup>
      </Modal.Body>
    </Modal>
  );
}

function PublicDatasetItem({
  entry,
  job,
  locale,
  onGet,
  onOpen,
}: {
  entry: PublicDataset;
  job: InstallJob | null;
  locale: string;
  onGet: () => void;
  onOpen: (datasetId: string) => void;
}) {
  const { t } = useT();
  const isRunning = job?.status === "running";
  const percent = job ? jobPercent(job) : null;
  const datasetId = entry.dataset_id ?? (job?.status === "succeeded" ? job.dataset_id : null);

  let action;
  if (isRunning) {
    action = (
      <Button size="sm" variant="primary" disabled>
        <Spinner animation="border" size="sm" className="me-1" />
        <T text="Getting…" />
      </Button>
    );
  } else if (entry.installed && datasetId) {
    action = (
      <Button size="sm" variant="outline-primary" onClick={() => onOpen(datasetId)}>
        <T text="Open" />
      </Button>
    );
  } else if (entry.installed) {
    // The tables are here and the dataset was deleted: getting it again makes
    // the dataset and downloads nothing.
    action = (
      <Button size="sm" variant="outline-primary" onClick={onGet}>
        <T text="Make a dataset" />
      </Button>
    );
  } else {
    action = (
      <Button
        size="sm"
        variant="primary"
        disabled={Boolean(entry.unavailable)}
        title={entry.unavailable ?? undefined}
        onClick={onGet}
      >
        <T text="Get" />
      </Button>
    );
  }

  return (
    <ListGroup.Item className="px-0" data-public-dataset={entry.key}>
      <div className="d-flex gap-3 align-items-start">
        <div className="flex-grow-1" style={{ minWidth: 0 }}>
          <div className="d-flex flex-wrap align-items-center gap-2 mb-1">
            <strong>{entry.title}</strong>
            <Badge bg={categoryColour(entry.category)}>{categoryName(entry.category, t)}</Badge>
            {entry.installed && (
              <Badge bg="success-lt">
                <T text="In this database" />
              </Badge>
            )}
          </div>
          <div className="small mb-1">{entry.description}</div>
          <div className="small text-secondary">
            {entry.tables.length === 1
              ? t("{rows} rows in {table}", {
                  rows: entry.rows.toLocaleString(locale),
                  table: entry.tables[0] ?? "",
                })
              : t("{rows} rows in {count} tables: {tables}", {
                  rows: entry.rows.toLocaleString(locale),
                  count: entry.tables.length,
                  tables: entry.tables.join(", "),
                })}
            {" · "}
            {t("{size} download", { size: formatBytes(entry.download_bytes, locale) })}
          </div>
          <div className="small text-secondary">
            <a href={entry.licence_url} target="_blank" rel="noreferrer">
              {entry.licence}
            </a>
            {" · "}
            <a href={entry.homepage} target="_blank" rel="noreferrer">
              <T text="About the data" />
            </a>
            {" · "}
            <T text="Credit: {credit}" args={{ credit: entry.attribution }} />
          </div>
          {entry.unavailable && !entry.installed && (
            <div className="small text-warning mt-1">{entry.unavailable}</div>
          )}
          {isRunning && job && (
            <div className="mt-2">
              <div className="small text-secondary mb-1">{jobText(job, t, locale)}</div>
              <ProgressBar
                now={percent ?? 100}
                animated={percent === null}
                striped={percent === null}
                aria-label={jobText(job, t, locale)}
              />
            </div>
          )}
          {job?.status === "failed" && (
            <Alert variant="danger" className="small mt-2 mb-0 py-2">
              {job.error}
            </Alert>
          )}
        </div>
        <div className="text-nowrap">{action}</div>
      </div>
    </ListGroup.Item>
  );
}
