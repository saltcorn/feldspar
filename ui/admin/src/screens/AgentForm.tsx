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

import { useEffect, useState, type FormEvent, type ReactNode } from "react";
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
import { IconArrowLeft, IconChevronDown, IconChevronRight } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { OptionalRoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import { modelOptions, type ModelItem } from "../llmModels";
import {
  AGENT_TABS,
  ANSWER_SETTINGS,
  CREDITS,
  LIMITS,
  REASONING_EFFORTS,
  ROLES,
  agentAttributes,
  agentTab,
  agentTabHref,
  chargesCredits,
  missingRequired,
  readEffort,
  readNumbers,
  readRoles,
  refusalTab,
  shellWarning,
  traitNeedsAttention,
  traitSummary,
  splitShellSettings,
  type AgentTab,
  type RoleChoice,
  type RoleKey,
} from "../agentForm";
import { AgentHeader, AgentTabs } from "./AgentHeader";
import { SettingsFields, buildConfig, readConfig, type FieldSpec } from "../settings";
import { T, useT } from "../i18n";

type TraitInfo = ListAgentTraitsResponse[number];
type AgentItem = ListAgentsResponse[number];
type ProviderItem = ListLlmProvidersResponse[number];

/** One enabled trait as the form holds it: which trait, and its settings as the
 * strings the controls edit. */
type Enabled = { trait: string; config: Record<string, string> };

export function AgentForm({ agentId, tab: rawTab }: { agentId?: string; tab?: string }) {
  const { t } = useT();
  // The tab open, from the URL, so a reload or a link opens the same one.
  // Every tab edits the one form: switching keeps what was typed.
  const tab = agentTab(rawTab);
  // The tab a failed save was about, marked until the next save.
  const [problemTab, setProblemTab] = useState<AgentTab | null>(null);
  const roles = useRoles();
  const [traits, setTraits] = useState<TraitInfo[] | null>(null);
  const [providers, setProviders] = useState<ProviderItem[]>([]);
  const [models, setModels] = useState<ModelItem[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [name, setName] = useState("");
  // The name as saved, for the page's title: it does not follow the box
  // while it is being edited.
  const [savedName, setSavedName] = useState("");
  const [description, setDescription] = useState("");
  const [provider, setProvider] = useState("");
  const [model, setModel] = useState("");
  const [systemPrompt, setSystemPrompt] = useState("");
  const [minRole, setMinRole] = useState<number | null>(null);
  const [enabled, setEnabled] = useState<Enabled[]>([]);
  // Which trait cards are open, by position in `enabled`: folded to one line
  // unless something on them needs the admin, or they were just added.
  const [openTraits, setOpenTraits] = useState<boolean[]>([]);
  // What the agent had, so keys this form does not show survive a save.
  const [storedAttributes, setStoredAttributes] = useState<unknown>({});
  const [numbers, setNumbers] = useState<Record<string, string>>({});
  const [effort, setEffort] = useState("");
  const [modelRoles, setModelRoles] = useState<Record<RoleKey, RoleChoice>>(readRoles({}));
  const [adding, setAdding] = useState("");
  // The credits group is open for an agent that charges, shut otherwise.
  const [creditsOpen, setCreditsOpen] = useState(false);

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
          setSavedName(existing.name);
          setDescription(existing.description);
          setProvider(existing.provider);
          setModel(existing.model ?? "");
          setSystemPrompt(existing.system_prompt);
          setMinRole(existing.min_role ?? null);
          const loadedTraits = existing.traits.map((t) => ({
            trait: t.trait,
            config: readConfig(t.config),
          }));
          setEnabled(loadedTraits);
          setOpenTraits(
            loadedTraits.map((entry) =>
              traitNeedsAttention(
                entry.trait,
                traitList.find((t) => t.name === entry.trait)?.config_spec ?? [],
                entry.config,
              ),
            ),
          );
          setStoredAttributes(existing.attributes);
          const stored = readNumbers(existing.attributes);
          setNumbers(stored);
          setCreditsOpen(chargesCredits(stored));
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

  // What the form holds, to tell whether it has changed since it loaded.
  const snapshot = JSON.stringify([
    name, description, provider, model, systemPrompt, minRole, enabled, numbers, effort, modelRoles,
  ]);
  const [loaded, setLoaded] = useState<string | null>(null);
  useEffect(() => {
    if (traits && loaded === null) setLoaded(snapshot);
  }, [traits, loaded, snapshot]);
  const dirty = loaded !== null && snapshot !== loaded;
  // Closing or reloading the page with unsaved edits asks first. Leaving
  // through the admin's own links does not: those change the hash, which the
  // browser does not let a page stop.
  useEffect(() => {
    if (!dirty) return;
    const warn = (e: BeforeUnloadEvent) => e.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  const specOf = (trait: string) => traits?.find((t) => t.name === trait)?.config_spec ?? [];

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const missing = missingRequired(name, provider);
    if (missing) {
      setError(t(missing.message));
      setProblemTab(missing.tab);
      navigate(agentTabHref(agentId, missing.tab).slice(1));
      return;
    }
    setBusy(true);
    setError(null);
    setProblemTab(null);
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
      setLoaded(snapshot);
      navigate("/agents");
    } catch (err) {
      // The server's own refusal — "no LLM provider named `house`", "its tool
      // `query_books` has the same name as one from trait 1" — is the message
      // that says what to fix.
      const message = errorMessage(err, "Could not save the agent.");
      setError(message);
      setProblemTab(refusalTab(message));
      // A refusal that names a trait opens its card.
      setOpenTraits((open) => enabled.map((entry, i) => open[i] || message.includes(entry.trait)));
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

  // The number boxes of one group, three or two to a row.
  const numberBoxes = (
    attrs: readonly { key: string; label: string; help: string }[],
    md: number,
  ) => (
    <Row>
      {attrs.map((attr) => (
        <Col md={md} key={attr.key}>
          <NumberBox
            attr={attr}
            value={numbers[attr.key] ?? ""}
            onChange={(value) => setNumbers((n) => ({ ...n, [attr.key]: value }))}
          />
        </Col>
      ))}
    </Row>
  );

  // In the header, so it is on every tab; it submits the form below.
  const save = (
    <Button type="submit" form="agent-form" disabled={busy}>
      {busy ? t("Saving…") : agentId ? t("Save changes") : t("Create agent")}
    </Button>
  );

  return (
    <>
      {agentId ? (
        // An agent's settings share the header of its other screens.
        <AgentHeader agent={savedName} section="settings" actions={save} />
      ) : (
        <PageHeader
          title={t("New agent")}
          actions={
            <>
              <Button variant="outline-secondary" onClick={() => navigate("/agents")}>
                <IconArrowLeft className="icon-2" />
                <T text="Back" />
              </Button>
              {save}
            </>
          }
        />
      )}
      <PageBody>
        {agentId && savedName && <AgentTabs agent={savedName} agentId={agentId} active="settings" />}
        {error && <Alert variant="danger">{error}</Alert>}
        {providers.length === 0 && (
          <Alert variant="warning">
            <T text="No LLM providers are configured, so this agent will have nothing to talk to." />{" "}
            <Alert.Link href="#/llm-providers/new"><T text="Connect one first." /></Alert.Link>
          </Alert>
        )}

        {/* The tabs follow the questions asked of an agent: who it is and
            which models it runs on, what it is told, what it may do, and how
            far one run may go. A trait's settings can run long (`coding`'s fill
            a screen), which is why they have a tab of their own. */}
        <ul className="nav nav-underline mb-3" aria-label={t("Settings")}>
          {AGENT_TABS.map((item) => (
            <li className="nav-item" key={item.id}>
              <a
                className={`nav-link${item.id === tab ? " active" : ""}`}
                aria-current={item.id === tab ? "page" : undefined}
                href={agentTabHref(agentId, item.id)}
              >
                {t(item.label)}
                {item.id === "traits" && enabled.length > 0 && (
                  <span className="badge bg-secondary-lt ms-2">{enabled.length}</span>
                )}
                {item.id === problemTab && (
                  <span
                    className="badge bg-danger ms-2 p-1"
                    role="img"
                    aria-label={t("Needs attention")}
                    title={t("Needs attention")}
                  />
                )}
              </a>
            </li>
          ))}
        </ul>

        <Form id="agent-form" onSubmit={(e) => void submit(e)}>
          {tab === "general" && (
          <>
          <Card className="mb-3">
            <Card.Header><T text="General" /></Card.Header>
            <Card.Body>
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
                    label={t("Who may chat")}
                    value={minRole}
                    roles={roles}
                    blank="Admin only"
                    onChange={setMinRole}
                  >
                  </OptionalRoleSelect>
                </Col>
              </Row>
              <Form.Group controlId="agentDescription">
                <Form.Label><T text="Description" /></Form.Label>
                <Form.Control
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </Form.Group>
            </Card.Body>
          </Card>

          {/* The main model and its two alternatives side by side, in one
              shape, so the strong and cheap ones read as optional stand-ins
              for the main one rather than as settings of their own. */}
          <Card className="mb-3">
            <Card.Header><T text="Models" /></Card.Header>
            <Card.Body>
              <ModelRow
                id="agent-main"
                label={t("Main model")}
                help={t("Does the work.")}
                required
              >
                <Form.Select
                  id="agent-main-provider"
                  aria-label={t("Main model: provider")}
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
                <Form.Select
                  aria-label={t("Main model: model")}
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                >
                  {modelOptions(models, model).map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </Form.Select>
              </ModelRow>
              {ROLES.map((role) => (
                <RolePicker
                  key={role.key}
                  id={`agent-role-${role.key}`}
                  label={t(role.label)}
                  help={t(role.help)}
                  providers={providers}
                  value={modelRoles[role.key]}
                  onChange={(value) => setModelRoles((r) => ({ ...r, [role.key]: value }))}
                />
              ))}
              <p className="text-muted small">
                <T text="A planned coding agent plans on the strong model and hands small jobs to the cheap one. Left on the main model, they cost nothing extra." />
              </p>

              <h4 className="mt-4 mb-3"><T text="How it answers" /></h4>
              <Row>
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
                {ANSWER_SETTINGS.map((attr) => (
                  <Col md={4} key={attr.key}>
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

          </>
          )}

          {tab === "instructions" && (
          <Card className="mb-3">
            <Card.Header><T text="Instructions" /></Card.Header>
            <Card.Body>
              <Form.Group controlId="agentPrompt">
                <Form.Label><T text="System prompt" /></Form.Label>
                <Form.Control
                  as="textarea"
                  rows={12}
                  // The tab is the prompt's alone: it takes the window's height.
                  style={{ minHeight: "60vh" }}
                  value={systemPrompt}
                  onChange={(e) => setSystemPrompt(e.target.value)}
                />
                <Form.Text muted>
                  <T text="What the agent is told it is, before anything the conversation adds." />
                </Form.Text>
              </Form.Group>
            </Card.Body>
          </Card>

          )}

          {tab === "traits" && (
          <Card className="mb-3">
            <Card.Header><T text="What it can do" /></Card.Header>
            <Card.Body>
              {enabled.length === 0 && (
                <p className="text-muted">
                  <T text="No traits: this agent can talk, and can do nothing else. Each trait you add is one deliberate grant." />
                </p>
              )}

              {enabled.map((entry, index) => {
                const spec = specOf(entry.trait);
                const open = openTraits[index] ?? true;
                const description =
                  traits.find((t) => t.name === entry.trait)?.description ??
                  t("This trait is not registered on this server.");
                const summary = traitSummary(spec, entry.config);
                const bodyId = `trait-${index}-settings`;
                return (
                  <Card className="mb-3" key={`${entry.trait}-${index}`}>
                    <Card.Header className="d-flex align-items-center gap-2">
                      {/* The whole title is the toggle, as the credits card's is. */}
                      <button
                        type="button"
                        className="btn btn-link text-reset text-start text-decoration-none p-0 d-flex align-items-start justify-content-start gap-2 flex-fill overflow-hidden"
                        style={{ minWidth: 0 }}
                        aria-expanded={spec.length > 0 ? open : undefined}
                        aria-controls={spec.length > 0 && open ? bodyId : undefined}
                        disabled={spec.length === 0}
                        onClick={() =>
                          setOpenTraits((list) => {
                            const next = enabled.map((_, i) => list[i] ?? true);
                            next[index] = !open;
                            return next;
                          })
                        }
                      >
                        <span className="mt-1" aria-hidden="true">
                          {spec.length === 0 ? (
                            <span className="d-inline-block" style={{ width: "1rem" }} />
                          ) : open ? (
                            <IconChevronDown className="icon-2" />
                          ) : (
                            <IconChevronRight className="icon-2" />
                          )}
                        </span>
                        <span className="overflow-hidden" style={{ minWidth: 0 }}>
                          <strong>{entry.trait}</strong>
                          <span className="d-block text-muted small fw-normal text-truncate">
                            {!open && summary ? summary : description}
                          </span>
                        </span>
                      </button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        className="flex-shrink-0"
                        onClick={() => {
                          setEnabled((list) => list.filter((_, i) => i !== index));
                          setOpenTraits((list) => list.filter((_, i) => i !== index));
                        }}
                      >
                        <T text="Remove" />
                      </Button>
                    </Card.Header>
                    {open && spec.length > 0 && (
                      <Card.Body id={bodyId}>
                        <TraitSettings
                          trait={entry.trait}
                          spec={spec}
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
                    )}
                  </Card>
                );
              })}

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
                  onClick={() => {
                    if (adding === "") return;
                    setEnabled((list) => [...list, { trait: adding, config: {} }]);
                    // A trait just added opens, to be filled in.
                    setOpenTraits((list) => [...enabled.map((_, i) => list[i] ?? true), true]);
                  }}
                >
                  <T text="Add trait" />
                </Button>
              </div>
              <p className="text-muted small mt-3 mb-0">
                <T text="A trait can be added more than once — one" /> <code>query_table</code> <T text="for each table it may read. Two that would produce the same tool name are refused when you save." />
              </p>
            </Card.Body>
          </Card>

          )}

          {tab === "limits" && (
          <>
          <Card className="mb-3">
            <Card.Header><T text="Limits per run" /></Card.Header>
            <Card.Body>
              <p className="text-muted small">
                <T text="A run that reaches one stops and says which; the conversation can be continued. Blank is no limit unless it says otherwise." />
              </p>
              {numberBoxes(LIMITS, 4)}
            </Card.Body>
          </Card>

          <Card className="mb-3">
            <Card.Header>
              <button
                type="button"
                className="btn btn-link p-0 text-reset text-decoration-none d-flex align-items-center gap-2"
                aria-expanded={creditsOpen}
                aria-controls="agent-credits"
                onClick={() => setCreditsOpen((open) => !open)}
              >
                {creditsOpen ? (
                  <IconChevronDown className="icon-2" />
                ) : (
                  <IconChevronRight className="icon-2" />
                )}
                <T text="Charging users (LLM credits)" />
              </button>
            </Card.Header>
            {creditsOpen && (
              <Card.Body id="agent-credits">
                <p className="text-muted small">
                  <T text="Charge the person using this agent for its model calls, from their LLM credits. A run stops when they run out. Blank rates are not metered, and an admin is never charged." />
                </p>
                {numberBoxes(CREDITS, 6)}
              </Card.Body>
            )}
          </Card>
          </>
          )}
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
  const { t } = useT();
  return (
    <Form.Group className="mb-3" controlId={`agent-${attr.key}`}>
      <Form.Label>{t(attr.label)}</Form.Label>
      <Form.Control
        type="number"
        step="any"
        min={0}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      <Form.Text muted>{t(attr.help)}</Form.Text>
    </Form.Group>
  );
}

/** One model of the agent: its name and purpose on the left, the provider and
 * model selects beside them. The main model and both alternatives share it. */
function ModelRow({
  id,
  label,
  help,
  required = false,
  children,
}: {
  id: string;
  label: string;
  help: string;
  required?: boolean;
  children: ReactNode;
}) {
  return (
    <Row className="mb-3 align-items-start">
      <Col md={3}>
        <Form.Label htmlFor={`${id}-provider`} className="mb-0">
          {label}
          {required && <span className="text-danger"> *</span>}
        </Form.Label>
        <div className="text-muted small">{help}</div>
      </Col>
      <Col md={9}>
        <div className="d-flex gap-2">{children}</div>
      </Col>
    </Row>
  );
}

/** A role's model: a provider, blank for "the main model", and one of its models. */
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
    <ModelRow id={id} label={label} help={help}>
      <Form.Select
        id={`${id}-provider`}
        aria-label={`${label}: provider`}
        value={value.provider}
        onChange={(e) => onChange({ provider: e.target.value, model: "" })}
      >
        <option value=""><T text="Same as the main model" /></option>
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
      {value.provider !== "" ? (
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
      ) : (
        // An empty second column keeps the three rows' selects aligned.
        <div className="w-100" aria-hidden="true" />
      )}
    </ModelRow>
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
