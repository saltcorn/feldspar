// A Saltcorn UI view's configuration editor (TODO "Saltcorn UI" Phase 10).
//
// Not a form this screen knows: the view pattern's own `configuration_workflow`,
// one step at a time, each step's form built by the server over the
// configuration gathered so far and rendered with the settings form every other
// configurable thing here uses. Back and Next carry the answers along; Save, from
// any step, sends the whole configuration, which the server replays through
// every step and refuses naming the step and the field.
//
// A layout step opens in the builder (TODO "The builder" §9), a document of its
// own on this server. The builder starts from the *saved* configuration, so what
// the wizard has gathered is saved first; the builder's Next comes back here at
// `?step=n`, and the wizard opens there over the configuration the builder saved.
// On a server built without the builder the layout is shown as the JSON it is
// saved as, with the sentence saying why.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import {
  NO_BUILDER,
  OPEN_IN_BUILDER,
  builderViewUrl,
  configurationChanged,
  openStep,
} from "../builder";
import { useBuilderAvailable } from "../builderStatus";
import { IconArrowLeft } from "../icons";
import { AlertBody, PageBody, PageHeader } from "../layout";
import { SettingsFields } from "../settings";
import {
  STEP_SKIPPED,
  applyStep,
  layoutJson,
  nameParam,
  saveViewBody,
  stepFormValues,
  stepTitle,
  type AppItem,
  type Configuration,
  type PatternItem,
  type StepItem,
  type ViewItem,
} from "../views";
import { T, useT } from "../i18n";

export function ViewEditor({
  appId,
  name,
  initialStep = null,
}: {
  appId: string;
  name: string;
  /** The step to open at, counting from 0: the builder's way back. */
  initialStep?: number | null;
}) {
  const { t } = useT();
  const builderAvailable = useBuilderAvailable();
  const [app, setApp] = useState<AppItem | null>(null);
  const [view, setView] = useState<ViewItem | null>(null);
  const [pattern, setPattern] = useState<PatternItem | null>(null);
  const [configuration, setConfiguration] = useState<Configuration>({});
  const [step, setStep] = useState<StepItem | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  /** Open step `index` over `config`. A step v1 skips for this configuration is
   * passed over in the direction of travel, so the wizard does not stop on a
   * step with nothing to ask — unless there is nowhere further to go. */
  const open = async (target: ViewItem, index: number, config: Configuration, direction: 1 | -1) => {
    setBusy(true);
    setError(null);
    try {
      const next = await openStep(
        (at) =>
          api.viewConfigStep(appId, {
            viewpattern: target.viewpattern,
            table_name: target.table_name ?? null,
            name: target.name,
            step: at,
            context: config,
          }),
        index,
        direction,
      );
      setStep(next);
      setValues(stepFormValues(next));
    } catch (err) {
      setError(errorMessage(err, "Could not build this step of the view's configuration."));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    void (async () => {
      try {
        const [apps, found, patterns] = await Promise.all([
          api.listApplications(),
          api.getView(appId, nameParam(name)),
          // The pattern only names the steps across the top.
          api.listViewPatterns().catch(() => [] as PatternItem[]),
        ]);
        const owner = apps.find((a) => a.id === appId);
        if (!owner) {
          setLoadError("That application no longer exists.");
          return;
        }
        setApp(owner);
        setView(found);
        setPattern(patterns.find((p) => p.name === found.viewpattern) ?? null);
        const config = (found.configuration ?? {}) as Configuration;
        setConfiguration(config);
        await open(found, initialStep ?? 0, config, 1);
      } catch (err) {
        setLoadError(errorMessage(err, "Could not load the view."));
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId, name, initialStep]);

  /** The configuration with what is on screen in it. */
  const gathered = () => (step ? applyStep(configuration, step, values) : configuration);

  const go = async (direction: 1 | -1) => {
    if (!view || !step) return;
    const config = gathered();
    setConfiguration(config);
    setSaved(false);
    await open(view, step.index + direction, config, direction);
  };

  const save = async () => {
    if (!view) return;
    const config = gathered();
    setConfiguration(config);
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      setView(await api.saveView(appId, nameParam(view.name), saveViewBody(view, config)));
      setSaved(true);
    } catch (err) {
      setError(errorMessage(err, "Could not save the view."));
    } finally {
      setBusy(false);
    }
  };

  /** Save what the wizard gathered, if it differs from what is saved, and open
   * this step in the builder, which builds over the saved configuration. */
  const openInBuilder = async () => {
    if (!view || !step) return;
    const config = gathered();
    setBusy(true);
    setError(null);
    try {
      if (configurationChanged(view.configuration, config)) {
        await api.saveView(appId, nameParam(view.name), saveViewBody(view, config));
      }
      window.location.assign(builderViewUrl(appId, view.name, step.index));
    } catch (err) {
      setError(errorMessage(err, "Could not save the view before opening the builder."));
      setBusy(false);
    }
  };

  const header = (
    <PageHeader
      pretitle={app ? `${app.name} · view` : "View"}
      title={name}
      docTitle={t("{name} view", { name })}
      actions={
        <Button
          variant="outline-secondary"
          onClick={() => navigate(`/applications/${encodeURIComponent(appId)}/views`)}
        >
          <IconArrowLeft className="icon-2" />
          <T text="Views" />
        </Button>
      }
    />
  );

  if (loadError) {
    return (
      <>
        {header}
        <PageBody>
          <Alert variant="danger">{loadError}</Alert>
        </PageBody>
      </>
    );
  }
  if (!app || !view) {
    return (
      <>
        {header}
        <PageBody>
          <Spinner animation="border" role="status" />
        </PageBody>
      </>
    );
  }

  return (
    <>
      {header}
      <PageBody>
        <p className="text-muted">
          {pattern?.label || view.viewpattern}
          {view.table_name ? ` over ${view.table_name}` : ""}
        </p>
        {pattern && pattern.steps.length > 1 && (
          <ul className="nav nav-pills mb-3">
            {pattern.steps.map((label, i) => (
              <li className="nav-item" key={`${i}-${label}`}>
                <span
                  className={`nav-link${step?.index === i ? " active" : ""}`}
                  aria-current={step?.index === i ? "step" : undefined}
                >
                  {i + 1}. {label}
                </span>
              </li>
            ))}
          </ul>
        )}
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        {saved && (
          <Alert variant="success" dismissible onClose={() => setSaved(false)}>
            <T text="Saved. The view serves this configuration from its next request." />
          </Alert>
        )}

        <div className="card">
          <div className="card-header">
            <h3 className="card-title">{step ? stepTitle(step) : "Configuration"}</h3>
          </div>
          <div className="card-body">
            {!step ? (
              busy && <Spinner animation="border" role="status" />
            ) : step.skip ? (
              <p className="text-muted mb-0">{STEP_SKIPPED}</p>
            ) : step.builder ? (
              builderAvailable === false ? (
                <>
                  <Alert variant="info">{NO_BUILDER}</Alert>
                  <pre className="small mb-0">{layoutJson(configuration)}</pre>
                </>
              ) : (
                <>
                  <p className="text-muted">{OPEN_IN_BUILDER}</p>
                  <Button
                    disabled={busy || builderAvailable === null}
                    onClick={() => void openInBuilder()}
                  >
                    <T text="Open in builder" />
                  </Button>
                  <details className="mt-3">
                    <summary className="text-muted"><T text="The layout as saved (JSON)" /></summary>
                    <pre className="small mb-0 mt-2">{layoutJson(configuration)}</pre>
                  </details>
                </>
              )
            ) : (
              <>
                {step.blurb && <p className="text-muted">{step.blurb}</p>}
                {step.issues.length > 0 && (
                  <Alert variant="warning">
                    <ul className="mb-0">
                      {step.issues.map((issue) => (
                        <li key={issue}>{issue}</li>
                      ))}
                    </ul>
                  </Alert>
                )}
                {step.fields.length === 0 ? (
                  <p className="text-muted mb-0"><T text="This step has nothing to set for this view." /></p>
                ) : (
                  <SettingsFields
                    spec={step.fields}
                    values={values}
                    idPrefix="view-config"
                    onChange={(field, value) => setValues({ ...values, [field]: value })}
                  />
                )}
              </>
            )}
          </div>
          <div className="card-footer d-flex gap-2">
            <Button
              variant="outline-secondary"
              disabled={busy || !step || step.index === 0}
              onClick={() => void go(-1)}
            >
              <T text="Back" />
            </Button>
            <Button
              variant="outline-primary"
              disabled={busy || !step || step.index + 1 >= step.count}
              onClick={() => void go(1)}
            >
              <T text="Next" />
            </Button>
            <Button className="ms-auto" disabled={busy || !step} onClick={() => void save()}>
              <T text="Save view" />
            </Button>
          </div>
        </div>
      </PageBody>
    </>
  );
}
