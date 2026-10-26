// Create / edit an agent: a provider, a prompt, and the traits it is given.
//
// The trait picker is the part worth reading. A trait declares its
// configuration as `FormField`s and `SettingsFields` renders it, so **no screen
// knows a specific trait** — `query_table`'s table box and `run_trigger`'s
// trigger box are the same code, and a trait added by a plugin gets a working
// form with no change here.
//
// The one thing that is not the file-store/trigger form's shape: a trait may be
// enabled **more than once** (§11.2). "Query books" and "query orders" are one
// trait with two configurations, so the enabled traits are a *list* the admin
// adds to, each entry carrying its own settings — and the order is the order the
// tools are offered to the model in.
//
// One exception to "no screen knows a trait", and it is presentation only: the
// `coding` trait's shell settings are drawn as a group of their own, with a
// warning when the shell has no sandbox, because that grant is every other
// grant at once and deserves not to look like one more checkbox (`agentForm.ts`).

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type {
  CreateAgentRequest,
  ListAgentTraitsResponse,
  ListAgentsResponse,
  ListLlmProvidersResponse,
} from "../client";
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { OptionalRoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import { modelOptions, type ModelItem } from "../llmModels";
import {
  BUDGETS,
  CREDITS,
  NUMBER_ATTRIBUTES,
  REASONING_EFFORTS,
  ROLES,
  agentAttributes,
  readEffort,
  readNumbers,
  readRoles,
  shellWarning,
  splitShellSettings,
  type RoleChoice,
  type RoleKey,
} from "../agentForm";
import { SettingsFields, buildConfig, readConfig, type FieldSpec } from "../settings";
import { T, useT } from "../i18n";

type TraitInfo = ListAgentTraitsResponse[number];
type AgentItem = ListAgentsResponse[number];
type ProviderItem = ListLlmProvidersResponse[number];

/** One enabled trait as the form holds it: which trait, and its settings as the
 * strings the controls edit. */
type Enabled = { trait: string; config: Record<string, string> };

export function AgentForm({ agentId }: { agentId?: string }) {
  const { t } = useT();
  const roles = useRoles();
  const [traits, setTraits] = useState<TraitInfo[] | null>(null);
  const [providers, setProviders] = useState<ProviderItem[]>([]);
  const [models, setModels] = useState<ModelItem[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [provider, setProvider] = useState("");
  const [model, setModel] = useState("");
  const [systemPrompt, setSystemPrompt] = useState("");
  const [minRole, setMinRole] = useState<number | null>(null);
  const [enabled, setEnabled] = useState<Enabled[]>([]);
  // What the agent had, so keys this form does not show survive a save.
  const [storedAttributes, setStoredAttributes] = useState<unknown>({});
  const [numbers, setNumbers] = useState<Record<string, string>>({});
  const [effort, setEffort] = useState("");
  const [modelRoles, setModelRoles] = useState<Record<RoleKey, RoleChoice>>(readRoles({}));
  const [adding, setAdding] = useState("");

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const [traitList, providerList] = await Promise.all([
          api.listAgentTraits(),
          api.listLlmProviders(),
        ]);
        let existing: AgentItem | undefined;
        if (agentId) {
          existing = (await api.listAgents()).find((a) => a.id === agentId);
          if (!existing) {
            if (!cancelled) setLoadError("That agent no longer exists.");
            return;
          }
        }
        if (cancelled) return;
        setTraits(traitList);
        setProviders(providerList);
        setAdding(traitList[0]?.name ?? "");
        if (existing) {
          setName(existing.name);
          setDescription(existing.description);
          setProvider(existing.provider);
          setModel(existing.model ?? "");
          setSystemPrompt(existing.system_prompt);
          setMinRole(existing.min_role ?? null);
          setEnabled(
            existing.traits.map((t) => ({
              trait: t.trait,
              config: readConfig(t.config),
            })),
          );
          setStoredAttributes(existing.attributes);
          setNumbers(readNumbers(existing.attributes));
          setEffort(readEffort(existing.attributes));
          setModelRoles(readRoles(existing.attributes));
        } else {
          // A new agent starts on the installation's default provider.
          const preferred = providerList.find((p) => p.is_default) ?? providerList[0];
          setProvider(preferred?.name ?? "");
        }
      } catch {
        if (!cancelled) setLoadError("Could not load the traits and providers.");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  // The chosen provider's models, for the model pick-list. Fetched again when
  // the provider changes; a provider that is missing has none to offer.
  useEffect(() => {
    const chosen = providers.find((p) => p.name === provider);
    if (!chosen) {
      setModels([]);
      return;
    }
    let cancelled = false;
    api
      .listLlmModels(chosen.id)
      .then((list) => {
        if (!cancelled) setModels(list);
      })
      .catch(() => {
        if (!cancelled) setModels([]);
      });
    return () => {
      cancelled = true;
    };
  }, [providers, provider]);

  const specOf = (trait: string) => traits?.find((t) => t.name === trait)?.config_spec ?? [];

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const body: CreateAgentRequest = {
        name: name.trim(),
        description: description.trim(),
        provider,
        model: model.trim() === "" ? null : model.trim(),
        system_prompt: systemPrompt,
        traits: enabled.map((entry) => ({
          trait: entry.trait,
          config: buildConfig(specOf(entry.trait), entry.config),
        })),
        min_role: minRole,
        // The sparse attributes: what was typed, nothing that was not, and
        // every stored key this form has no box for.
        attributes: agentAttributes(storedAttributes, numbers, modelRoles, effort),
      };
      if (agentId) {
        await api.updateAgent(agentId, body);
      } else {
        await api.createAgent(body);
      }
      navigate("/agents");
    } catch (err) {
      // The server's own refusal — "no LLM provider named `house`", "its tool
      // `query_books` has the same name as one from trait 1" — is the message
      // that says what to fix.
      setError(errorMessage(err, "Could not save the agent."));
    } finally {
      setBusy(false);
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!traits) {
    return (
      <PageBody>
        <div className="py-5 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle="Agents"
        title={agentId ? "Edit agent" : "New agent"}
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/agents")}>
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {providers.length === 0 && (
          <Alert variant="warning">
            <T text="No LLM providers are configured, so this agent will have nothing to talk to." />{" "}
            <Alert.Link href="#/llm-providers/new"><T text="Connect one first." /></Alert.Link>
          </Alert>
        )}

        <Form onSubmit={(e) => void submit(e)}>
          <Row>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="agentName">
                <Form.Label>
                  <T text="Name" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control value={name} required onChange={(e) => setName(e.target.value)} />                
              </Form.Group>
            </Col>
            <Col md={6}>
              <OptionalRoleSelect
                id="agentMinRole"
                label={t("Minimum role")}
                value={minRole}
                roles={roles}
                blank="Admin only"
                onChange={setMinRole}
              >
              </OptionalRoleSelect>
            </Col>
          </Row>

          <Form.Group className="mb-3" controlId="agentDescription">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </Form.Group>

          <Card className="mb-3">
            <Card.Header><T text="Model" /></Card.Header>
            <Card.Body>
              <Row>
                <Col md={6}>
                  <Form.Group className="mb-3" controlId="agentProvider">
                    <Form.Label>
                      <T text="LLM provider" /><span className="text-danger"> *</span>
                    </Form.Label>
                    <Form.Select
                      value={provider}
                      onChange={(e) => {
                        setProvider(e.target.value);
                        // A model name belongs to one provider; another
                        // provider's default is the safe starting point.
                        setModel("");
                      }}
                    >
                      {/* A provider that was deleted out from under a saved
                          agent still has to be shown, or saving this form would
                          silently repoint the agent at another one. */}
                      {providers.every((p) => p.name !== provider) && provider !== "" && (
                        <option value={provider}>
                          {t("{name} (missing)", { name: provider })}
                        </option>
                      )}
                      {providers.map((p) => (
                        <option key={p.id} value={p.name}>
                          {p.name}
                        </option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                </Col>
                <Col md={6}>
                  <Form.Group className="mb-3" controlId="agentModel">
                    <Form.Label><T text="Model" /></Form.Label>
                    <Form.Select value={model} onChange={(e) => setModel(e.target.value)}>
                      {modelOptions(models, model).map((o) => (
                        <option key={o.value} value={o.value}>
                          {o.label}
                        </option>
                      ))}
                    </Form.Select>
                    <Form.Text muted>
                      <T text="One of the provider's models. Models, their prices and the default are set on the provider." />
                    </Form.Text>
                  </Form.Group>
                </Col>
              </Row>

              <Form.Group className="mb-3" controlId="agentPrompt">
                <Form.Label><T text="System prompt" /></Form.Label>
                <Form.Control
                  as="textarea"
                  rows={5}
                  value={systemPrompt}
                  onChange={(e) => setSystemPrompt(e.target.value)}
                />
                <Form.Text muted>
                  <T text="What the agent is told it is, before anything the conversation adds." />
                </Form.Text>
              </Form.Group>

              <Row>
                {NUMBER_ATTRIBUTES.map((attr) => (
                  <Col md={4} key={attr.key}>
                    <NumberBox
                      attr={attr}
                      value={numbers[attr.key] ?? ""}
                      onChange={(value) => setNumbers((n) => ({ ...n, [attr.key]: value }))}
                    />
                  </Col>
                ))}
                <Col md={4}>
                  <Form.Group className="mb-3" controlId="agentReasoningEffort">
                    <Form.Label><T text="Reasoning effort" /></Form.Label>
                    <Form.Select value={effort} onChange={(e) => setEffort(e.target.value)}>
                      <option value=""><T text="The model's own setting" /></option>
                      {REASONING_EFFORTS.map((level) => (
                        <option key={level} value={level}>{level}</option>
                      ))}
                    </Form.Select>
                    <Form.Text muted>
                      <T text="How much the model thinks before it answers. Lower is faster and cheaper." />
                    </Form.Text>
                  </Form.Group>
                </Col>
              </Row>
            </Card.Body>
          </Card>

          <Card className="mb-3">
            <Card.Header><T text="Roles" /></Card.Header>
            <Card.Body>
              <p className="text-muted small">
                <T text="The model above is the" /> <strong><T text="executor" /></strong><T text=", which does the work. A planned coding agent plans on the strong model and hands small jobs to the cheap one; an agent with neither set uses its own model for everything." />
              </p>
              <Row>
                {ROLES.map((role) => (
                  <Col md={6} key={role.key}>
                    <RolePicker
                      id={`agent-role-${role.key}`}
                      label={role.label}
                      help={role.help}
                      providers={providers}
                      value={modelRoles[role.key]}
                      onChange={(value) => setModelRoles((r) => ({ ...r, [role.key]: value }))}
                    />
                  </Col>
                ))}
              </Row>
            </Card.Body>
          </Card>

          <Card className="mb-3">
            <Card.Header><T text="Budgets" /></Card.Header>
            <Card.Body>
              <p className="text-muted small">
                <T text="Per run. A run that reaches one stops and says which; the conversation can be continued. Blank is no limit unless it says otherwise." />
              </p>
              <Row>
                {BUDGETS.map((attr) => (
                  <Col md={6} key={attr.key}>
                    <NumberBox
                      attr={attr}
                      value={numbers[attr.key] ?? ""}
                      onChange={(value) => setNumbers((n) => ({ ...n, [attr.key]: value }))}
                    />
                  </Col>
                ))}
              </Row>
            </Card.Body>
          </Card>

          <Card className="mb-3">
            <Card.Header><T text="LLM credits" /></Card.Header>
            <Card.Body>
              <p className="text-muted small">
                <T text="Charge the person using this agent for its model calls, from their LLM credits. A run stops when they run out. Blank rates are not metered, and an admin is never charged." />
              </p>
              <Row>
                {CREDITS.map((attr) => (
                  <Col md={6} key={attr.key}>
                    <NumberBox
                      attr={attr}
                      value={numbers[attr.key] ?? ""}
                      onChange={(value) => setNumbers((n) => ({ ...n, [attr.key]: value }))}
                    />
                  </Col>
                ))}
              </Row>
            </Card.Body>
          </Card>

          <Card className="mb-3">
            <Card.Header><T text="Traits" /></Card.Header>
            <Card.Body>
              {enabled.length === 0 && (
                <p className="text-muted">
                  <T text="No traits: this agent can talk, and can do nothing else. Each trait you add is one deliberate grant." />
                </p>
              )}

              {enabled.map((entry, index) => (
                <Card className="mb-3" key={`${entry.trait}-${index}`}>
                  <Card.Header className="d-flex align-items-center justify-content-between">
                    <div>
                      <strong>{entry.trait}</strong>
                      <div className="text-muted small">
                        {traits.find((t) => t.name === entry.trait)?.description ??
                          "This trait is not registered on this server."}
                      </div>
                    </div>
                    <Button
                      size="sm"
                      variant="outline-danger"
                      onClick={() => setEnabled((list) => list.filter((_, i) => i !== index))}
                    >
                      <T text="Remove" />
                    </Button>
                  </Card.Header>
                  <Card.Body>
                    <TraitSettings
                      trait={entry.trait}
                      spec={specOf(entry.trait)}
                      values={entry.config}
                      onChange={(key, value) =>
                        setEnabled((list) =>
                          list.map((item, i) =>
                            i === index
                              ? { ...item, config: { ...item.config, [key]: value } }
                              : item,
                          ),
                        )
                      }
                      idPrefix={`trait-${index}`}
                    />
                  </Card.Body>
                </Card>
              ))}

              <div className="d-flex gap-2 align-items-start">
                <Form.Select
                  value={adding}
                  onChange={(e) => setAdding(e.target.value)}
                  aria-label={t("Trait to add")}
                  className="w-auto"
                >
                  {traits.map((t) => (
                    <option key={t.name} value={t.name}>
                      {t.name}
                    </option>
                  ))}
                </Form.Select>
                <Button
                  variant="outline-secondary"
                  onClick={() =>
                    adding !== "" &&
                    setEnabled((list) => [...list, { trait: adding, config: {} }])
                  }
                >
                  <T text="Add trait" />
                </Button>
              </div>
              <p className="text-muted small mt-3 mb-0">
                <T text="A trait can be added more than once — one" /> <code>query_table</code> <T text="for each table it may read. Two that would produce the same tool name are refused when you save." />
              </p>
            </Card.Body>
          </Card>

          <div className="btn-list">
            <Button type="submit" disabled={busy}>
              {busy ? "Saving…" : agentId ? "Save changes" : "Create agent"}
            </Button>
          </div>
        </Form>
      </PageBody>
    </>
  );
}

/** One optional number box. */
function NumberBox({
  attr,
  value,
  onChange,
}: {
  attr: { key: string; label: string; help: string };
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <Form.Group className="mb-3" controlId={`agent-${attr.key}`}>
      <Form.Label>{attr.label}</Form.Label>
      <Form.Control
        type="number"
        step="any"
        min={0}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      <Form.Text muted>{attr.help}</Form.Text>
    </Form.Group>
  );
}

/** A role's model: a provider, blank for "the agent's own", and one of its models. */
function RolePicker({
  id,
  label,
  help,
  providers,
  value,
  onChange,
}: {
  id: string;
  label: string;
  help: string;
  providers: ProviderItem[];
  value: RoleChoice;
  onChange: (value: RoleChoice) => void;
}) {
  const { t } = useT();
  const [models, setModels] = useState<ModelItem[]>([]);
  useEffect(() => {
    const chosen = providers.find((p) => p.name === value.provider);
    if (!chosen) {
      setModels([]);
      return;
    }
    let cancelled = false;
    api
      .listLlmModels(chosen.id)
      .then((list) => {
        if (!cancelled) setModels(list);
      })
      .catch(() => {
        if (!cancelled) setModels([]);
      });
    return () => {
      cancelled = true;
    };
  }, [providers, value.provider]);

  return (
    <Form.Group className="mb-3">
      <Form.Label htmlFor={`${id}-provider`}>{label}</Form.Label>
      <div className="d-flex gap-2">
        <Form.Select
          id={`${id}-provider`}
          aria-label={`${label}: provider`}
          value={value.provider}
          onChange={(e) => onChange({ provider: e.target.value, model: "" })}
        >
          <option value=""><T text="Same as the agent" /></option>
          {providers.every((p) => p.name !== value.provider) && value.provider !== "" && (
            <option value={value.provider}>
              {t("{name} (missing)", { name: value.provider })}
            </option>
          )}
          {providers.map((p) => (
            <option key={p.id} value={p.name}>
              {p.name}
            </option>
          ))}
        </Form.Select>
        {value.provider !== "" && (
          <Form.Select
            aria-label={`${label}: model`}
            value={value.model}
            onChange={(e) => onChange({ ...value, model: e.target.value })}
          >
            {modelOptions(models, value.model).map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </Form.Select>
        )}
      </div>
      <Form.Text muted>{help}</Form.Text>
    </Form.Group>
  );
}

/** A trait's settings, with `coding`'s shell settings as a group of their own. */
function TraitSettings({
  trait,
  spec,
  values,
  onChange,
  idPrefix,
}: {
  trait: string;
  spec: FieldSpec[];
  values: Record<string, string>;
  onChange: (key: string, value: string) => void;
  idPrefix: string;
}) {
  const { own, shell } = splitShellSettings(trait, spec);
  const warning = shellWarning(trait, values);
  return (
    <>
      <SettingsFields spec={own} values={values} onChange={onChange} idPrefix={idPrefix} />
      {shell.length > 0 && (
        <fieldset className="border rounded p-3 mt-2">
          <legend className="float-none w-auto px-2 mb-0 fs-5"><T text="Shell" /></legend>
          <p className="text-muted small">
            <T text="A shell is every permission above at once. It is offered only when an admin is chatting, and it is off unless you tick it." />
          </p>
          <SettingsFields
            spec={shell}
            values={values}
            onChange={onChange}
            idPrefix={`${idPrefix}-shell`}
          />
          {warning && (
            <Alert variant="danger" className="mb-0 mt-2">
              {warning}
            </Alert>
          )}
        </fieldset>
      )}
    </>
  );
}
