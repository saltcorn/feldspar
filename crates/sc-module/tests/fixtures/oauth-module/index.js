// A module whose action asks for its caller's consent at an OAuth 2.0 provider
// (TODO-imap M10.3): the tokens go into a row of its own `oa_accounts`.

const Table = () => require("@saltcorn/data/models/table").findOne({ name: "oa_accounts" });

module.exports = {
  sc_plugin_api_version: 1,
  tables: [
    {
      name: "oa_accounts",
      fields: [
        { name: "owner", type: "text" },
        { name: "email", type: "text" },
        { name: "auth", type: "text" },
        { name: "access_token", secret: true },
        { name: "refresh_token", secret: true },
        { name: "expires_at", type: "timestamp" },
        { name: "plain_token", type: "text" },
      ],
    },
  ],
  actions: {
    // `payload.request` is merged over a well-formed request, so a test can
    // spoil one part of it at a time.
    connect: {
      run: async ({ payload, user, oauth }) => {
        const account = await Table().getRow({ id: payload.account_id });
        if (!account || (!payload.anyone && account.owner !== String(user && user.id))) throw new Error("not your account");
        const provider = payload.provider || "https://login.example.com";
        const request = {
          authorization_url: `${provider}/authorize`,
          token_url: `${provider}/token`,
          client_id: "cid",
          client_secret: "csecret",
          scope: "mail offline_access",
          params: { login_hint: account.email, prompt: "select_account" },
          return_to: "/settings/mail",
          store: {
            table: "oa_accounts",
            id: account.id,
            fields: { access_token: "access_token", refresh_token: "refresh_token", expires_at: "expires_at" },
            set: { auth: "oauth2" },
          },
          ...(payload.request || {}),
        };
        try {
          return await oauth.begin(request);
        } catch (e) {
          return { error: e.message };
        }
      },
    },
  },
};
