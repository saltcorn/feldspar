// The dialog a move of the admin UI opens (Settings → Development → Admin
// subdomain): it says where the admin UI now is, shows the new address
// becoming ready, and only then offers to go there — signed in, by way of a
// one-time handoff. The logic it shows is `adminAddress.ts`'s.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import { answers, moveSteps, originOf, readyToFollow, type MoveStep } from "../adminAddress";
import type { GetAdminAddressResponse } from "../client";
import { IconAlertTriangle, IconCheck } from "../icons";
import { AlertBody } from "../layout";
import { T, useT } from "../i18n";

/** How often the dialog asks the server, and then the new address, again. */
const POLL_MS = 2000;

export function AdminMove({ onClose }: { onClose: () => void }) {
  const { t } = useT();
  const [address, setAddress] = useState<GetAdminAddressResponse | null>(null);
  const [reachable, setReachable] = useState<boolean | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [following, setFollowing] = useState(false);

  const steps = moveSteps(address, reachable, t);
  const ready = readyToFollow(steps);
  const host = address?.admin_host ?? null;

  // Ask until everything is done: the server for the certificate, then the new
  // address itself from this browser. Stops once there is nothing left to wait
  // for, and when the dialog closes.
  useEffect(() => {
    if (ready) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = async () => {
      try {
        const next = await api.getAdminAddress();
        if (stopped) return;
        setAddress(next);
        setError(null);
        const certificateDone =
          moveSteps(next, null).find((s) => s.id === "certificate")?.state === "done";
        if (certificateDone && next.admin_host) {
          const ok = await answers(originOf(next.admin_host, window.location));
          if (stopped) return;
          setReachable(ok);
          if (ok) return;
        }
      } catch (e) {
        if (stopped) return;
        setError(e instanceof Error ? e.message : String(e));
      }
      timer = setTimeout(() => void tick(), POLL_MS);
    };
    void tick();
    return () => {
      stopped = true;
      if (timer) clearTimeout(timer);
    };
  }, [ready]);

  const follow = async () => {
    setFollowing(true);
    setError(null);
    try {
      const handoff = await api.createAdminHandoff();
      window.location.assign(`${originOf(handoff.host, window.location)}${handoff.path}`);
    } catch (e) {
      setFollowing(false);
      setError(e instanceof Error ? e.message : t("Could not move the session."));
    }
  };

  return (
    <Modal show onHide={onClose} centered backdrop="static">
      <Modal.Header>
        <Modal.Title>
          <T text="The admin UI is moving" />
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <p>
          {host ? (
            <T
              text="You will now be directed to {address}. It can be followed once it has a certificate and answers this browser; you stay signed in."
              values={{
                address: <strong>{originOf(host, window.location)}</strong>,
              }}
            />
          ) : (
            <T text="You will now be directed to the new address of the admin UI." />
          )}
        </p>
        <ul className="list-unstyled mb-0">
          {steps.map((step) => (
            <Step key={step.id} step={step} />
          ))}
        </ul>
        {error && (
          <Alert variant="danger" className="mt-3 mb-0">
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
      </Modal.Body>
      <Modal.Footer>
        {/* The old address keeps serving the admin UI until the new one is
            ready, so staying is a real choice rather than a dead end. */}
        <Button variant="secondary" onClick={onClose} disabled={following}>
          <T text="Stay here for now" />
        </Button>
        <Button onClick={() => void follow()} disabled={!ready || following}>
          {following ? (
            <Spinner animation="border" size="sm" role="status" />
          ) : host ? (
            <T text="Go to {host}" values={{ host }} />
          ) : (
            <T text="Go to the new address" />
          )}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

/** One line of progress: an icon for its state, its label, and any detail. */
function Step({ step }: { step: MoveStep }) {
  const icon =
    step.state === "done" ? (
      <IconCheck className="text-success" />
    ) : step.state === "failed" ? (
      <IconAlertTriangle className="text-danger" />
    ) : step.state === "active" ? (
      <Spinner animation="border" size="sm" role="status" />
    ) : (
      <span className="d-inline-block" aria-hidden="true">
        ·
      </span>
    );
  return (
    <li className={`d-flex gap-2 mb-2${step.state === "pending" ? " text-secondary" : ""}`}>
      <span className="flex-shrink-0">{icon}</span>
      <span>
        {step.label}
        {step.detail && <div className="small text-secondary text-break">{step.detail}</div>}
      </span>
    </li>
  );
}
