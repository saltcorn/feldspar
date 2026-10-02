// Top-level admin app: bootstraps auth state and gates the three top-level
// states the design calls for — create-first-user, login, and the authenticated
// admin shell — then routes between the admin screens with a tiny hash router
// (no router dependency, and no inline styles, so the strict CSP holds).
//
// The shell is Tabler's **vertical layout**: a dark sidebar holding the brand
// and the section links, and a `.page-wrapper` beside it in which each screen
// renders its own `PageHeader` + `PageBody` (see `layout.tsx`). On wide screens
// a button at the foot of the sidebar folds it to a rail of icons (Tabler's
// `navbar-folded`) and back; the collapse on narrow screens is React state
// toggling Bootstrap's `show` class rather than Bootstrap's own JS — the SPA
// already owns the DOM, so vendoring a second script to add one class would buy
// nothing.

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import Spinner from "react-bootstrap/Spinner";

import { api } from "./api";
import { splitRoute, stepParam } from "./builder";
import { PoppedChats } from "./PoppedChats";
import { useChatWindows } from "./chatWindows";
import type { AuthStatusResponse } from "./client";
import { showAppOutcome } from "./appActions";
import { AppOutcomeToast, ApplicationsNav } from "./AppSidebar";
import {
  IconBolt,
  IconBroadcast,
  IconChartHistogram,
  IconChevronLeft,
  IconChevronRight,
  IconFolder,
  IconLogout,
  IconMoon,
  IconRobot,
  IconSettings,
  IconSun,
  IconTable,
  IconUsers,
  SaltcornLogo,
} from "./icons";
import { I18nProvider } from "./i18n";
import { useFoldedSidebar, useTheme } from "./layout";
import { LocalePicker } from "./screens/LocalePicker";
import { AgentChat } from "./screens/AgentChat";
import { AgentForm } from "./screens/AgentForm";
import { Agents } from "./screens/Agents";
import { Applications } from "./screens/Applications";
import { ApplicationForm } from "./screens/ApplicationForm";
import { ApplicationLibrary } from "./screens/ApplicationLibrary";
import { ApplicationTranslations } from "./screens/Translations";
import { ApplicationViews } from "./screens/ApplicationViews";
import { PageProperties } from "./screens/PageProperties";
import { ViewEditor } from "./screens/ViewEditor";
import { DbConnections } from "./screens/DbConnections";
import { FileManager } from "./screens/FileManager";
import { FileStores } from "./screens/FileStores";
import { FileStoreForm } from "./screens/FileStoreForm";
import { FirstUser } from "./screens/FirstUser";
import { GraphqlExplorer } from "./screens/GraphqlExplorer";
import { LlmProviders } from "./screens/LlmProviders";
import { LlmProviderForm } from "./screens/LlmProviderForm";
import { Login } from "./screens/Login";
import { Roles } from "./screens/Roles";
import { Settings } from "./screens/Settings";
import { StreamForm } from "./screens/StreamForm";
import { StreamObserve } from "./screens/StreamObserve";
import { Streams } from "./screens/Streams";
import { Tables } from "./screens/Tables";
import { TableData } from "./screens/TableData";
import { TableDetail } from "./screens/TableDetail";
import { RunDetail } from "./screens/RunDetail";
import { Triggers } from "./screens/Triggers";
import { TriggerForm } from "./screens/TriggerForm";
import { Users } from "./screens/Users";
import { WorkflowEditor } from "./screens/WorkflowEditor";
import { WorkflowRuns } from "./screens/WorkflowRuns";
import { T, useT } from "./i18n";
import { modelRedirect } from "./modelRedirect";

/** The authenticated user, as reported by `authStatus` / `login`. */
export type CurrentUser = NonNullable<AuthStatusResponse["current_user"]>;

/** Subscribe to `location.hash`, normalised to a path like `/tables`. */
function useHashRoute(): string {
  const read = () => window.location.hash.replace(/^#/, "") || "/tables";
  const [route, setRoute] = useState(read);
  useEffect(() => {
    const onChange = () => setRoute(read());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);
  return route;
}

/** Navigate by updating the hash (the router above reacts to it). */
export function navigate(path: string): void {
  window.location.hash = path;
}

/**
 * Where a file store is edited as code: the IDE (design §12.1).
 *
 * Not a hash route — the IDE is a **separate page** with its own bundle, served
 * at `/ide/`, because VS Code initializes once per page. So this is an ordinary
 * link that leaves the SPA, and the browser's Back button is what comes back.
 */
export function ideUrl(store: string, path?: string): string {
  const base = `/ide/?store=${encodeURIComponent(store)}`;
  // A file to open once the workbench is up — a model's program (Stan TODO §18).
  return path && path.trim() !== "" ? `${base}&path=${encodeURIComponent(path.trim())}` : base;
}

export function App() {
  const [status, setStatus] = useState<AuthStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.authStatus());
      setError(null);
    } catch {
      setError("Could not reach the server. Is it running?");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  if (error) {
    return (
      <div className="page page-center">
        <div className="container container-tight py-4">
          <div className="alert alert-danger">{error}</div>
        </div>
      </div>
    );
  }

  if (!status) {
    return (
      <div className="page page-center">
        <div className="container container-tight py-4 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </div>
    );
  }

  // The locale the *server* negotiated for this request (§16.1, D8), which is
  // the one it has already promised in `Content-Language`. The provider is
  // outside the three top-level states on purpose: the sign-in page is a page
  // too, and it has a language.
  return (
    <I18nProvider locale={status.locales?.current ?? "en"}>
      {!status.any_user_exists ? (
        <FirstUser onCreated={refresh} />
      ) : !status.current_user ? (
        <Login onLoggedIn={refresh} />
      ) : (
        <Shell
          user={status.current_user}
          locales={status.locales}
          onLogout={refresh}
        />
      )}
    </I18nProvider>
  );
}

/** One entry in the sidebar: where it goes, what it is called, and which routes
 * count as "here" (a detail screen is still its section). */
type NavItem = {
  href: string;
  label: string;
  icon: ReactNode;
  /** Route prefixes that light this entry up. */
  matches: string[];
};

/** The Data Layer section of the sidebar: the installation's shared furniture.
 * Applications are the other section, and are not entries here — that section is
 * about one application at a time, picked in the sidebar (`AppSidebar.tsx`). */
export const NAV: NavItem[] = [
  {
    href: "#/tables",
    label: "Tables",
    icon: <IconTable />,
    // The database connections list is part of this section rather than one of
    // its own, the way LLM providers hang off Agents: a connection exists to put
    // tables in the tables list, and the way to it is a button on that screen.
    matches: ["/tables", "/db-connections"],
  },
  {
    href: "#/file-stores",
    label: "Files",
    icon: <IconFolder />,
    matches: ["/file-stores", "/files"],
  },
  {
    href: "#/triggers",
    label: "Actions",
    icon: <IconBolt />,
    matches: ["/triggers"],
  },
  {
    href: "#/agents",
    label: "Agents",
    icon: <IconRobot />,
    // The providers list is part of this section rather than one of its own: an
    // LLM provider exists to be pointed at by an agent, and nothing else in the
    // admin UI has any use for one.
    matches: ["/agents", "/llm-providers"],
  },
  {
    // The Analytics UI: its own bundle under `/analytics/` (analytics TODO
    // A1.14), a full navigation rather than a hash route. Datasets, models and
    // workspaces live there; it replaced *Predictive models* in A3, and the old
    // `#/models/…` links redirect into its model editor (`modelRedirect.ts`).
    // Beside Agents rather than under Tables: a model is a question asked *of*
    // a table, and the section it belongs to is the one about answering
    // questions rather than the one about storing rows.
    href: "/analytics/",
    label: "Analytics",
    icon: <IconChartHistogram />,
    matches: [],
  },
  {
    href: "#/streams",
    label: "Streams",
    icon: <IconBroadcast />,
    // Between Triggers and Files, and that is the argument (§9): a stream is a
    // *source of events*, so it belongs beside the thing that listens to them
    // rather than beside the models. Everything above it in this section is at
    // rest — a table has rows, a file has bytes — and a stream is the one entry
    // that moves on its own.
    matches: ["/streams"],
  },
  {
    href: "#/users",
    label: "Users",
    icon: <IconUsers />,
    // Roles hang off this section rather than standing beside it, the way LLM
    // providers hang off Agents: a role exists to be held by a user, and the
    // way to the list is a button on the Users screen.
    matches: ["/users", "/roles"],
  },
  {
    href: "#/settings",
    label: "Settings",
    icon: <IconSettings />,
    // Last, and one entry however many sections it grows: settings are about
    // the *installation* rather than about anything in it, and an admin looks
    // for them in one place rather than under whichever thing they configure.
    matches: ["/settings"],
  },
];

/** The authenticated admin shell: Tabler's vertical layout around the screen. */
function Shell({
  user,
  locales,
  onLogout,
}: {
  user: CurrentUser;
  locales: AuthStatusResponse["locales"];
  onLogout: () => void;
}) {
  const { t } = useT();
  const route = useHashRoute();
  const [menuOpen, setMenuOpen] = useState(false);
  const [theme, toggleTheme] = useTheme();
  const [folded, toggleFolded] = useFoldedSidebar();
  // A docked chat covers the bottom-right corner of whatever is underneath it.
  // For most screens that is an overlay doing what an overlay does; for the
  // chat *screen* it would be a window sitting on the send button of the page's
  // own composer, so the page is told how much of the corner is taken and keeps
  // clear of it (`admin.css`). Minimized windows are two rows of pixels along
  // the very bottom and are not worth narrowing a transcript for.
  const docked = useChatWindows().filter(
    (chat) => chat.mode === "docked",
  ).length;
  const corner =
    docked === 0 ? "" : ` chat-corner-taken chat-corner-${Math.min(docked, 3)}`;

  // A tap on a sidebar link should close the sidebar it was in; on a wide
  // screen the collapse is not rendered as a drawer, so this is a no-op there.
  useEffect(() => setMenuOpen(false), [route]);

  // The applications list shows a build's news in its own banner, and the shell
  // shows it as a toast everywhere else — so news already read on the list is
  // cleared on the way out rather than following the admin to the next screen.
  // A build still running then reports when it finishes, wherever that is.
  const { path } = splitRoute(route);
  const previousPath = useRef(path);
  useEffect(() => {
    if (previousPath.current === "/applications" && path !== "/applications") {
      showAppOutcome(null);
    }
    previousPath.current = path;
  }, [path]);

  const logout = async () => {
    try {
      await api.logout();
    } finally {
      onLogout();
    }
  };

  return (
    <div className={`page${corner}`}>
      {/* `navbar-folded` is Tabler's own folded sidebar: the rail is
          `--tblr-sidebar-folded-width` wide with the link titles collapsed, and
          the page wrapper beside it takes the same offset. It stays folded
          until the button at the foot unfolds it — not `navbar-folded-hover`,
          which springs open under a passing pointer. Tabler owns both numbers,
          which is why nothing here (or in `admin.css`) restates the geometry. */}
      <aside
        className={`navbar navbar-vertical navbar-expand-lg${folded ? " navbar-folded" : ""
          }`}
        data-bs-theme="dark"
      >
        <div className="container-fluid">
          <button
            className="navbar-toggler"
            type="button"
            aria-controls="sidebar-menu"
            aria-expanded={menuOpen}
            aria-label={t("Toggle navigation")}
            onClick={() => setMenuOpen((open) => !open)}
          >
            <span className="navbar-toggler-icon" />
          </button>
          {/* No `navbar-brand-autodark` here: that class flips a monochrome
              logo to white for a dark sidebar, and this one has its own
              colours to keep. */}
          <div className="navbar-brand">
            <a
              href="#/tables"
              className="d-flex align-items-center gap-2"
              aria-label={t("Saltcorn")}
            >
              <div className="d-flex">
                <SaltcornLogo />
                {/* The wordmark is the widest thing in the sidebar; folded, the
                    rail is the logo mark's width and Tabler would crop this to
                    the stems of its first two letters. */}
                <div className="ms-2 sidebar-wide-only">
                  <div className="saltcorn-label"><T text="Saltcorn" /></div>
                  <div className="feldspar-label"><T text="Feldspar" /></div>
                </div>
              </div>
            </a>
          </div>
          {/* On a narrow screen the collapse is shut by default, so the account
              controls sit in this always-visible row instead of at the foot of
              the menu (where the wide layout keeps them). */}
          <div className="navbar-nav flex-row d-lg-none">
            <ThemeToggle theme={theme} onToggle={toggleTheme} />
            <div className="nav-item ms-2">
              <button
                type="button"
                className="nav-link px-0"
                onClick={() => void logout()}
                aria-label={t("Log out")}
                title={`Log out (${user.email})`}
              >
                <IconLogout className="icon-1" />
              </button>
            </div>
          </div>
          <div
            className={
              menuOpen
                ? "collapse navbar-collapse show"
                : "collapse navbar-collapse"
            }
            id="sidebar-menu"
          >
            <ul className="navbar-nav pt-lg-3">
              {/* Tabler's section titles: folded to a rail, each becomes a short
                  rule between the groups of icons. */}
              <li className="nav-section-title"><T text="Data Layer" /></li>
              {NAV.map((item) => {
                const active = item.matches.some((prefix) =>
                  route.startsWith(prefix),
                );
                return (
                  <li
                    key={item.href}
                    className={active ? "nav-item active" : "nav-item"}
                  >
                    <a
                      className={active ? "nav-link active" : "nav-link"}
                      href={item.href}
                      aria-current={active ? "page" : undefined}
                      // Folded, the icon is all there is to go on, so the name
                      // becomes the hover label. Unfolded it is already on
                      // screen and a tooltip would only repeat it.
                      title={folded ? item.label : undefined}
                    >
                      <span className="nav-link-icon d-md-none d-lg-inline-block">
                        {item.icon}
                      </span>
                      <span className="nav-link-title">{item.label}</span>
                    </a>
                  </li>
                );
              })}
              <li className="nav-section-title"><T text="Applications" /></li>
              <ApplicationsNav route={route} folded={folded} />
            </ul>
            {/* The nav list above is `flex-grow: 1` in a column collapse, so
                everything below it settles at the bottom of the sidebar.

                The fold switch is only offered on `lg` and up: below that the
                sidebar is a drawer, which has no width to give back. It is
                deliberately *not* Tabler's `sidebar-folded` toggle
                button: Tabler styles that one as the pin of a hover-unfolding
                rail, hiding it outright in a `navbar-folded` sidebar — which
                would leave a folded sidebar with no way to unfold it. */}
            {/* Folded, the `px-3` that gives these room beside a label is wider
                than a 4rem rail can spare, so they centre instead. That is a
                class swap here rather than a rule in `admin.css`: Bootstrap's
                spacing utilities are `!important`. */}
            <div
              className={`d-none d-lg-flex pb-2 ${folded ? "justify-content-center px-0" : "justify-content-end px-3"
                }`}
            >
              <button
                type="button"
                className="btn btn-icon btn-ghost-secondary"
                onClick={toggleFolded}
                aria-pressed={folded}
                aria-label={folded ? "Expand the sidebar" : "Fold the sidebar"}
                title={folded ? "Expand the sidebar" : "Fold the sidebar"}
              >
                {folded ? (
                  <IconChevronRight className="icon-2" />
                ) : (
                  <IconChevronLeft className="icon-2" />
                )}
              </button>
            </div>
            <div
              className={`d-none d-lg-block py-3 border-top ${folded ? "px-0" : "px-3"
                }`}
            >
              {/* Folded there is no room for an address, so the email moves
                  into the log-out button's tooltip (see `admin.css`). */}
              <div className="text-secondary text-truncate mb-2 sidebar-wide-only">
                {user.email}
              </div>
              <div className="d-flex align-items-center gap-2 flex-wrap">
                <button
                  type="button"
                  className="btn btn-outline-secondary btn-sm"
                  onClick={() => void logout()}
                  title={`Log out (${user.email})`}
                >
                  <IconLogout className="icon-2" />
                  <span className="sidebar-wide-only"><T text="Log out" /></span>
                </button>
                <ThemeToggle theme={theme} onToggle={toggleTheme} />
                {/* Nothing at all on an installation that serves one language
                    (D11): i18n is a thing an admin turns on, not a control
                    every installation grows. */}
                <LocalePicker user={user} locales={locales} folded={folded} />
              </div>
            </div>
          </div>
        </div>
      </aside>

      <div className="page-wrapper">
        <Screen route={route} user={user} />
        <footer className="footer footer-transparent d-print-none">
          <div className="container-xl">
            <div className="row text-center align-items-center flex-row-reverse">
              <div className="col-12 col-lg-auto mt-3 mt-lg-0">
                <span className="text-secondary"><T text="Saltcorn" /></span>
              </div>
            </div>
          </div>
        </footer>
      </div>

      {/* Outside the routed screen, and outside the page wrapper: a popped-out
          chat is furniture of the whole admin, and it stays in the corner while
          everything above changes underneath it (`PoppedChats.tsx`). */}
      <PoppedChats />
      {/* A build started from the sidebar finishes wherever the admin has got
          to by then; the applications list has its own banner for the news. */}
      {path !== "/applications" && <AppOutcomeToast />}
    </div>
  );
}

/** Light/dark switch. One button that shows the scheme it would switch *to*,
 * which is how Tabler's own header reads (it swaps two links; we swap an icon). */
function ThemeToggle({
  theme,
  onToggle,
}: {
  theme: string;
  onToggle: () => void;
}) {
  const dark = theme === "dark";
  return (
    <div className="nav-item">
      <button
        type="button"
        className="nav-link px-0"
        onClick={onToggle}
        title={dark ? "Enable light mode" : "Enable dark mode"}
        aria-label={dark ? "Enable light mode" : "Enable dark mode"}
      >
        {dark ? (
          <IconSun className="icon-1" />
        ) : (
          <IconMoon className="icon-1" />
        )}
      </button>
    </div>
  );
}

/** Resolve the current hash route to a screen.
 *
 * `user` reaches only the screens that are *about* the signed-in admin rather
 * than about a record — today the GraphQL explorer, which runs its queries under
 * that admin's own authority and has to say whose. */
/** Leave for another page — a model link that now lives in the Analytics UI.
 * `replace`, so Back does not return to a hash that only redirects again. */
function Redirect({ to }: { to: string }) {
  useEffect(() => {
    window.location.replace(to);
  }, [to]);
  return null;
}

function Screen({ route, user }: { route: string; user: CurrentUser }) {
  // Routes are matched on their path; a query is what a screen is opened with
  // (the builder's way back to `views/:name?step=n`).
  const { path, query } = splitRoute(route);
  const tableDataMatch = path.match(/^\/tables\/([^/]+)\/data$/);
  if (tableDataMatch) {
    return <TableData table={decodeURIComponent(tableDataMatch[1])} />;
  }
  const tableMatch = path.match(/^\/tables\/([^/]+)$/);
  if (tableMatch) {
    return <TableDetail table={decodeURIComponent(tableMatch[1])} />;
  }
  if (path === "/applications/new") {
    return <ApplicationForm />;
  }
  const graphqlMatch = path.match(/^\/applications\/([^/]+)\/graphql$/);
  if (graphqlMatch) {
    return (
      <GraphqlExplorer
        appId={decodeURIComponent(graphqlMatch[1])}
        user={user}
      />
    );
  }
  const viewEditMatch = path.match(
    /^\/applications\/([^/]+)\/views\/([^/]+)$/,
  );
  if (viewEditMatch) {
    return (
      <ViewEditor
        key={route}
        appId={decodeURIComponent(viewEditMatch[1])}
        name={decodeURIComponent(viewEditMatch[2])}
        initialStep={stepParam(query)}
      />
    );
  }
  // `pages/new` before `pages/:name/properties`; a page named "new" has its
  // properties at `pages/new/properties`, so the two never meet.
  const newPageMatch = path.match(/^\/applications\/([^/]+)\/pages\/new$/);
  if (newPageMatch) {
    return (
      <PageProperties key={path} appId={decodeURIComponent(newPageMatch[1])} name={null} />
    );
  }
  const pagePropertiesMatch = path.match(
    /^\/applications\/([^/]+)\/pages\/([^/]+)\/properties$/,
  );
  if (pagePropertiesMatch) {
    return (
      <PageProperties
        key={path}
        appId={decodeURIComponent(pagePropertiesMatch[1])}
        name={decodeURIComponent(pagePropertiesMatch[2])}
      />
    );
  }
  const translationsMatch = path.match(/^\/applications\/([^/]+)\/translations$/);
  if (translationsMatch) {
    return (
      <ApplicationTranslations appId={decodeURIComponent(translationsMatch[1])} />
    );
  }
  const libraryMatch = path.match(/^\/applications\/([^/]+)\/library$/);
  if (libraryMatch) {
    return <ApplicationLibrary appId={decodeURIComponent(libraryMatch[1])} />;
  }
  const viewsMatch = path.match(/^\/applications\/([^/]+)\/(views|pages)$/);
  if (viewsMatch) {
    return (
      <ApplicationViews
        appId={decodeURIComponent(viewsMatch[1])}
        tab={viewsMatch[2] as "views" | "pages"}
      />
    );
  }
  const editMatch = path.match(
    /^\/applications\/([^/]+)\/(edit|app-settings)$/,
  );
  if (editMatch) {
    return (
      <ApplicationForm
        appId={decodeURIComponent(editMatch[1])}
        tab={editMatch[2] === "app-settings" ? "app-settings" : "settings"}
      />
    );
  }
  if (path.startsWith("/applications")) {
    return <Applications />;
  }
  if (path === "/triggers/new") {
    return <TriggerForm />;
  }
  // "Create trigger" on a table's own page: the same form, with that table
  // already chosen as the one the trigger fires on.
  const triggerForTableMatch = path.match(/^\/triggers\/new\/([^/]+)$/);
  if (triggerForTableMatch) {
    return <TriggerForm table={decodeURIComponent(triggerForTableMatch[1])} />;
  }
  const triggerEditMatch = path.match(/^\/triggers\/([^/]+)\/edit$/);
  if (triggerEditMatch) {
    return <TriggerForm triggerId={decodeURIComponent(triggerEditMatch[1])} />;
  }
  // A workflow is a trigger **body** (§10.3, decision 1), so its editor and its
  // runs hang off the trigger's id rather than standing beside it as an entity
  // of their own — there is no `/workflows/…` because there is no workflow to
  // address without a trigger.
  const workflowMatch = path.match(/^\/triggers\/([^/]+)\/workflow$/);
  if (workflowMatch) {
    return <WorkflowEditor triggerId={decodeURIComponent(workflowMatch[1])} />;
  }
  const workflowRunsMatch = path.match(/^\/triggers\/([^/]+)\/runs$/);
  if (workflowRunsMatch) {
    return (
      <WorkflowRuns triggerId={decodeURIComponent(workflowRunsMatch[1])} />
    );
  }
  if (path.startsWith("/triggers")) {
    return <Triggers />;
  }
  if (path === "/streams/new") {
    return <StreamForm />;
  }
  const streamEditMatch = path.match(/^\/streams\/([^/]+)\/edit$/);
  if (streamEditMatch) {
    return <StreamForm streamId={decodeURIComponent(streamEditMatch[1])} />;
  }
  // Observe is keyed on the route, so switching between two streams' sockets
  // builds a new screen rather than feeding one stream's elements into the
  // other's tail.
  const streamObserveMatch = path.match(/^\/streams\/([^/]+)\/observe$/);
  if (streamObserveMatch) {
    return (
      <StreamObserve key={path} streamId={decodeURIComponent(streamObserveMatch[1])} />
    );
  }
  if (path.startsWith("/streams")) {
    return <Streams />;
  }
  if (path === "/file-stores/new") {
    return <FileStoreForm />;
  }
  const storeEditMatch = path.match(/^\/file-stores\/([^/]+)\/edit$/);
  if (storeEditMatch) {
    return <FileStoreForm storeId={decodeURIComponent(storeEditMatch[1])} />;
  }
  if (path.startsWith("/file-stores")) {
    return <FileStores />;
  }
  // `/files/<store>` opens at the root; `/files/<store>/<dir>` opens in a
  // directory, which is what an application row links to (§2.4).
  const filesMatch = path.match(/^\/files\/([^/]+)(?:\/(.*))?$/);
  if (filesMatch) {
    const dir = (filesMatch[2] ?? "")
      .split("/")
      .filter((s) => s.length > 0)
      .map(decodeURIComponent)
      .join("/");
    return (
      <FileManager store={decodeURIComponent(filesMatch[1])} initialDir={dir} />
    );
  }
  if (path === "/agents/new") {
    return <AgentForm />;
  }
  // A chat is addressed by the agent's **name**, not its id: it is what the run
  // history is keyed by (§11.4) and what the socket's `start` carries, so a
  // bookmarked chat URL says which agent it is.
  const agentChatMatch = path.match(/^\/agents\/([^/]+)\/chat$/);
  if (agentChatMatch) {
    return <AgentChat agent={decodeURIComponent(agentChatMatch[1])} />;
  }
  const agentEditMatch = path.match(/^\/agents\/([^/]+)\/edit$/);
  if (agentEditMatch) {
    return <AgentForm agentId={decodeURIComponent(agentEditMatch[1])} />;
  }
  if (path.startsWith("/agents")) {
    return <Agents />;
  }
  if (path === "/llm-providers/new") {
    return <LlmProviderForm />;
  }
  const providerEditMatch = path.match(/^\/llm-providers\/([^/]+)\/edit$/);
  if (providerEditMatch) {
    return (
      <LlmProviderForm providerId={decodeURIComponent(providerEditMatch[1])} />
    );
  }
  if (path.startsWith("/llm-providers")) {
    return <LlmProviders />;
  }
  if (path.startsWith("/db-connections")) {
    return <DbConnections />;
  }
  // The model screens moved to the Analytics UI's model editor (analytics
  // TODO A3.8); a bookmark to one of them lands there.
  const moved = modelRedirect(path);
  if (moved) {
    return <Redirect to={moved} />;
  }
  // A run is addressed by its own id, as `getRun` is: which workflow it is of is
  // the server's answer, not the URL's.
  const runMatch = path.match(/^\/runs\/([^/]+)$/);
  if (runMatch) {
    return <RunDetail runId={decodeURIComponent(runMatch[1])} />;
  }
  if (path.startsWith("/users")) {
    return <Users />;
  }
  if (path.startsWith("/roles")) {
    return <Roles />;
  }
  if (path.startsWith("/settings")) {
    return <Settings />;
  }
  return <Tables />;
}
