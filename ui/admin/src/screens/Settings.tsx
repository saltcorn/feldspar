// Settings screen: the values in `_fd_config`, rendered from their declarations —
// and, beside them, backup and restore.
//
// The screen is **tabbed**, and the tabs are **the sections the server declared**,
// plus Modules, Checklists, Backup and System. That is the whole of what this screen knows about how settings are
// grouped: a section arriving from the server gets a tab labelled with its own
// label, and nothing here names `ssl` or `email`. Backup is the exception because
// it is not a section at all — it is the pair of operations that produce and
// consume a file — and it keeps the end of the strip, where an admin expects it.
//
// A settings tab knows **nothing about any particular setting**. The server sends
// sections, each with a list of fields carrying the same declaration a file store's
// backend or an LLM provider sends (`settings.tsx`'s `FieldSpec`), plus a sentence
// of help; this renders whatever arrives and posts back what was edited. Adding a
// setting is a Rust declaration and a redeployed server — there is no matching
// change here, which is the point of declaring settings as data (§6.2, §13.5).
//
// What is worth naming because it is not obvious from the code:
//
// - **A setting saves as it is edited** (`settingsSaving.ts`): a checkbox or a
//   choice at once, a typed value when the admin leaves the box or presses
//   Enter. One save at a time, through the same `Autosaver` an application's
//   form uses, and each save sends only the settings it is about — the server
//   merges them over what it has stored.
// - **Except the settings the server holds for Apply** (`applied`): the TLS and
//   Email sections, which the server checks together — `custom` mode with no
//   certificate, an SMTP username with no password — so saving each box as it
//   was typed would be refused half-way through a correct edit; and the admin
//   subdomain, whose save moves the admin UI. Their card has an Apply button at
//   the top right, primary while the card holds unsaved changes and disabled
//   while it does not. The message a refusal shows is the server's own, because
//   "the certificate and private key do not match" is worth more than anything
//   this screen could invent.
// - **The values live in this component** rather than in each card, so an edit
//   waiting for Apply survives switching tabs.
// - **A secret arrives as the sentinel** and is sent back only when the admin
//   types over it, which is how the private key stays editable without ever
//   being sent to the browser (`SettingField` handles the input itself).
//
// Two saves are asked about first. **Share sign-in between applications**
// (Authentication): changing it ends every session, the admin's own included,
// so the switch waits for a confirmation that says so (Cancel puts it back), and
// afterwards the screen reloads into the sign-in page rather than failing its
// next call. And **the admin subdomain**: applying a new one moves this screen
// to another host, which the confirmation names before it happens.

import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { ADMIN_SUBDOMAIN, adminMoved } from "../adminAddress";
import { ApplyButton } from "../ApplyButton";
import { Autosaver, type SaveState } from "../autosave";
import { SaveStatus } from "../SaveStatus";
import type { GetSettingsResponse } from "../client";
import { AlertBody, PageBody, PageHeader } from "../layout";
import { mcpEnabled } from "../mcpTokens";
import {
  SettingField,
  buildConfig,
  initialValues,
  isShown,
  readConfig,
  type FieldSpec,
} from "../settings";
import {
  afterSave,
  appliedFields,
  autosavedFields,
  mixesModes,
  saveMode,
  unsavedNames,
  type SettingSpec,
} from "../settingsSaving";
import { AdminMove } from "./AdminMove";
import { BackupTab } from "./BackupTab";
import { CodeLibraryPanel } from "./CodeLibraries";
import { SharedTranslationsPanel } from "./SharedTranslations";
import { McpTokensPanel } from "./McpTokens";
import { ClaudeCodeStatusPanel } from "./ClaudeCodeStatus";
import { statusKey, terminalTicked } from "../claudeStatus";
import { OAuthProvidersPanel } from "./OAuthProviders";
import { ModulesTab } from "./ModulesTab";
import { BuildHostsPanel } from "./BuildHosts";
import { Checklists } from "./Checklists";
import { PythonStatusPanel } from "./PythonStatus";
import { SystemTab } from "./SystemTab";
import { TestEmail } from "./TestEmail";
import { T, useT } from "../i18n";

/** One section as the API describes it. */
type Section = GetSettingsResponse["sections"][number];

/** Every field of every section, flattened — what a field's `show_if` may
 * name, and what a save's payload is built against. */
export function allFields(sections: Section[]): SettingSpec[] {
  return sections.flatMap((section) => section.fields);
}

/** What a save sends: each setting in `spec`, with an emptied box as `null`.
 *
 * `buildConfig` drops an empty optional value, which is right where a config is
 * stored as one bag (the whole bag is replaced, so a dropped key is a cleared
 * one) and wrong here, where each setting is its own row: an omitted key would
 * mean "leave it as it was", and clearing a box would do nothing at all. `null`
 * is what the server reads as "clear this setting", returning it to its
 * declared default. */
export function settingsPayload(
  spec: FieldSpec[],
  values: Record<string, string>,
): Record<string, unknown> {
  const payload: Record<string, unknown> = { ...buildConfig(spec, values) };
  for (const field of spec) {
    if (!(field.name in payload)) payload[field.name] = null;
  }
  return payload;
}

/** The setting whose change signs everyone out
 * (`sc_config::SHARED_SESSION_COOKIE`): a session cookie already in a browser
 * keeps the scope it was given, so the server ends every session when the
 * scope changes. */
export const SHARED_SESSION_COOKIE = "shared_session_cookie";

/** Whether saving `values` over what is `stored` changes whether the session
 * cookie is shared, and so signs everyone out — the admin saving it included.
 * A checkbox never saved reads as its default, off. */
export function signsEveryoneOut(
  stored: Record<string, string>,
  values: Record<string, string>,
): boolean {
  const on = (v: string | undefined) => v === "true";
  return on(stored[SHARED_SESSION_COOKIE]) !== on(values[SHARED_SESSION_COOKIE]);
}

/** The Backup tab's identity: one of the two tabs that are not declared
 * sections, and therefore one of the two names this screen still has to hold. */
export const BACKUP_TAB = "backup";

/** The Modules tab's identity — the other one.
 *
 * Not a settings section because it is not a bag of values: it is a list of
 * installed things, each with its own settings form read from the module's own
 * declaration. It sits beside Backup for the same reason Backup does — it is
 * about the *installation* rather than about anything in it. */
export const MODULES_TAB = "modules";

/** The Checklists tab's identity: the Data Layer's checklists, the ones with
 * no application (TODO-checklists C1.16). A list of records, like Modules, not
 * a bag of values; an application's checklists stay under that application. */
export const CHECKLISTS_TAB = "checklists";

/** The System tab's identity: which build this is, what the process is doing
 * to the machine, and the two acts on the whole installation — Restart and
 * Clear all. Facts about the running server, not settings; last in the strip. */
export const SYSTEM_TAB = "system";

/** Which tab is showing: a section's `name`, or {@link BACKUP_TAB}. */
export type SettingsTab = string;

/** One entry in the tab strip. */
export type TabSpec = { id: SettingsTab; label: string };

/** The tabs this screen has, in the order they are shown: one per declared
 * section, then Modules, Checklists, Backup and System.
 *
 * A pure function of the sections, so what an admin sees is exactly what the
 * server declared — a section with no tab, or a tab with no section, cannot
 * happen because there is nowhere for either to come from. */
export function settingsTabs(sections: Section[]): TabSpec[] {
  return [
    ...sections.map((section) => ({ id: section.name, label: section.label })),
    { id: MODULES_TAB, label: "Modules" },
    { id: CHECKLISTS_TAB, label: "Checklists" },
    { id: BACKUP_TAB, label: "Backup" },
    { id: SYSTEM_TAB, label: "System" },
  ];
}

/** Which tab a freshly loaded screen opens on: the first settings section, and
 * Backup only when the server declared no sections at all. */
export function initialTab(sections: Section[], requested?: string | null): SettingsTab {
  if (requested && settingsTabs(sections).some((entry) => entry.id === requested)) {
    return requested;
  }
  return sections.length > 0 ? sections[0].name : MODULES_TAB;
}

/** The tab a `/settings/<tab>` route asks for, or `null` for plain `/settings`.
 * What a screen elsewhere links to when "back" means one tab of this one — the
 * checklist screens' way back to the Checklists tab. */
export function settingsTabFromPath(path: string): string | null {
  const match = path.match(/^\/settings\/([^/]+)$/);
  return match ? decodeURIComponent(match[1]) : null;
}

/** The route that opens this screen on `tab`. */
export function settingsTabPath(tab: SettingsTab): string {
  return `/settings/${encodeURIComponent(tab)}`;
}

/** One save of the settings that save as they are edited: the payload, and the
 * form's text it was built from, so the answer can tell which boxes the admin
 * has typed into since. */
type AutosaveBody = { values: Record<string, unknown>; form: Record<string, string> };

/** What the autosaver is handed for the form showing `values`: every setting
 * that saves as it is edited, as the form holds it. Compared against the last
 * save by the autosaver, so a commit that changed nothing sends nothing. */
function autosaveBody(spec: SettingSpec[], values: Record<string, string>): AutosaveBody {
  const fields = autosavedFields(spec);
  const form: Record<string, string> = {};
  for (const field of fields) form[field.name] = values[field.name] ?? "";
  return { values: settingsPayload(fields, values), form };
}

/** A question the screen is asking before a save. */
type Confirming = { kind: "sign-out" } | { kind: "move"; section: string } | null;

export function Settings({ requestedTab = null }: { requestedTab?: string | null }) {
  const { t } = useT();
  // The sections are loaded *here* rather than inside a panel because the tab
  // strip is derived from them: a screen whose panels fetched their own could
  // not name its own tabs.
  const [sections, setSections] = useState<Section[] | null>(null);
  // What the form shows, and what the server last said is *stored*. Mirrored
  // in refs because a save's answer and the handlers that start one must read
  // the newest of both, not the render they were created in.
  const [values, setValues] = useState<Record<string, string>>({});
  const [stored, setStored] = useState<Record<string, string>>({});
  const valuesRef = useRef(values);
  const storedRef = useRef(stored);
  const sectionsRef = useRef<Section[]>([]);
  // The keys this host's `feldspar.toml` pins: shown, never editable.
  const [hostKeys, setHostKeys] = useState<string[]>([]);
  const [tab, setTab] = useState<SettingsTab>(BACKUP_TAB);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>({ kind: "idle" });
  // The card whose Apply is in flight, the last one applied, and why one was
  // refused — each shown in that card.
  const [applying, setApplying] = useState<string | null>(null);
  const [applied, setApplied] = useState<string | null>(null);
  const [applyError, setApplyError] = useState<{ section: string; message: string } | null>(
    null,
  );
  // Whether the last save moved the admin UI to another host, which opens the
  // dialog that waits for the new address and then goes there.
  const [moving, setMoving] = useState(false);
  const [confirming, setConfirming] = useState<Confirming>(null);

  const show = (next: Record<string, string>) => {
    valuesRef.current = next;
    setValues(next);
  };

  /** Take a save's answer: what is stored now, and the saved boxes as stored. */
  const land = (
    response: GetSettingsResponse,
    names: string[],
    sent: Record<string, string>,
  ): boolean => {
    const before = storedRef.current;
    const after = readConfig(response.values);
    // The session this screen runs on has ended: reloading is what shows the
    // sign-in page, and there is nothing left to follow a move with.
    if (signsEveryoneOut(before, after)) {
      window.location.reload();
      return false;
    }
    // Compared as stored, before and after: the server has already moved the
    // admin UI by the time this answer arrives.
    if (adminMoved(before, after)) setMoving(true);
    storedRef.current = after;
    sectionsRef.current = response.sections;
    setStored(after);
    setSections(response.sections);
    setHostKeys(response.host_keys);
    const fresh = initialValues(allFields(response.sections), after);
    show(afterSave(names, sent, valuesRef.current, fresh));
    return true;
  };

  const saverRef = useRef<Autosaver<AutosaveBody> | null>(null);
  if (!saverRef.current) {
    saverRef.current = new Autosaver<AutosaveBody>(
      async (body) => {
        const response = await api.updateSettings({ values: body.values });
        land(response, Object.keys(body.form), body.form);
      },
      setSaveState,
      (err) => errorMessage(err, t("Could not save the settings.")),
    );
  }

  useEffect(() => {
    void (async () => {
      try {
        const response = await api.getSettings();
        const config = readConfig(response.values);
        const spec = allFields(response.sections);
        const initial = initialValues(spec, config);
        storedRef.current = config;
        sectionsRef.current = response.sections;
        setSections(response.sections);
        setStored(config);
        setHostKeys(response.host_keys);
        show(initial);
        setTab(initialTab(response.sections, requestedTab));
        // What was loaded is stored: not a change to save.
        saverRef.current?.baseline(autosaveBody(spec, initial));
      } catch {
        setLoadError(t("Could not load the settings."));
      }
    })();
    // Leaving the screen saves what is waiting.
    return () => {
      void saverRef.current?.flush();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Save the settings that save as they are edited, as `next` has them. */
  const autosave = (next: Record<string, string>) => {
    saverRef.current?.change(autosaveBody(allFields(sectionsRef.current), next), 0);
  };

  const onChange = (field: SettingSpec, value: string) => {
    const next = { ...valuesRef.current, [field.name]: value };
    show(next);
    if (applied) setApplied(null);
    if (saveMode(field) !== "change") return;
    // The one switch that signs everyone out asks first; Cancel puts it back.
    if (signsEveryoneOut(storedRef.current, next)) setConfirming({ kind: "sign-out" });
    else autosave(next);
  };

  const onCommit = (field: SettingSpec, value: string) => {
    if (saveMode(field) !== "commit") return;
    const next = { ...valuesRef.current, [field.name]: value };
    show(next);
    autosave(next);
  };

  /** Apply one card's held settings: the ones the admin has changed. */
  const apply = async (section: Section, confirmed = false) => {
    const held = appliedFields(section.fields);
    const names = unsavedNames(held, storedRef.current, valuesRef.current);
    if (names.length === 0) return;
    // Applying a new admin subdomain moves this screen: said before it does.
    if (
      !confirmed &&
      names.includes(ADMIN_SUBDOMAIN) &&
      adminMoved(storedRef.current, valuesRef.current)
    ) {
      setConfirming({ kind: "move", section: section.name });
      return;
    }
    setConfirming(null);
    setApplying(section.name);
    setApplyError(null);
    setApplied(null);
    try {
      // After whatever is still autosaving, so the two never race.
      await saverRef.current?.flush();
      const sent = valuesRef.current;
      const response = await api.updateSettings({
        values: settingsPayload(
          held.filter((field) => names.includes(field.name)),
          sent,
        ),
      });
      if (land(response, names, sent)) setApplied(section.name);
    } catch (e) {
      setApplyError({
        section: section.name,
        message: errorMessage(e, t("Could not save the settings.")),
      });
    } finally {
      setApplying(null);
    }
  };

  const confirmSignOut = () => {
    setConfirming(null);
    autosave(valuesRef.current);
  };

  const cancelSignOut = () => {
    setConfirming(null);
    const saved = initialValues(allFields(sectionsRef.current), storedRef.current);
    show({ ...valuesRef.current, [SHARED_SESSION_COOKIE]: saved[SHARED_SESSION_COOKIE] ?? "" });
  };

  if (!sections) {
    return (
      <>
        <PageHeader pretitle={t("Administration")} title={t("Settings")} />
        <PageBody>
          {loadError ? (
            <Alert variant="danger">{loadError}</Alert>
          ) : (
            <Spinner animation="border" role="status" />
          )}
        </PageBody>
      </>
    );
  }

  const spec = allFields(sections);
  const onSettingsTab = sections.some((section) => section.name === tab);
  const movingTo = (values[ADMIN_SUBDOMAIN] ?? "").trim().toLowerCase();

  return (
    <>
      <PageHeader
        pretitle={t("Administration")}
        title={t("Settings")}
        actions={onSettingsTab ? <SaveStatus state={saveState} /> : undefined}
      />
      <PageBody>
        {/* Hand-built rather than react-bootstrap's `Tabs`, for the reason the
            multi-select is hand-built: the admin SPA is served under a strict CSP
            with no inline styles, and these are Tabler's own `.nav-underline` classes
            with nothing but classes doing the work. Each panel is mounted only
            while it is showing, which is what makes the Backup tab's first render
            the thing that loads its options. */}
        <ul className="nav nav-underline mb-3" role="tablist">
          {settingsTabs(sections).map((entry) => (
            <li className="nav-item" key={entry.id} role="presentation">
              <button
                type="button"
                id={`settings-tab-${entry.id}`}
                className={`nav-link${tab === entry.id ? " active" : ""}`}
                role="tab"
                aria-selected={tab === entry.id}
                onClick={() => setTab(entry.id)}
              >
                {entry.label}
              </button>
            </li>
          ))}
        </ul>

        {sections.map((section) => (
          <TabPanel id={section.name} showing={tab} key={section.name}>
            {saveState.kind === "error" && (
              <Alert variant="danger">
                <AlertBody>{t("Not saved: {reason}", { reason: saveState.message })}</AlertBody>
              </Alert>
            )}
            <SectionCard
              section={section}
              spec={spec}
              values={values}
              stored={stored}
              hostKeys={hostKeys}
              applying={applying === section.name}
              busy={applying !== null}
              applied={applied === section.name}
              applyError={applyError?.section === section.name ? applyError.message : null}
              onDismissApplied={() => setApplied(null)}
              onChange={onChange}
              onCommit={onCommit}
              onApply={() => void apply(section)}
            />
            {/* Whatever this section has beyond its fields — the Email tab's
                test message. Outside the card on purpose: it is a different
                verb, and it acts on what is *stored*. */}
            <SectionExtra name={section.name} stored={stored} values={values} />
          </TabPanel>
        ))}
        <TabPanel id={MODULES_TAB} showing={tab}>
          <ModulesTab />
        </TabPanel>
        <TabPanel id={CHECKLISTS_TAB} showing={tab}>
          <Checklists appId={null} />
        </TabPanel>
        <TabPanel id={BACKUP_TAB} showing={tab}>
          <BackupTab />
        </TabPanel>
        <TabPanel id={SYSTEM_TAB} showing={tab}>
          {/* Clear all is at the bottom of this tab, with Restart: the acts on
              the whole installation rather than on anything in it. */}
          <SystemTab />
        </TabPanel>
        {moving && <AdminMove onClose={() => setMoving(false)} />}
        <Modal show={confirming?.kind === "sign-out"} onHide={cancelSignOut} centered>
          <Modal.Header closeButton>
            <Modal.Title>
              <T text="You will be signed out" />
            </Modal.Title>
          </Modal.Header>
          <Modal.Body>
            <p className="mb-0">
              <T text="Changing whether applications share a sign-in ends every session, yours included. Everyone, you as well, will need to sign in again." />
            </p>
          </Modal.Body>
          <Modal.Footer>
            <Button variant="secondary" onClick={cancelSignOut}>
              <T text="Cancel" />
            </Button>
            <Button onClick={confirmSignOut}>
              <T text="Save and sign out" />
            </Button>
          </Modal.Footer>
        </Modal>
        <Modal show={confirming?.kind === "move"} onHide={() => setConfirming(null)} centered>
          <Modal.Header closeButton>
            <Modal.Title>
              <T text="Move the admin UI?" />
            </Modal.Title>
          </Modal.Header>
          <Modal.Body>
            <p className="mb-0">
              {movingTo ? (
                <T
                  text="The admin UI will be served on the subdomain {subdomain} from now on. This page waits for the new address to answer, then takes you there."
                  values={{ subdomain: <code>{movingTo}</code> }}
                />
              ) : (
                <T text="The admin UI will be served on the base domain from now on. This page waits for the new address to answer, then takes you there." />
              )}
            </p>
          </Modal.Body>
          <Modal.Footer>
            <Button variant="secondary" onClick={() => setConfirming(null)}>
              <T text="Cancel" />
            </Button>
            <Button
              onClick={() => {
                const section = sections.find(
                  (s) => confirming?.kind === "move" && s.name === confirming.section,
                );
                if (section) void apply(section, true);
              }}
            >
              <T text="Move" />
            </Button>
          </Modal.Footer>
        </Modal>
      </PageBody>
    </>
  );
}

/** One tab's contents, rendered only while its tab is the one selected. */
function TabPanel({
  id,
  showing,
  children,
}: {
  id: SettingsTab;
  showing: SettingsTab;
  children: ReactNode;
}) {
  if (id !== showing) return null;
  return (
    <div role="tabpanel" aria-labelledby={`settings-tab-${id}`}>
      {children}
    </div>
  );
}

/** One section's heading, explanation and controls — with, when it holds
 * settings for Apply, the Apply button at the right of its title row. */
function SectionCard({
  section,
  spec,
  values,
  stored,
  hostKeys,
  applying,
  busy,
  applied,
  applyError,
  onDismissApplied,
  onChange,
  onCommit,
  onApply,
}: {
  section: Section;
  /** Every section's fields: what a field's `show_if` may name. */
  spec: SettingSpec[];
  values: Record<string, string>;
  stored: Record<string, string>;
  hostKeys: string[];
  applying: boolean;
  busy: boolean;
  applied: boolean;
  applyError: string | null;
  onDismissApplied: () => void;
  onChange: (field: SettingSpec, value: string) => void;
  onCommit: (field: SettingSpec, value: string) => void;
  onApply: () => void;
}) {
  const { t } = useT();
  const pinned = t(
    "Set in this server's feldspar.toml, which wins over this screen. Change it there and restart the server.",
  );
  const fields: SettingSpec[] = section.fields;
  const held = appliedFields(fields);
  const dirty = unsavedNames(held, stored, values).length > 0;
  const mixed = mixesModes(fields);
  const formId = `settings-${section.name}-apply`;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    onApply();
  };
  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title">{section.label}</h3>
          <p className="card-subtitle text-secondary mb-0">{section.description}</p>
        </div>
        {held.length > 0 && (
          <div className="card-actions">
            <ApplyButton dirty={dirty} busy={busy} applying={applying} form={formId} />
          </div>
        )}
      </div>
      {/* The form the title row's Apply button submits: Enter in one of the
          card's held boxes applies the card, and a box that saves on its own
          catches its Enter itself (`SettingField`'s `onCommit`), so a card
          with nothing held never submits anything. */}
      <form className="card-body" id={formId} onSubmit={submit}>
        {applyError && (
          <Alert variant="danger">
            <AlertBody>{applyError}</AlertBody>
          </Alert>
        )}
        {applied && (
          <Alert variant="success" dismissible onClose={onDismissApplied}>
            <AlertBody>
              <T text="Applied." />
            </AlertBody>
          </Alert>
        )}
        {/* A field whose `show_if` does not hold for what the form shows now
            is hidden, as the admin edits — MCP from this machine only, while
            the MCP server is unticked. */}
        {fields.filter((field) => isShown(field, spec, values)).map((field) => (
          <div key={field.name}>
            <SettingField
              field={field}
              value={values[field.name] ?? ""}
              onChange={(v) => onChange(field, v)}
              onCommit={(v) => onCommit(field, v)}
              idPrefix={`setting-${section.name}`}
              pinned={hostKeys.includes(field.name) ? pinned : undefined}
            />
            {/* The help sits under the control rather than in the label:
                these are sentences, and a label is a name. */}
            {field.help && (
              <div className="form-hint mt-n2 mb-3 text-secondary">{field.help}</div>
            )}
            {/* In a card where everything else saves at once, a setting that
                waits says so. */}
            {mixed && field.applied && (
              <div className="form-hint mt-n2 mb-3 text-secondary">
                <T text="Saved when you press Apply." />
              </div>
            )}
          </div>
        ))}
      </form>
    </div>
  );
}

/** The **one** place this screen knows a section by name: a section may have an
 * *act* as well as fields, or a *reading* that is not a field at all, and
 * neither can be declared as a `FormField`.
 *
 * Kept to a single lookup so it is obvious what the exception costs — a section
 * with nothing here renders its form and nothing else, which is every section
 * but Email, Localisation, Development and Authentication. */
function SectionExtra({
  name,
  stored,
  values,
}: {
  name: string;
  stored: Record<string, string>;
  values: Record<string, string>;
}) {
  if (name === "email") return <TestEmail />;
  // Settings → Localisation → Translations (shared) (TODO F6).
  if (name === "localisation") return <SharedTranslationsPanel />;
  // Settings → Authentication → External identity providers: a list of
  // records with a form each, not fields of this one.
  if (name === "authentication") return <OAuthProvidersPanel />;
  if (name === "development")
    return (
      <>
        <PythonStatusPanel />
        {/* Hidden while the MCP server is unticked on the form, as its
            loopback switch is; ticked but not yet saved, the panel says the
            route is not served yet. */}
        {mcpEnabled(values) && <McpTokensPanel enabled={mcpEnabled(stored)} />}
        {/* Which claude the IDE's terminal runs: shown while the switch is
            ticked, and read again after a save changes the switch or the
            path, so it answers for what was saved. */}
        {terminalTicked(values) && <ClaudeCodeStatusPanel key={statusKey(stored)} />}
        <CodeLibraryPanel />
        {/* The machines that build native apps for this server
            (`TODO-build-hosts.md`). */}
        <BuildHostsPanel />
      </>
    );
  return null;
}
