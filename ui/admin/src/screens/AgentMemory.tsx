// What one agent remembers (TODO-agent-skills AS8.3): the memories its
// `memory` trait stored — personal ones for every user, and shared ones — and
// the consolidated preferences a `consolidate_memory` trigger wrote.
//
// The admin can read and search them, and delete one: a person asking for
// what an agent knows about them to be forgotten is the reason this screen
// exists. Delete asks first, because nothing else keeps a copy.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import type { ListAgentMemoriesResponse, ListUsersResponse } from "../client";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { when } from "./WorkflowRuns";
import { T, useT } from "../i18n";

type MemoryItem = ListAgentMemoriesResponse[number];

/** Who a memory is about, by email where the user can be looked up. */
export function memoryUserLabel(memory: MemoryItem, users: ListUsersResponse): string {
  if (!memory.user_id) return "—";
  return users.find((u) => u.id === memory.user_id)?.email ?? memory.user_id;
}

export function AgentMemory({ agent }: { agent: string }) {
  const { t } = useT();
  const [memories, setMemories] = useState<MemoryItem[] | null>(null);
  const [users, setUsers] = useState<ListUsersResponse>([]);
  const [search, setSearch] = useState("");
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(
    async (phrase: string) => {
      try {
        setMemories(
          await api.listAgentMemories({
            agent,
            search: phrase.trim() || undefined,
          }),
        );
        setError(null);
      } catch (err) {
        setError(errorMessage(err, "Could not load what this agent remembers."));
      }
    },
    [agent],
  );

  useEffect(() => {
    void load("");
  }, [load]);

  useEffect(() => {
    api
      .listUsers()
      .then(setUsers)
      .catch(() => setUsers([]));
  }, []);

  const remove = async (memory: MemoryItem) => {
    if (!window.confirm(t("Delete this memory? The agent will not remember it again."))) {
      return;
    }
    try {
      await api.deleteAgentMemory(memory.id);
      await load(search);
    } catch (err) {
      setError(errorMessage(err, "Could not delete that memory."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Agent"
        title={`${agent} · ${t("memory")}`}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/agents")}>
              <IconArrowLeft className="icon-2" />
              <T text="Back" />
            </Button>
            <Button
              variant="outline-secondary"
              href={`#/agents/${encodeURIComponent(agent)}/runs`}
            >
              <T text="Runs" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <Form
          className="d-flex gap-2 mb-3"
          onSubmit={(e) => {
            e.preventDefault();
            void load(search);
          }}
        >
          <Form.Control
            type="search"
            aria-label={t("Search memories")}
            placeholder={t("Search memories")}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
          <Button type="submit" variant="outline-primary">
            <T text="Search" />
          </Button>
        </Form>

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Written" /></th>
                <th><T text="User" /></th>
                <th><T text="Kind" /></th>
                <th><T text="Topic" /></th>
                <th><T text="Contents" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {memories?.length === 0 && (
                <tr>
                  <td colSpan={6} className="text-muted">
                    <T text="This agent does not remember anything yet." />
                  </td>
                </tr>
              )}
              {memories?.map((memory) => (
                <tr key={memory.id}>
                  <td className="small text-nowrap">{when(memory.written_at)}</td>
                  <td className="small">{memoryUserLabel(memory, users)}</td>
                  <td>
                    <StatusBadge tone={memory.personal ? "blue" : "secondary"}>
                      {memory.kind} · {memory.personal ? t("personal") : t("shared")}
                    </StatusBadge>
                  </td>
                  <td className="small">{memory.topic || "—"}</td>
                  <td style={{ whiteSpace: "pre-wrap", maxWidth: "32rem" }}>
                    {memory.contents}
                  </td>
                  <td className="text-end">
                    <Button
                      size="sm"
                      variant="outline-danger"
                      onClick={() => void remove(memory)}
                    >
                      <T text="Delete" />
                    </Button>
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
