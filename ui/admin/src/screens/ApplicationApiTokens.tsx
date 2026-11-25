// The "API tokens" part of an application's Security tab (TODO AT1): the
// tokens its users minted for themselves, whose each is, and a Revoke button.
//
// The switch and the minting role are fields of the application form, saved
// with it; this panel comes after the form, as the identity provider's does,
// because a revoke is a request of its own. An admin does not mint here: a
// token runs as the user who holds it, and only that user (or the operator's
// `feldspar api-token create`) makes one.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListApplicationApiTokensResponse } from "../client";
import { appUrl } from "../adminAddress";
import { T, useT } from "../i18n";

type Token = ListApplicationApiTokensResponse[number];

/** The badge colour of a token's state. */
function stateVariant(state: string): string {
  return state === "live" ? "success" : "secondary";
}

/** A token's state, in words the catalogue has. */
function stateLabel(state: string, t: (text: string) => string): string {
  switch (state) {
    case "live":
      return t("live");
    case "revoked":
      return t("revoked");
    default:
      return t("expired");
  }
}

/** A timestamp as a short local date and time, or a dash. */
function when(at: string | null | undefined): string {
  return at ? new Date(at).toLocaleString() : "—";
}

export function ApplicationApiTokensPanel({
  appId,
  subdomain,
  enabled,
}: {
  appId: string;
  subdomain: string;
  /** The switch as the form has it now, saved or about to be. */
  enabled: boolean;
}) {
  const { t } = useT();
  const [tokens, setTokens] = useState<Token[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = async () => {
    try {
      setTokens(await api.listApplicationApiTokens(appId));
      setError(null);
    } catch (err) {
      setError(errorMessage(err, "Could not load the API tokens."));
    }
  };
  useEffect(() => {
    void reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  const revoke = async (token: Token) => {
    if (!window.confirm(t("Revoke {label}? Scripts using it stop working at once.", { label: token.label }))) {
      return;
    }
    setBusy(true);
    try {
      await api.revokeApplicationApiToken(appId, token.id);
      await reload();
    } catch (err) {
      setError(errorMessage(err, "Could not revoke the token."));
    } finally {
      setBusy(false);
    }
  };

  const page = `${appUrl(subdomain)}/_feldspar/api-tokens/page`;

  return (
    <Card className="mb-3">
      <Card.Header>
        <T text="Users' API tokens" />
      </Card.Header>
      <Card.Body>
        {!enabled && (
          <Alert variant="secondary">
            <T text="API tokens are off: a token is refused, and nobody can create one. Users can still see and revoke theirs." />
          </Alert>
        )}
        {enabled && (
          <p className="text-muted">
            <T
              text="Users create their own at {page}, or with the generated client's {method}."
              values={{
                page: (
                  <a href={page} target="_blank" rel="noreferrer">
                    {page}
                  </a>
                ),
                method: <code>apiTokenCreate</code>,
              }}
            />
          </p>
        )}
        {error && <Alert variant="danger">{error}</Alert>}
        {tokens && tokens.length === 0 && (
          <p className="text-muted mb-0">
            <T text="Nobody has an API token on this application." />
          </p>
        )}
        {tokens && tokens.length > 0 && (
          <Table size="sm" responsive className="mb-0">
            <thead>
              <tr>
                <th>{t("Name")}</th>
                <th>{t("User")}</th>
                <th>{t("State")}</th>
                <th>{t("Created")}</th>
                <th>{t("Expires")}</th>
                <th>{t("Last used")}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {tokens.map((token) => (
                <tr key={token.id}>
                  <td>{token.label}</td>
                  <td>{token.email ?? t("(deleted user)")}</td>
                  <td>
                    <Badge bg={stateVariant(token.state)}>{stateLabel(token.state, t)}</Badge>
                  </td>
                  <td>{when(token.created_at)}</td>
                  <td>{token.expires_at ? when(token.expires_at) : t("never")}</td>
                  <td>{token.last_used_at ? when(token.last_used_at) : t("never")}</td>
                  <td className="text-end">
                    {token.state === "live" && (
                      <Button
                        size="sm"
                        variant="outline-danger"
                        disabled={busy}
                        onClick={() => void revoke(token)}
                      >
                        <T text="Revoke" />
                      </Button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card.Body>
    </Card>
  );
}
