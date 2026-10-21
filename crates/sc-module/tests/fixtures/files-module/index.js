// A module that keeps files where its declared file field says — the store
// `fm_files`, folder `docs/` — from a service and from an action (TODO-imap M6).

const MB = 1024 * 1024;

/** The byte at `i` of the big file: a pattern that a shifted or truncated copy
 * would not reproduce. */
const byteAt = (i) => (i * 31 + (i >> 16)) & 0xff;

/** 20 MB as a stream of 64 kB chunks, generated as it is read. */
async function* bigStream(size) {
  for (let at = 0; at < size; at += 64 * 1024) {
    const chunk = new Uint8Array(Math.min(64 * 1024, size - at));
    for (let i = 0; i < chunk.length; i++) chunk[i] = byteAt(at + i);
    yield chunk;
  }
}

/** Whether `bytes` is the big file, checked by sampling. */
const isBig = (bytes, size) => {
  if (bytes.length !== size) return false;
  for (let i = 0; i < size; i += 4099) if (bytes[i] !== byteAt(i)) return false;
  return bytes[size - 1] === byteAt(size - 1);
};

const refusal = async (attempt) => {
  try {
    await attempt();
    return "allowed";
  } catch (e) {
    return e.message;
  }
};

module.exports = {
  sc_plugin_api_version: 1,
  tables: [
    {
      name: "fm_docs",
      fields: [
        { name: "title", type: "text" },
        { name: "doc", file_store: "fm_files", file_folder: "docs" },
      ],
    },
    {
      name: "fm_results",
      fields: [
        { name: "step", type: "text" },
        { name: "note", type: "text" },
      ],
    },
  ],
  actions: {
    store_doc: {
      run: async ({ payload, files }) => {
        const path = await files.write("fm_files", payload.path, payload.text, {
          contentType: "text/plain",
        });
        const back = await files.read("fm_files", path, { encoding: "utf8" });
        const outside = await refusal(() => files.write("fm_files", "elsewhere/x.txt", "x"));
        return { path, back, outside };
      },
    },
  },
  services: {
    keeper: {
      description: "Stores files in its own area once, and records what happened",
      start: async (ctx) => {
        const note = (step, text) => ctx.db.fm_results.insert({ step, note: String(text) });
        const f = ctx.files;
        try {
          // A small file, with its type and name.
          const small = await f.write("fm_files", "docs//a/./hello.txt", "hello, files", {
            contentType: "text/x-greeting",
            filename: "Hello.txt",
          });
          await note("small_path", small);
          await note("small_read", await f.read("fm_files", small, { encoding: "utf8" }));

          // 20 MB from a stream, and 20 MB as one Uint8Array: both in pieces.
          const size = 20 * MB;
          const streamed = await f.write("fm_files", "docs/big-stream.bin", bigStream(size), {
            contentType: "application/octet-stream",
          });
          await note("stream_size", (await f.stat("fm_files", streamed)).size);
          const whole = new Uint8Array(size);
          for (let i = 0; i < size; i++) whole[i] = byteAt(i);
          const buffered = await f.write("fm_files", "docs/big-buffer.bin", whole);
          await note("buffer_size", (await f.stat("fm_files", buffered)).size);
          await note("stream_read", isBig(await f.read("fm_files", streamed), size));

          // Refused: outside the folder, a sibling with the folder's name as a
          // prefix, another store, climbing out, the root, an op not offered.
          await note("outside", await refusal(() => f.write("fm_files", "other/x.txt", "x")));
          await note("sibling", await refusal(() => f.write("fm_files", "docs-x/x.txt", "x")));
          await note("store", await refusal(() => f.write("fm_other", "docs/x.txt", "x")));
          await note("climb", await refusal(() => f.read("fm_files", "docs/../secret.txt")));
          await note("folder", await refusal(() => f.write("fm_files", "docs", "x")));

          // Removed: gone.
          await note("remove", await f.remove("fm_files", buffered));
          await note("stat_after", JSON.stringify(await f.stat("fm_files", buffered)));
          await note("done", "ok");
        } catch (e) {
          await note("failed", e.stack || e.message);
        }
        return async () => {};
      },
    },
  },
};
