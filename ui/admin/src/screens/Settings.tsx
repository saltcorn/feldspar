// Settings screen: the values in `_fd_config`, rendered from their declarations —
// and, beside them, backup and restore.
//
// The screen is **tabbed**, and the tabs are **the sections the server declared**,
// plus Backup. That is the whole of what this screen knows about how settings are
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
// Three behaviours are worth naming because they are not obvious from the code:
//
// - **A secret arrives as the sentinel** and is posted back unchanged unless the
//   admin types over it, which is how the private key stays editable without
//   ever being sent to the browser (`SettingField` handles the input itself).
// - **Saving is one act**, over the whole bag, from whichever tab is showing.
//   The tabs partition the *display*, not the transaction: the server validates
//   the settings together — `custom` mode with no certificate, an SMTP username
//   with no password — and refuses them whole, so a per-tab save would be a way
//   to leave half a configuration applied. The message it refuses with is the
//   server's own, because "the certificate and private key do not match" is
//   worth more than anything this screen could invent.
// - **The one act is why the values live in this component** rather than in each
//   panel: a panel holding its own edits would post a bag missing every other
//   tab's, which the server would read as "clear them".

import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import type { GetSettingsResponse } from "../client";
import { AlertBody, PageBody, PageHeader } from "../layout";
import { mcpEnabled } from "../mcpTokens";
import {
  SettingField,
  buildConfig,
  initialValues,
  readConfig,
  type FieldSpec,
} from "../settings";
import { BackupTab } from "./BackupTab";
import { ClearAllPanel } from "./ClearAll";
import { McpTokensPanel } from "./McpTokens";
import { ModulesTab } from "./ModulesTab";
import { PythonStatusPanel } from "./PythonStatus";
import { TestEmail } from "./TestEmail";
import { T, useT } from "../i18n";

/** One section as the API describes it. */
type Section = GetSettingsResponse["sections"][number];

/** Every field of every section, flattened — the spec the payload is built
 * against, since the save is one bag of values rather than one per section. */
export function allFields(sections: Section[]): FieldSpec[] {
  return sections.flatMap((section) => section.fields);
}

/** What Save sends: every declared setting, with an emptied box as `null`.
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

/** Which tab is showing: a section's `name`, or {@link BACKUP_TAB}. */
export type SettingsTab = string;

/** One entry in the tab strip. */
export type TabSpec = { id: SettingsTab; label: string };

/** The tabs this screen has, in the order they are shown: one per declared
 * section, then Backup.
 *
 * A pure function of the sections, so what an admin sees is exactly what the
 * server declared — a section with no tab, or a tab with no section, cannot
 * happen because there is nowhere for either to come from. */
export function settingsTabs(sections: Section[]): TabSpec[] {
  return [
    ...sections.map((section) => ({ id: section.name, label: section.label })),
    { id: MODULES_TAB, label: "Modules" },
    { id: BACKUP_TAB, label: "Backup" },
  ];
}

/** Which tab a freshly loaded screen opens on: the first settings section, and
 * Backup only when the server declared no sections at all. */
export function initialTab(sections: Section[]): SettingsTab {
  return sections.length > 0 ? sections[0].name : MODULES_TAB;
}

export function Settings() {
  const { t } = useT();
  // The sections are loaded *here* rather than inside a panel because the tab
  // strip is derived from them: a screen whose panels fetched their own could
  // not name its own tabs.
  const [sections, setSections] = useState<Section[] | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  // What the server last said is *stored*, as distinct from what the form is
  // currently showing. A panel below the form acts on the stored configuration —
  // the MCP token panel offers to mint against a route that is either served or
  // not — and a ticked-but-unsaved checkbox is neither.
  const [stored, setStored] = useState<Record<string, string>>({});
  // The keys this host's `feldspar.toml` pins: shown, never editable.
  const [hostKeys, setHostKeys] = useState<string[]>([]);
  const [tab, setTab] = useState<SettingsTab>(BACKUP_TAB);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  /** Take a settings response as the screen's state. */
  const adopt = (response: GetSettingsResponse, opening: boolean) => {
    const config = readConfig(response.values);
    setSections(response.sections);
    setValues(initialValues(allFields(response.sections), config));
    setStored(config);
    setHostKeys(response.host_keys);
    if (opening) setTab(initialTab(response.sections));
  };

  useEffect(() => {
    void (async () => {
      try {
        adopt(await api.getSettings(), true);
      } catch {
        setError("Could not load the settings.");
      }
    })();
  }, []);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!sections) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      // The response is what was *stored*, not what was sent: a cleared box
      // comes back as the declared default, and a secret as the sentinel.
      adopt(
        await api.updateSettings({ values: settingsPayload(allFields(sections), values) }),
        false,
      );
      setSaved(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the settings.");
    } finally {
      setBusy(false);
    }
  };

  if (!sections) {
    return (
      <>
        <PageHeader title={t("Settings")} />
        <PageBody>
          {error ? (
            <Alert variant="danger">{error}</Alert>
          ) : (
            <Spinner animation="border" role="status" />
          )}
        </PageBody>
      </>
    );
  }

  return (
    <>
      <PageHeader title={t("Settings")} />
      <PageBody>
        {/* Hand-built rather than react-bootstrap's `Tabs`, for the reason the
            multi-select is hand-built: the admin SPA is served under a strict CSP
            with no inline styles, and these are Tabler's own `.nav-pills` classes
            with nothing but classes doing the work. Each panel is mounted only
            while it is showing, which is what makes the Backup tab's first render
            the thing that loads its options. */}
        <ul className="nav nav-pills mb-3" role="tablist">
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
            {error && (
              <Alert variant="danger">
                <AlertBody>{error}</AlertBody>
              </Alert>
            )}
            {saved && (
              <Alert variant="success" dismissible onClose={() => setSaved(false)}>
                <AlertBody>
                  <T text="Settings saved. Certificate and port changes take effect when the server restarts." />
                </AlertBody>
              </Alert>
            )}
            <form onSubmit={(e) => void save(e)}>
              <SectionCard
                section={section}
                values={values}
                hostKeys={hostKeys}
                onChange={(name, v) => setValues((current) => ({ ...current, [name]: v }))}
              />
              <div className="btn-list">
                <Button type="submit" disabled={busy}>
                  {busy ? "Saving…" : "Save settings"}
                </Button>
              </div>
            </form>
            {/* Whatever this section has beyond its fields — the Email tab's
                test message. Outside the form on purpose: it is a different
                verb, and it acts on what is *stored*. */}
            <SectionExtra name={section.name} stored={stored} />
          </TabPanel>
        ))}
        <TabPanel id={MODULES_TAB} showing={tab}>
          <ModulesTab />
        </TabPanel>
        <TabPanel id={BACKUP_TAB} showing={tab}>
          <BackupTab />
        </TabPanel>
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

/** One section's heading, explanation and controls. */
function SectionCard({
  section,
  values,
  hostKeys,
  onChange,
}: {
  section: Section;
  values: Record<string, string>;
  hostKeys: string[];
  onChange: (name: string, value: string) => void;
}) {
  const { t } = useT();
  const pinned = t(
    "Set in this server's feldspar.toml, which wins over this screen. Change it there and restart the server.",
  );
  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title">{section.label}</h3>
          <p className="card-subtitle text-secondary mb-0">{section.description}</p>
        </div>
      </div>
      <div className="card-body">
        {section.fields.map((field) => (
          <div key={field.name}>
            <SettingField
              field={field}
              value={values[field.name] ?? ""}
              onChange={(v) => onChange(field.name, v)}
              idPrefix={`setting-${section.name}`}
              pinned={hostKeys.includes(field.name) ? pinned : undefined}
            />
            {/* The help sits under the control rather than in the label:
                these are sentences, and a label is a name. */}
            {field.help && (
              <div className="form-hint mt-n2 mb-3 text-secondary">{field.help}</div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

/** The **one** place this screen knows a section by name: a section may have an
 * *act* as well as fields, or a *reading* that is not a field at all, and
 * neither can be declared as a `FormField`.
 *
 * Kept to a single lookup so it is obvious what the exception costs — a section
 * with nothing here renders its form and nothing else, which is every section
 * but Email and Development. */
function SectionExtra({
  name,
  stored,
}: {
  name: string;
  stored: Record<string, string>;
}) {
  if (name === "email") return <TestEmail />;
  if (name === "development")
    return (
      <>
        <PythonStatusPanel />
        <McpTokensPanel enabled={mcpEnabled(stored)} />
        <ClearAllPanel />
      </>
    );
  return null;
}
