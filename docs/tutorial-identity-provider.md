# Tutorial: let another service sign in with your application's users

Your staff already have accounts in a Feldspar application, `staff.example.com`. Now you want
them to sign in to something else with those same accounts: a Grafana, a wiki, an internal tool
behind a proxy. They should not need a second password, and when someone leaves, disabling them
in one place should be enough.

Feldspar can do this. An application can be an **OpenID Connect identity provider**: other
services ("clients", or "relying parties") send people to the application to sign in, and get back
who they are. Any software that supports "Generic OAuth" or "OpenID Connect" can use it without
Feldspar-specific code.

This tutorial turns the provider on and registers a client. It then puts
[oauth2-proxy](https://oauth2-proxy.github.io/oauth2-proxy/) in front of a service as the worked
example. The design is in [TECHNICAL_DESIGN.md §7.2](TECHNICAL_DESIGN.md).

## Step 1 — Turn the provider on

**Applications → Staff → Application settings → Identity provider**, and switch on **Let
other services sign people in with this application's users**. It saves as you go.

The tab shows two addresses:

- **Issuer**: `https://staff.example.com`. This is the application's own address; the provider
  lives on the application, not on the admin host.
- **Discovery URL**: `https://staff.example.com/.well-known/openid-configuration`. Most software
  needs only this. It lists every endpoint, the signing keys' address and what is supported.

While the switch is off, every one of these addresses answers 404.

**Who may sign in** is what the application's **Security** tab says: its minimum role, maximum
role and access condition. A user the application would refuse is refused here too. The
relying party is told `access_denied`. The check is made again at every token refresh and
userinfo call, so raising the minimum role locks people out of the other service as well.

## Step 2 — Register a client

On the same tab, **Register a client**:

- **Name**: what the consent page shows ("Allow *Grafana* to use your Staff account?").
- **Confidential**: on for anything with a server side (Grafana, a wiki, oauth2-proxy), which
  keeps a secret. Off for a single-page or mobile app, which cannot keep one and relies on PKCE
  alone. This cannot be changed later.
- **Redirect URIs**: where the service receives the result, exactly as it sends it, one per line.
  Nothing looser is accepted: no prefix, no wildcard, no trailing-slash leniency. They must be
  `https`, except `http://localhost` for development.
- **Scopes it may ask for**: none ticked means all of `openid`, `email`, `profile` and
  `offline_access`.
- **First-party**: skips the consent page, for a service you run yourself.
- **Further claims**: extra facts about the user, one `claim = field` per line. For example,
  `groups = role` gives the role's name as `groups`. `department = department` gives a field you
  added to the users table. They appear in the ID token and at userinfo.
- **Post-logout redirect URIs**: where signing out from the service may return to.

**Register** shows the **client secret once**. Copy it now. Only a hash is stored, so if it is
lost, use **New secret** on the client's row, which also stops the old one at once.

## Step 3 — oauth2-proxy in front of a service

oauth2-proxy puts a sign-in in front of any web service. With the client from Step 2
(redirect URI `https://tools.example.com/oauth2/callback`):

```sh
oauth2-proxy \
  --provider=oidc \
  --oidc-issuer-url=https://staff.example.com \
  --client-id=fdc_… \
  --client-secret=fdcs_… \
  --redirect-url=https://tools.example.com/oauth2/callback \
  --code-challenge-method=S256 \
  --scope="openid email profile" \
  --oidc-groups-claim=groups \
  --email-domain='*' \
  --cookie-secret=<32 random bytes> \
  --upstream=http://127.0.0.1:8080
```

Two of these settings are required:

- `--code-challenge-method=S256`: Feldspar requires PKCE of every client, including confidential
  ones.
- The issuer is the application's address exactly as the discovery document says it, with no
  trailing slash.

Open the service. oauth2-proxy sends you to the application's sign-in page. It is the
application's own sign-in, so a second factor is asked for if the account has one, and the
"Sign in with …" buttons of the external providers it offers are there too. Next comes a consent
page, once per user and client. Then you are back at the service, signed in.
`/oauth2/userinfo` on the proxy shows what it was told:

```json
{"user":"cf884680-42e4-4ee5-ad08-429cb366748c","email":"admin@example.com","groups":["Admin"]}
```

`user` is the account's id (`sub`). It stays the same for as long as the account exists, even
if the email address changes. Use it, not the email, as the key in the other service.

Other software takes the same settings under other names. Grafana's `[auth.generic_oauth]` has
`client_id`, `client_secret`, `auth_url`, `token_url` and `api_url`, which are the authorize,
token and userinfo addresses from the discovery document. It also needs `use_pkce = true`, and
reads a role from a claim with `role_attribute_path`.

## What the other service is given

| Scope | Claims |
| --- | --- |
| `openid` | `sub` (the user's id), plus the client's further claims |
| `email` | `email`, `email_verified` |
| `profile` | `name` (from a `name` or `full_name` field), and any of `given_name`, `family_name`, `nickname`, `preferred_username`, `picture`, `locale`, … that the users table has a field of that name for |
| `offline_access` | nothing more: a refresh token is always issued |

The ID token is signed with RS256 by default (ES256 is published too). The signing keys are at
`/_feldspar/oidc/jwks`.

The access token is short-lived (10 minutes) and is good for `/userinfo` **only**. It cannot
call your application's API, the admin API or the MCP server. The refresh token lasts 30 days
unused and is replaced at every use. If an old one is presented again, everything issued from
that sign-in is revoked, because one of the two presenters must have stolen it.

## Signing out, and ending access

- **The service signs the person out** at `end_session_endpoint`
  (`/_feldspar/oidc/logout`), returning to a registered post-logout redirect URI. With an
  `id_token_hint` this happens at once; without one, the person is asked first, so a link on
  another site cannot sign anyone out.
- **The service gives up a token** at `/_feldspar/oidc/revoke`. A refresh token ends everything
  from that sign-in; an access token ends alone.
- **You end it**, from Feldspar:
  - Changing a user's password, turning their second factor on or off, or disabling them
    ends every token they hold, at every service.
  - Deleting the user does too.
  - **Revoke** beside a row under **Signed-in users** withdraws that user's consent to that
    service and ends its tokens.
  - Deleting a client ends everything it was given.

## Keys and backups

One key set signs for every application on the server, and is made the first time it is needed.
**Rotate keys** makes new keys. The old ones stay published for a day, so tokens they signed
still verify.

The private keys are stored encrypted with the instance key (`secrets.key`). A backup of an
application that is a provider carries its clients and the encrypted keys, together with
`secrets.key`. If you restore without that key, the old keys cannot be opened. Feldspar then
says so, never serves them, and makes new ones; relying parties fetch the new keys from the
JWKS on their own.

## Not supported, on purpose

Feldspar does not support the implicit or hybrid flows, the password and client-credentials
grants, or `plain` PKCE. It has no dynamic client registration (an administrator registers every
client), no encrypted ID tokens and no front- or back-channel logout. The admin host is never an
issuer.
