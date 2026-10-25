// A module that grants users LLM credits through `credits` (TODO-payments
// P6.4): one action that tries each operation and says what the server
// answered.

const attempt = async (call) => {
  try {
    return { ok: await call() };
  } catch (e) {
    return { refused: String(e && e.message ? e.message : e) };
  }
};

module.exports = {
  sc_plugin_api_version: 1,
  // The ledger, created when the module loads; and a name that is not one.
  core_tables: ["llm_credits", "invoices"],
  actions: {
    grant_credits: {
      run: async ({ payload, credits }) => {
        const { user } = payload;
        // A month's allowance, keyed by the invoice that paid for it.
        const month = new Date(Date.now() + 30 * 24 * 3600 * 1000);
        const granted = await attempt(() =>
          credits.grant({ user, amount: 100, key: "invoice:in_1", expires_at: month, kind: "allowance" }),
        );
        // The same webhook delivered twice: the same key does nothing.
        const replayed = await attempt(() =>
          credits.grant({ user: { id: user }, amount: 100, key: "invoice:in_1" }),
        );
        const topUp = await attempt(() => credits.grant({ user, amount: "2.5", key: "pack:7", kind: "top_up" }));
        const keyless = await attempt(() => credits.grant({ user, amount: 5 }));
        const threshold = await attempt(() => credits.setThreshold(user, 10));
        const balance = await credits.balance(user);
        // The next month's allowance, carrying at most 50 of this month's over (P7.1).
        const twoMonths = new Date(Date.now() + 60 * 24 * 3600 * 1000);
        const renewed = await attempt(() =>
          credits.grant({ user, amount: 100, key: "invoice:in_2", expires_at: twoMonths, kind: "allowance", rollover: { from: "invoice:in_1", cap: 50 } }),
        );
        // There is no debit: only agent runs spend credits.
        const debit = typeof credits.debit;
        return { granted, replayed, topUp, keyless, threshold, balance, renewed, debit };
      },
    },
  },
};
