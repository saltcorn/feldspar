// A module that grants users roles through `roles` (TODO-payments P3.3): one
// action that tries each kind of grant and says what the server answered.

const attempt = async (call) => {
  try {
    return { ok: await call() };
  } catch (e) {
    return { refused: String(e && e.message ? e.message : e) };
  }
};

module.exports = {
  sc_plugin_api_version: 1,
  actions: {
    grant_roles: {
      run: async ({ payload, roles }) => {
        const { user } = payload;
        // A month of role 70, as a subscription's period would be.
        const month = new Date(Date.now() + 30 * 24 * 3600 * 1000);
        const granted = await attempt(() =>
          roles.grant({ user, role: 70, ref: "sub_1", expires_at: month, note: "Basic" }),
        );
        // The renewal: the same ref, a later end and a better tier.
        const renewed = await attempt(() =>
          roles.grant({
            user: { id: user },
            role: 60,
            ref: "sub_1",
            expires_at: month.getTime() + 30 * 24 * 3600 * 1000,
          }),
        );
        const above = await attempt(() => roles.grant({ user, role: 40, ref: "sub_2" }));
        const admin = await attempt(() => roles.grant({ user, role: 1, ref: "sub_3" }));
        const got = await roles.get("sub_1");
        const revoked = await roles.revoke("sub_1");
        const missing = await roles.get("nothing");
        return { granted, renewed, above, admin, got, revoked, missing };
      },
    },
  },
};
