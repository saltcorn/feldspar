// A module that receives webhooks (TODO-payments P1.1): it records what it was
// sent, byte for byte, in a table of its own, and answers with a status the
// caller chooses. It also reads a table its settings name (P4.1's `reads`), and
// asks to read `users`, which no setting names.

const crypto = require("crypto");

/** What `call` did: its answer, or the refusal. */
const outcome = async (call) => {
  try {
    return { ok: await call() };
  } catch (e) {
    return { refused: e.message };
  }
};

module.exports = {
  sc_plugin_api_version: 1,
  configuration_workflow: () => ({
    steps: [{ name: "Lookup", form: { fields: [{ name: "lookup_table", label: "Lookup table", type: "String" }] } }],
  }),
  // The table its settings name — and `users`, which they do not, so is not admitted.
  reads: (configuration) => [configuration.lookup_table, "users"].filter(Boolean),
  tables: [
    {
      name: "hook_deliveries",
      fields: [
        { name: "hook", type: "text" },
        { name: "rest", type: "text" },
        { name: "body_sha256", type: "text" },
        { name: "body_length", type: "int" },
        { name: "signature", type: "text" },
        { name: "content_type", type: "text" },
        { name: "trespass", type: "text" },
      ],
    },
  ],
  webhooks: {
    deliver: {
      description: "Records a delivery",
      handle: async (req, ctx) => {
        // `rawBody` is the bytes as sent: a signature is computed over them,
        // so they must not have been parsed and re-serialised on the way.
        const sha = crypto.createHash("sha256").update(req.rawBody).digest("hex");
        let trespass = null;
        if (req.headers["x-trespass"]) {
          // Each of these is refused: a table the module did not declare,
          // and its own SQL.
          const outcomes = [];
          for (const attempt of [
            () => ctx.db.users.insert({ email: "intruder@example.com", role: 1 }),
            () => ctx.db.sql("select 1"),
          ]) {
            try {
              await attempt();
              outcomes.push("allowed");
            } catch (e) {
              outcomes.push(e.message);
            }
          }
          trespass = outcomes.join(" | ");
        }
        const row = await ctx.db.hook_deliveries.insert({
          hook: "deliver",
          rest: req.rest,
          body_sha256: sha,
          body_length: req.rawBody.length,
          signature: req.headers["x-signature"] || null,
          content_type: req.headers["content-type"] || null,
          trespass,
        });
        ctx.log.info(`delivery ${row.id} of ${req.rawBody.length} bytes`);
        const status = Number(req.headers["x-answer-status"] || 200);
        return { status, body: { received: req.rawBody.length, sha256: sha, method: req.method } };
      },
    },
    // Reads the table its settings name, tries to write it, and tries to read
    // `users`, answering what each did.
    lookup: {
      description: "Reads the lookup table",
      handle: async (req, ctx) => {
        const table = ctx.configuration.lookup_table;
        return {
          status: 200,
          body: {
            read: await outcome(() => ctx.db[table].count()),
            write: await outcome(() => ctx.db[table].insert({ note: "from the webhook" })),
            users: await outcome(() => ctx.db.users.count()),
          },
        };
      },
    },
    // A bare function is its own handler, and may answer text.
    ping: async (req) => ({ status: 202, body: `pong ${req.text()}`, headers: { "x-pong": "1" } }),
    // A handler that throws is a 500, so the provider retries.
    broken: async () => {
      throw new Error("the provider's event could not be processed");
    },
  },
};
