// A fit while it runs (analytics TODO A3.5): its stage, a progress bar per
// chain for a posterior, and Cancel — every fit can be cancelled now (A3.3);
// one whose provider cannot stop mid-call stops at its next stage. What it
// shows comes from the fit's progress socket (`progress.ts`).

import { useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import ProgressBar from "react-bootstrap/ProgressBar";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { chainPercent } from "./models";
import { stageText, type LiveProgress } from "./progress";

export function FitRunning({ instance, live }: { instance: string; live: LiveProgress | null }) {
  const { t } = useT();
  const [asked, setAsked] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const progress = live?.progress ?? null;
  const stopping = asked || Boolean(live?.cancelRequested);

  const cancel = async () => {
    setError(null);
    try {
      await api.cancelModelFit(instance);
      setAsked(true);
    } catch (err) {
      setError(errorMessage(err, t("Could not cancel the fit.")));
    }
  };

  return (
    <Card className="mb-3" data-fit-running={instance}>
      <Card.Header className="d-flex align-items-center gap-2">
        <Spinner animation="border" size="sm" />
        <span>{progress ? stageText(t, progress.stage) : t("starting")}</span>
        <Button size="sm" variant="outline-danger" className="ms-auto" disabled={stopping} onClick={() => void cancel()}>
          {stopping ? <T text="Stopping…" /> : <T text="Cancel" />}
        </Button>
      </Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {progress && progress.chains.length > 0 ? (
          progress.chains.map((c) => (
            <div className="mb-2" key={c.chain}>
              <div className="d-flex small text-secondary">
                <span>{t("Chain {chain}", { chain: c.chain })}</span>
                <span className="ms-auto">
                  {c.phase === "warmup"
                    ? t("warmup, iteration {i} of {n}", { i: c.iteration, n: c.total })
                    : t("sampling, iteration {i} of {n}", { i: c.iteration, n: c.total })}
                </span>
              </div>
              <ProgressBar
                now={chainPercent(c)}
                variant={c.phase === "warmup" ? "secondary" : "primary"}
                aria-label={t("Chain {chain}", { chain: c.chain })}
              />
            </div>
          ))
        ) : (
          <p className="text-muted mb-0">
            {progress?.stage === "compiling" ? (
              <T text="Compiling the program — a C++ compile, a minute or so, once per program: the next fit of the same program starts sampling at once." />
            ) : progress?.stage === "queued" ? (
              <T text="Waiting for the server's process budget: other fits' chains are running." />
            ) : (
              <T text="The fit is running on the server, which sends its progress here as it changes. Nothing survives a restart: a fit still running when the server stops is failed at boot." />
            )}
          </p>
        )}
      </Card.Body>
    </Card>
  );
}
