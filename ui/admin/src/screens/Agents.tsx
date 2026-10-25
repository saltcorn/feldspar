// The agents list: every agent an admin has configured, and the way into each
// one's chat.
//
// The triggers list with a different noun, including the part that matters most
// on both: an agent that does not validate is **still here**, marked, with the
// reason in the badge's tooltip. Hiding it would hide the screen the repair
// happens on — and unlike a trigger, a broken agent is not silently inert: it is
// the thing an admin is about to try to talk to.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { noteApplicationsChanged } from "../appActions";
import type { ListAgentsResponse } from "../client";
import { navigate } from "../App";
import { IconPlus } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { roleLabel, useRoles } from "../roles";
import { T, useT } from "../i18n";

type AgentItem = ListAgentsResponse[number];

/** What an agent may do, in the words of the traits it was given. Empty means
 * an agent that only talks, which is a real and useful shape. */
function traitSummary(agent: AgentItem): string {
  if (agent.traits.length === 0) return "—";
  return agent.traits
    .map((enabled) => {
      const config = enabled.config as Record<string, unknown> | null;
      // The first configured value is what a trait is *about* — the table, the
      // trigger — so "query_table (books)" reads without this screen knowing
      // which setting any particular trait calls its target.
      const target = config
        ? Object.values(config).find((v) => typeof v === "string" && v !== "")
        : undefined;
      return target ? `${enabled.trait} (${String(target)})` : enabled.trait;
    })
    .join(", ");
}

export function Agents() {
  const { t } = useT();
  const [agents, setAgents] = useState<AgentItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const roles = useRoles();

  const load = async () => {
    try {
      setAgents(await api.listAgents());
    } catch (err) {
      setError(errorMessage(err, "Could not load the agents."));
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const remove = async (agent: AgentItem) => {
    if (
      !window.confirm(
        t(
          'Remove the agent "{name}"?\n\nIts past conversations are kept — they are a record of what happened.',
          { name: agent.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteAgent(agent.id);
      await load();
      // It may have been an application's builder, whose chat link the sidebar
      // offers.
      noteApplicationsChanged();
    } catch (err) {
      setError(errorMessage(err, "Could not remove the agent."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Agents"
        title={t("Agents")}
        actions={
          <>
            <Button
              variant="outline-secondary"
              onClick={() => navigate("/llm-providers")}
            >
              <T text="LLM providers" />
            </Button>
            <Button onClick={() => navigate("/agents/new")}>
              <IconPlus className="icon-2" />
              <T text="New agent" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Provider" /></th>
                <th><T text="Traits" /></th>
                <th><T text="Who may chat" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {agents?.length === 0 && (
                <tr>
                  <td colSpan={5} className="text-muted">
                    <T text="No agents yet. Connect an LLM provider, then add an agent to talk to it." />
                  </td>
                </tr>
              )}
              {agents?.map((agent) => (
                <tr key={agent.id}>
                  <td>
                    {agent.name}
                    {agent.description && (
                      <div className="text-muted small">{agent.description}</div>
                    )}
                    {/* The reason lives in the tooltip: it is a sentence, and a
                        sentence in a table cell would push everything else off
                        the row. */}
                    {agent.error && (
                      <StatusBadge tone="red" title={agent.error} className="mt-1">
                        <T text="Not usable" />
                      </StatusBadge>
                    )}
                  </td>
                  <td>
                    {agent.provider}
                    {agent.model && <div className="text-muted small">{agent.model}</div>}
                  </td>
                  <td className="text-break small">{traitSummary(agent)}</td>
                  <td>{agent.min_role == null ? "Admin only" : roleLabel(agent.min_role, roles)}</td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap align-items-center">
                      <Button
                        size="sm"
                        variant="primary"
                        href={`#/agents/${encodeURIComponent(agent.name)}/chat`}
                      >
                        <T text="Chat" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#/agents/${encodeURIComponent(agent.name)}/runs`}
                      >
                        <T text="Runs" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#/agents/${encodeURIComponent(agent.id)}/edit`}
                      >
                        <T text="Edit" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove(agent)}
                      >
                        <T text="Remove" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
        
      </PageBody>
    </>
  );
}
