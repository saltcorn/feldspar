/**
 * The settings screen's model: sections in, one payload out.
 *
 * The screen renders whatever the server declares, so what is worth pinning
 * down is the **round trip** — what an admin who opens Settings, changes one
 * box and saves actually sends. Two ways that can go wrong without a browser
 * noticing:
 *
 * - a value typed in a form is a string, and a `bool` or `int` setting sent as
 *   `"true"` or `"2525"` is refused by the server's own type check;
 * - the private key is shown as the redaction sentinel, and *not* sending it
 *   back unchanged would clear the stored certificate's key on the next save of
 *   an unrelated setting.
 *
 * And, since the tabs became a function of the sections rather than a constant,
 * what an admin sees along the top: one tab per declared section, Backup last.
 */

import { describe, expect, it } from "vitest";

import {
  SECRET_SENTINEL,
  buildConfig,
  initialValues,
  readConfig,
} from "../settings";
import {
  BACKUP_TAB,
  MODULES_TAB,
  allFields,
  initialTab,
  SHARED_SESSION_COOKIE,
  settingsPayload,
  settingsTabs,
  signsEveryoneOut,
} from "./Settings";
import { showsDetail, stateBadge } from "./PythonStatus";
import { testEmailBody } from "./TestEmail";

/** The shape `getSettings` returns, trimmed to what the model reads. */
const sections = [
  {
    name: "ssl",
    label: "SSL / TLS certificates",
    description: "How this server obtains its certificates.",
    fields: [
      {
        name: "ssl_mode",
        label: "Certificate source",
        type: "text",
        required: false,
        default: "off",
        options: ["off", "letsencrypt", "custom"],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
        help: "off serves plain HTTP.",
      },
      {
        name: "ssl_private_key",
        label: "Private key (PEM)",
        type: "text",
        required: false,
        default: null,
        options: [],
        multiline: true,
        secret: true,
        create_only: false,
        show_if: [],
        help: "Stored in the database.",
      },
      {
        name: "smtp_port",
        label: "SMTP port",
        type: "int",
        required: false,
        default: 587,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
        help: "",
      },
      {
        name: "redirect_http_to_https",
        label: "Redirect HTTP to HTTPS",
        type: "bool",
        required: false,
        default: true,
        options: [],
        multiline: false,
        secret: false,
        create_only: false,
        show_if: [],
        help: "",
      },
    ],
  },
];

describe("the settings screen's model", () => {
  it("edits every section's fields as one payload", () => {
    expect(allFields(sections).map((f) => f.name)).toEqual([
      "ssl_mode",
      "ssl_private_key",
      "smtp_port",
      "redirect_http_to_https",
    ]);
  });

  it("sends each setting as the type its declaration says", () => {
    const stored = readConfig({
      ssl_mode: "custom",
      ssl_private_key: SECRET_SENTINEL,
      smtp_port: 2525,
      redirect_http_to_https: false,
    });
    const values = initialValues(allFields(sections), stored);
    // What the form holds is text — that is what an input is.
    expect(values.smtp_port).toBe("2525");
    expect(values.redirect_http_to_https).toBe("false");

    const payload = buildConfig(allFields(sections), values);
    expect(payload).toEqual({
      ssl_mode: "custom",
      // Untouched: the sentinel goes back, and the server reads it as "keep
      // what is stored". Anything else here would clear a working key.
      ssl_private_key: SECRET_SENTINEL,
      smtp_port: 2525,
      redirect_http_to_https: false,
    });
  });

  it("falls back to each declared default for a setting nobody has set", () => {
    const values = initialValues(allFields(sections), readConfig({}));
    expect(values.ssl_mode).toBe("off");
    expect(values.smtp_port).toBe("587");
    const payload = buildConfig(allFields(sections), values);
    expect(payload.smtp_port).toBe(587);
    // An empty optional box is left out rather than sent as "", which is what
    // makes clearing a setting mean "use the default".
    expect(payload).not.toHaveProperty("ssl_private_key");
  });

  /** Each setting is its own row, so an omitted key means "leave it" — which
   * would make an emptied box do nothing. `null` is what clears one. */
  it("clears an emptied setting rather than leaving it as it was", () => {
    const values = initialValues(
      allFields(sections),
      readConfig({ ssl_mode: "custom", ssl_extra_domains: "old.example.com" }),
    );
    values.ssl_private_key = "";
    const payload = settingsPayload(allFields(sections), values);
    expect(payload.ssl_private_key).toBeNull();
    // Everything else still travels as its declared type.
    expect(payload.ssl_mode).toBe("custom");
    expect(payload.smtp_port).toBe(587);
  });

  it("sends a replaced secret as what was typed", () => {
    const values = initialValues(
      allFields(sections),
      readConfig({ ssl_private_key: SECRET_SENTINEL }),
    );
    values.ssl_private_key =
      "-----BEGIN PRIVATE KEY-----\nnew\n-----END PRIVATE KEY-----";
    expect(buildConfig(allFields(sections), values).ssl_private_key).toContain(
      "new",
    );
  });
});

/** A second section, to say something about *two* of them that one cannot. */
const emailSection = {
  name: "email",
  label: "Email",
  description: "The SMTP server this installation sends mail through.",
  fields: [
    {
      name: "smtp_host",
      label: "SMTP host",
      type: "text",
      required: false,
      default: null,
      options: [],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
      help: "The mail server's hostname.",
    },
  ],
};

/** The Development section: a checkbox and a level dropdown, which between them
 * are the two control kinds a settings tab renders and nothing else does. */
const developmentSection = {
  name: "development",
  label: "Development",
  description: "What this server prints while it runs.",
  fields: [
    {
      name: "log_sql",
      label: "Log SQL",
      type: "bool",
      required: false,
      default: false,
      options: [],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
      help: "Print every statement this server sends to the database.",
    },
    {
      name: "log_verbosity",
      label: "Log verbosity",
      type: "text",
      required: false,
      default: "warning",
      options: ["error", "warning", "info", "verbose", "trace"],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
      help: "info logs every server request.",
    },
  ],
};

describe("the settings screen's tabs", () => {
  it("shows one tab per declared section, labelled as the section is", () => {
    expect(
      settingsTabs([...sections, emailSection, developmentSection]),
    ).toEqual([
      { id: "ssl", label: "SSL / TLS certificates" },
      { id: "email", label: "Email" },
      { id: "development", label: "Development" },
      { id: MODULES_TAB, label: "Modules" },
      { id: BACKUP_TAB, label: "Backup" },
    ]);
  });

  /** The whole point of deriving them: a section the server adds gets a tab
   * without a line changing here, and a tab this screen invented could not
   * exist because there is nowhere for it to come from. */
  it("has no tab without a section, and no section without a tab", () => {
    const tabs = settingsTabs([...sections, emailSection]);
    const sectionNames = [...sections, emailSection].map((s) => s.name);
    const standalone = [MODULES_TAB, BACKUP_TAB];
    expect(
      tabs.filter((t) => !standalone.includes(t.id)).map((t) => t.id),
    ).toEqual(sectionNames);
    // Modules and Backup are the two tabs that are not sections, and they are
    // last, in that order.
    expect(tabs.slice(-2).map((t) => t.id)).toEqual(standalone);
    expect(tabs.filter((t) => t.id === BACKUP_TAB)).toHaveLength(1);
    expect(tabs.filter((t) => t.id === MODULES_TAB)).toHaveLength(1);
  });

  it("keeps Modules and Backup even when the server declares no sections at all", () => {
    expect(settingsTabs([])).toEqual([
      { id: MODULES_TAB, label: "Modules" },
      { id: BACKUP_TAB, label: "Backup" },
    ]);
    expect(initialTab([])).toBe(MODULES_TAB);
  });

  it("opens on the first settings section rather than on Backup", () => {
    expect(initialTab([...sections, emailSection])).toBe("ssl");
  });

  /** Every section's fields are still edited as **one** payload, from whichever
   * tab is showing: the tabs partition the display, not the transaction. */
  it("still saves every section's settings in one act", () => {
    const spec = allFields([...sections, emailSection]);
    const values = initialValues(
      spec,
      readConfig({ ssl_mode: "custom", smtp_host: "smtp.example.com" }),
    );
    const payload = settingsPayload(spec, values);
    expect(payload.ssl_mode).toBe("custom");
    expect(payload.smtp_host).toBe("smtp.example.com");
  });
});

describe("the Development tab", () => {
  /** A checkbox nobody ticked has to travel as `false`. An unticked box is not
   * an empty box: dropped, it would read as "leave it as it was", and the one
   * thing an admin does with this switch more often than turning it on is
   * turning it off again. */
  it("sends an unticked Log SQL as false rather than omitting it", () => {
    const spec = allFields([developmentSection]);
    const values = initialValues(spec, readConfig({ log_sql: true }));
    expect(values.log_sql).toBe("true");
    values.log_sql = "false";
    const payload = settingsPayload(spec, values);
    expect(payload.log_sql).toBe(false);
  });

  /** The level is a string chosen from the declared options, and the default is
   * what an installation nobody has touched sends back. */
  it("sends the chosen verbosity, defaulting to warning", () => {
    const spec = allFields([developmentSection]);
    const untouched = initialValues(spec, readConfig({}));
    expect(settingsPayload(spec, untouched).log_verbosity).toBe("warning");

    const values = initialValues(spec, readConfig({ log_verbosity: "info" }));
    expect(settingsPayload(spec, values).log_verbosity).toBe("info");
  });
});

describe("the test-email form", () => {
  /** An empty box means "send it to me", which the server spells as an absent
   * `to`. Sending `""` would ask it to parse the empty string as an address. */
  it("omits the recipient rather than sending an empty one", () => {
    expect(testEmailBody("")).toEqual({});
    expect(testEmailBody("   ")).toEqual({});
  });

  it("sends the address that was typed, trimmed", () => {
    expect(testEmailBody("  ada@example.com ")).toEqual({
      to: "ada@example.com",
    });
  });
});

describe("the Development tab's Python reading", () => {
  /** Each of the states the server can be in gets its own label — the whole
   * point of the panel is that "not built with Python", "turned off" and "not
   * started yet" have three different remedies. */
  it("labels each state the server can report", () => {
    expect(stateBadge("running").label).toBe("Running");
    expect(stateBadge("not_initialised").label).toBe("Not started yet");
    expect(stateBadge("off").label).toBe("Turned off");
    expect(stateBadge("not_built").label).toBe("Not built with Python");
    expect(stateBadge("unavailable").label).toBe("Not available");
  });

  /** A server ahead of the SPA it is serving — a state added after this bundle
   * was built — shows the server's own word rather than nothing. */
  it("falls back to the server's own word for a state it does not know", () => {
    expect(stateBadge("hibernating").label).toBe("hibernating");
  });

  /** A build with no interpreter has no environment, no packages and no runs,
   * and a table of zeros beside "not built with Python" would read as though
   * the zeros were the problem. */
  it("hides the numbers where there is no interpreter for them to describe", () => {
    expect(showsDetail("not_built")).toBe(false);
    expect(showsDetail("unavailable")).toBe(false);
    expect(showsDetail("off")).toBe(true);
    expect(showsDetail("not_initialised")).toBe(true);
    expect(showsDetail("running")).toBe(true);
  });
});

describe("the Development tab's shared sign-in", () => {
  /** The confirmation opens for a change either way, and for nothing else:
   * the server ends every session when the cookie's scope changes, and only
   * then. A setting never saved is off, which is what the form shows. */
  it("asks before a save that changes whether the session cookie is shared", () => {
    const off = { [SHARED_SESSION_COOKIE]: "false" };
    const on = { [SHARED_SESSION_COOKIE]: "true" };
    expect(signsEveryoneOut({}, on)).toBe(true);
    expect(signsEveryoneOut(off, on)).toBe(true);
    expect(signsEveryoneOut(on, off)).toBe(true);
    expect(signsEveryoneOut({}, off)).toBe(false);
    expect(signsEveryoneOut(on, on)).toBe(false);
    expect(signsEveryoneOut(off, { ...off, log_sql: "true" })).toBe(false);
  });
});
