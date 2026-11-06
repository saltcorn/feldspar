// LLM providers list: every model endpoint an admin has connected, and the
// one the server's configuration file supplies, if it supplies one.
//
// The file-stores list with a different noun. The file's provider is the
// counterpart of a `--file-store` store: listed, usable, and read-only — it
// opens as View rather than Edit and has no Remove. There is no "connected"
// state to show, because connecting a provider builds an HTTP client and sends
// nothing — whether it *works* is a request, which is what a model's Test
// button makes.
//
// One provider is the **default**: what is used where nothing names a provider
// (a new application's builder agent, a translation, the agent form's first
// choice). The admin picks it here; until they do, it is the file's provider,
// else the first by name.
//
// The summary column deliberately renders whatever settings a backend declares
// rather than reaching for a `model` only some might have. That is the same
// discipline `FileStores` keeps, and it is what lets a backend added later
// appear here with no change: the values it shows are already redacted by the
// server, so an API key summarises as the sentinel and nothing here has to know
// which setting was the secret.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListLlmProvidersResponse } from "../client";
import { navigate } from "../App";
import { IconPlus } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { asString } from "../settings";
import { T, useT } from "../i18n";

type ProviderItem = ListLlmProvidersResponse[number];

/** A one-line summary of a provider's settings, from whatever its backend
 * declared. Long values are elided so one setting cannot push the rest of the
 * row off the screen. */
function configSummary(provider: ProviderItem): string {
  const config = provider.config;
  if (!config || typeof config !== "object") return "";
  return Object.entries(config as Record<string, unknown>)
    .map(([key, value]) => `${key}: ${elide(asString(value))}`)
    .join(", ");
}

/** A value shortened to fit a table cell. */
function elide(value: string, max = 48): string {
  return value.length > max ? `${value.slice(0, max)}…` : value;
}

export function LlmProviders() {
  const { t } = useT();
  const [providers, setProviders] = useState<ProviderItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    try {
      setProviders(await api.listLlmProviders());
    } catch {
      setError("Could not load LLM providers.");
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const remove = async (provider: ProviderItem) => {
    if (
      !window.confirm(
        t(
          'Remove the LLM provider "{name}"?\n\nAnything configured to use it will stop working until it is repointed.',
          { name: provider.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteLlmProvider(provider.id);
      await load();
    } catch (err) {
      // The server names what still references it, so surface its message
      // rather than a generic failure.
      setError(errorMessage(err, "Could not remove the LLM provider."));
    }
  };

  const makeDefault = async (provider: ProviderItem) => {
    setError(null);
    try {
      await api.setDefaultLlmProvider(provider.id);
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not make the LLM provider the default."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle={t("Automation")}
        title={t("LLM providers")}
        actions={
          <Button onClick={() => navigate("/llm-providers/new")}>
            <IconPlus className="icon-2" />
            <T text="New provider" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Backend" /></th>
                <th><T text="Settings" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {providers?.length === 0 && (
                <tr>
                  <td colSpan={4} className="text-muted">
                    <T text="No LLM providers yet. Add one to give an agent a model to talk to." />
                  </td>
                </tr>
              )}
              {providers?.map((provider) => (
                <tr key={provider.id}>
                  <td>
                    {provider.name}
                    {provider.is_default && (
                      <Badge bg="primary-lt" className="ms-2">
                        <T text="default" />
                      </Badge>
                    )}
                    {provider.from_config_file && (
                      <Badge bg="secondary-lt" className="ms-2">
                        <T text="configuration file" />
                      </Badge>
                    )}
                    {provider.description && (
                      <div className="text-muted small">{provider.description}</div>
                    )}
                  </td>
                  <td>{provider.backend}</td>
                  <td className="text-break small">{configSummary(provider)}</td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap align-items-center">
                      {!provider.is_default && (
                        <Button
                          size="sm"
                          variant="outline-secondary"
                          onClick={() => void makeDefault(provider)}
                        >
                          <T text="Make default" />
                        </Button>
                      )}
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#/llm-providers/${encodeURIComponent(provider.id)}/edit`}
                      >
                        {provider.from_config_file ? <T text="View" /> : <T text="Edit" />}
                      </Button>
                      {!provider.from_config_file && (
                        <Button
                          size="sm"
                          variant="outline-danger"
                          onClick={() => void remove(provider)}
                        >
                          <T text="Remove" />
                        </Button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>

        <p className="text-muted small mt-3">
          <T text="API keys are stored in the Saltcorn database and are never sent back to this screen — an existing key shows as ••••••••. They are not encrypted at rest, so treat database access as key access." />
        </p>
        {providers?.some((p) => p.from_config_file) && (
          <p className="text-muted small">
            <T text="A provider marked configuration file is set in the server's feldspar.toml. Its key stays in that file — it is not stored in the database or included in backups — and it can only be changed there." />
          </p>
        )}
      </PageBody>
    </>
  );
}
