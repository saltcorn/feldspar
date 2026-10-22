// A module that receives webhooks (TODO-payments P1.1): it records what it was
// sent, byte for byte, in a table of its own, and answers with a status the
// caller chooses.

const crypto = require("crypto");

module.exports = {
  sc_plugin_api_version: 1,
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
    // A bare function is its own handler, and may answer text.
    ping: async (req) => ({ status: 202, body: `pong ${req.text()}`, headers: { "x-pong": "1" } }),
    // A handler that throws is a 500, so the provider retries.
    broken: async () => {
      throw new Error("the provider's event could not be processed");
    },
  },
};
