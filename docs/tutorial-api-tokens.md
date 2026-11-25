# Tutorial: call your application's API from a script

Your users keep their customers in a Feldspar application, `crm.example.com`. One of them wants a
nightly job that exports their customers to a spreadsheet, and another wants a shell script that
adds a row when a form on their website is filled in. Neither should put their password in a
script, and neither should be able to do anything from it that they could not do signed in.

That is what an **API token** is for. A user creates one on the application, and a script sends
it with each request. The application then treats the request as coming from that user: the same
access settings, the same table rules, the same ownership formulas. Revoking the token stops the
script without touching the user's password.

The design is in [TECHNICAL_DESIGN.md §13.7](TECHNICAL_DESIGN.md). The operator's side, the
`feldspar api-token` command and the log lines, is in [OPERATIONS.md §8.2](OPERATIONS.md).

## Step 1 — Turn tokens on

Tokens are off until an admin turns them on for an application. **Applications → CRM →
Application settings → Security → API tokens**, and switch on **Users may create API tokens**.

**Who may create one** is any signed-in user unless you pick a minimum role. Using a token is not
held to that role: a token works for as long as its user may use the application at all. The
**Who may use this application** settings above still apply to token holders.

Below the form, **Users' API tokens** lists every token on the application, whose it is, when it
was last used, and a **Revoke** button.

## Step 2 — Create a token

The user signs in to the application and opens:

```
https://crm.example.com/_feldspar/api-tokens/page
```

They name the token ("nightly export"), optionally give it an expiry in days, and press **Create
token**. The page shows the secret once. It begins `fsat_`, and nothing can show it again, so it
goes straight into the script's secret store.

A React application can offer the same thing in its own settings screen. The generated client has
three methods for the signed-in user:

```ts
const { tokens, may_create } = await api.apiTokenList();
const { token, secret } = await api.apiTokenCreate({ label: "nightly export", expires_in_days: 90 });
await api.apiTokenRevoke(token.id);
```

These work with the application's own sign-in session only. A request that itself carries a token
cannot create tokens.

## Step 3 — Use it

Send it in the `Authorization` header:

```bash
TOKEN=fsat_…
curl -H "Authorization: Bearer $TOKEN" https://crm.example.com/api/whoami
curl -H "Authorization: Bearer $TOKEN" https://crm.example.com/api/customers
curl -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name": "Ada", "email": "ada@example.com"}' \
  https://crm.example.com/api/customers
```

A write needs no CSRF token: that check protects cookies, and a token request uses none. The same
header works for GraphQL (`POST /graphql`), custom queries, uploads and exposed agents. It does not
work for the live socket.

The token gets exactly what its user gets. If the `customers` table's ownership formula is
`owner_email === user.email`, the export sees that user's customers and nobody else's, and an
insert with someone else's email is refused.

## When it stops working

The answer is a `401` that says why:

- the token was revoked, by its user or an admin, or it expired;
- the user was disabled or deleted;
- it was created on another application (a token works on one application only);
- the application has turned tokens off;
- it is an administration token (`fspk_…`, for the MCP server), which an application does not take.

A token never falls back to treating the request as the public: a script with a bad token gets an
error, not a smaller set of rows.

## A token for a service account

A job that belongs to nobody in particular should have an account of its own. The operator
creates the account and mints the token from a terminal:

```bash
feldspar add-user export-bot@example.com --role 80
feldspar api-token create --app crm --label "nightly export" --email export-bot@example.com
```

The secret is printed alone on stdout, so `token=$(feldspar api-token create …)` captures it.
`feldspar api-token list` and `feldspar api-token revoke ID` do the rest.
