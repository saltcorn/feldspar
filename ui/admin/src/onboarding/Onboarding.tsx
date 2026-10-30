// The renderer for a dialog flow (`flow.ts`): one modal over the admin shell,
// whose contents are the step the flow is on. The shell shows it with the
// first-run onboarding (`script.ts`) while `authStatus.onboarding_pending` says
// to; finishing or skipping tells the server, so it is not shown again.
//
// Every step is drawn the same way: the note (where this is done without the
// dialogs) at the top, then the text, then the step's own body — the options of
// an `ask` as a list to pick from, the settings of a `form`, the buttons of a
// `say` in the footer. A `run` step is a spinner while it works; when it fails
// the admin is put back on the dialog that led to it, with the error.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { T, useT } from "../i18n";
import { SettingsFields, type FieldSpec } from "../settings";
import {
  canGoBack,
  choose,
  currentStep,
  goBack,
  offered,
  settle,
  startFlow,
  submit,
  type Exit,
  type Flow,
  type FlowState,
} from "./flow";
import { onboardingEffects } from "./effects";
import { ONBOARDING, type OnboardingFx } from "./script";

/** Leave the dialogs for where the flow said to go. A new tab is opened here,
 * synchronously, so it is still the click that opens it. */
function leave(exit: Exit): void {
  if (exit.href) window.open(exit.href, "_blank", "noopener");
  if (exit.to) navigate(exit.to);
}

export function Onboarding({
  onClose,
  definition = ONBOARDING,
  fx = onboardingEffects,
}: {
  onClose: () => void;
  definition?: Flow<OnboardingFx>;
  fx?: OnboardingFx;
}) {
  // Not `t`: the flow's words are data, collected by the `t()` marker where
  // they are written (`flow.ts`), and translated here.
  const { t: translate } = useT();
  const [state, setState] = useState<FlowState>(() => startFlow(definition));
  const [values, setValues] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const step = currentStep(definition, state);
  const vars = state.ctx.vars;
  const say = (text: string) => translate(text, vars);

  const close = (exit: Exit = {}) => {
    // Shown once: a failure to record it means it is offered again next time,
    // which is the better of the two ways to be wrong.
    void api.finishOnboarding().catch(() => {});
    onClose();
    leave(exit);
  };

  /** Show `next`: a dialog, with a form's starting values, or the way out. */
  const move = (next: FlowState) => {
    if ("done" in next) {
      close(next.done);
      return;
    }
    const nextStep = currentStep(definition, next);
    setValues(nextStep?.kind === "form" ? (nextStep.initial?.(next.ctx) ?? {}) : {});
    setState(next);
  };

  // A `run` step has no dialog: do it, and show wherever it leads.
  useEffect(() => {
    if (step?.kind !== "run") return;
    let live = true;
    settle(definition, state, fx).then(
      (next) => live && move(next),
      (err) => {
        if (!live) return;
        setError(errorMessage(err, err instanceof Error ? err.message : "Something went wrong."));
        setState(goBack(state));
      },
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);

  const pick = (index: number) => {
    setError(null);
    move(choose(definition, state, index));
  };

  const send = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      move(await submit(definition, state, values, fx));
    } catch (err) {
      setError(errorMessage(err, err instanceof Error ? err.message : "Something went wrong."));
    } finally {
      setSubmitting(false);
    }
  };

  const back = () => {
    setError(null);
    move(goBack(state));
  };

  const busy = step?.kind === "run" || submitting;
  const fields: FieldSpec[] =
    step?.kind === "form"
      ? step.fields(state.ctx, values).map((field) => ({
          ...field,
          label: say(field.label),
          sublabel: field.sublabel && say(field.sublabel),
        }))
      : [];

  return (
    <Modal show centered size="lg" backdrop="static" onHide={() => close()}>
      <Modal.Header closeButton>
        <Modal.Title>{step && step.kind !== "run" ? say(step.title) : <T text="Welcome to Saltcorn" />}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {step?.kind === "run" ? (
          <div className="text-center py-4" role="status">
            <Spinner animation="border" />
            <div className="mt-3 text-secondary">{step.busy ? say(step.busy) : <T text="Working…" />}</div>
          </div>
        ) : (
          step && (
            <>
              {step.note && <Alert variant="info">{say(step.note)}</Alert>}
              {error && <Alert variant="danger">{error}</Alert>}
              {step.text && <p>{say(step.text)}</p>}
              {step.kind === "ask" && (
                <div className="list-group">
                  {offered(step, state.ctx).map((option, index) => (
                    <button
                      key={option.label}
                      type="button"
                      className="list-group-item list-group-item-action py-3"
                      onClick={() => pick(index)}
                    >
                      <div className="fw-bold">{say(option.label)}</div>
                      {option.description && (
                        <div className="text-secondary">{say(option.description)}</div>
                      )}
                    </button>
                  ))}
                </div>
              )}
              {step.kind === "form" && (
                <Form id="onboarding-form" onSubmit={send}>
                  <fieldset disabled={submitting}>
                    <SettingsFields
                      spec={fields}
                      values={values}
                      onChange={(name, value) => setValues((v) => ({ ...v, [name]: value }))}
                      idPrefix="onboarding"
                    />
                  </fieldset>
                </Form>
              )}
            </>
          )
        )}
      </Modal.Body>
      <Modal.Footer className="justify-content-between">
        <div className="d-flex gap-2">
          {canGoBack(state) && (
            <Button variant="outline-secondary" onClick={back} disabled={busy}>
              <T text="Back" />
            </Button>
          )}
          <Button variant="link" onClick={() => close()} disabled={submitting}>
            <T text="Skip for now" />
          </Button>
        </div>
        <div className="d-flex gap-2">
          {step?.kind === "say" &&
            offered(step, state.ctx).map((button, index) => (
              <Button
                key={button.label}
                variant={button.variant ?? (index === 0 ? "primary" : "secondary")}
                onClick={() => pick(index)}
              >
                {say(button.label)}
              </Button>
            ))}
          {step?.kind === "form" && (
            <Button type="submit" form="onboarding-form" disabled={submitting}>
              {submitting ? <T text="Working…" /> : say(step.submitLabel ?? "Continue")}
            </Button>
          )}
        </div>
      </Modal.Footer>
    </Modal>
  );
}
