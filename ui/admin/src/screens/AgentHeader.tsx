// The headers of the agents area.
//
// `AgentAreaHeader` is the three lists' — agents, LLM providers, MCP servers —
// with a tab for each, as an application's screens have (`ApplicationTabs`).
// `AgentHeader` and `AgentTabs` are one agent's screens' — chat, runs,
// memories and settings: "Agent" above its name, Back, and a tab for each.

import { useEffect, useState, type ReactNode } from "react";
import Button from "react-bootstrap/Button";

import { api } from "../api";
import { navigate } from "../App";
import {
  AGENT_AREA_TABS,
  AGENT_SECTIONS,
  agentSectionHref,
  type AgentAreaTab,
  type AgentSection,
} from "../agentNav";
import { IconArrowLeft } from "../icons";
import { PageHeader } from "../layout";
import { T, useT } from "../i18n";

/** The header of one of the area's lists, `action` its own New button. */
export function AgentAreaHeader({ action }: { action: ReactNode }) {
  const { t } = useT();
  return <PageHeader pretitle={t("Automation")} title={t("Agents")} actions={action} />;
}

/** The tabs between the area's lists, at the top of the page body. */
export function AgentAreaTabs({ active }: { active: AgentAreaTab }) {
  const { t } = useT();
  return (
    <ul className="nav nav-pills mb-3">
      {AGENT_AREA_TABS.map((tab) => (
        <li className="nav-item" key={tab.id}>
          <a
            className={`nav-link${tab.id === active ? " active" : ""}`}
            aria-current={tab.id === active ? "page" : undefined}
            href={tab.href}
          >
            {tab.label(t)}
          </a>
        </li>
      ))}
    </ul>
  );
}

/** The page header of one agent's screen. `actions` go after Back; the
 * browser's tab names the screen ("help desk · Runs"). */
export function AgentHeader({
  agent,
  section,
  actions,
}: {
  agent: string;
  section: AgentSection;
  actions?: ReactNode;
}) {
  const { t } = useT();
  const screen = AGENT_SECTIONS.find((s) => s.id === section)?.label(t) ?? "";
  return (
    <PageHeader
      pretitle={t("Agent")}
      title={agent}
      docTitle={`${agent} · ${screen}`}
      actions={
        <>
          <Button variant="outline-secondary" onClick={() => navigate("/agents")}>
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
          {actions}
        </>
      }
    />
  );
}

/** The agent's id, for the link to its settings: `known` when the screen has
 * it, otherwise looked up by name. `undefined` until found, and for an agent
 * that no longer exists. */
export function useAgentId(agent: string, known?: string): string | undefined {
  const [found, setFound] = useState<string | undefined>(undefined);
  useEffect(() => {
    if (known) return;
    let cancelled = false;
    setFound(undefined);
    api
      .listAgents()
      .then((agents) => {
        if (!cancelled) setFound(agents.find((a) => a.name === agent)?.id);
      })
      .catch(() => {
        // Without it the Settings tab is not a link; the screen still works.
      });
    return () => {
      cancelled = true;
    };
  }, [agent, known]);
  return known ?? found;
}

/** The tabs between the agent's screens. `compact` is the chat's top bar,
 * which is one line. */
export function AgentTabs({
  agent,
  agentId,
  active,
  compact = false,
}: {
  agent: string;
  agentId?: string;
  active: AgentSection;
  compact?: boolean;
}) {
  const { t } = useT();
  const id = useAgentId(agent, agentId);
  return (
    <ul
      className={compact ? "nav nav-pills flex-nowrap agent-tabs-compact" : "nav nav-pills mb-3"}
      aria-label={t("Agent")}
    >
      {AGENT_SECTIONS.map((section) => {
        const href = agentSectionHref(section.id, agent, id);
        const current = section.id === active;
        const linkClass = `nav-link${current ? " active" : ""}${href ? "" : " disabled"}${
          compact ? " py-1 px-2" : ""
        }`;
        return (
          <li className="nav-item" key={section.id}>
            <a
              className={linkClass}
              aria-current={current ? "page" : undefined}
              aria-disabled={href ? undefined : true}
              href={href ?? undefined}
            >
              {section.label(t)}
            </a>
          </li>
        );
      })}
    </ul>
  );
}
