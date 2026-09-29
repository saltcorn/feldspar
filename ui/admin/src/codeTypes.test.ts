/**
 * The declarations the code editor loads are only worth having if they are
 * **true of the sandbox**, so this test does not check that the generator emits
 * particular strings — it hands what it emits to the TypeScript compiler,
 * together with a body written the way `run_js_code`'s own documentation writes
 * one, and asserts that the body type-checks against it.
 *
 * That is the property that matters. A declaration file with a typo in it, a
 * chain method that returns the wrong builder, a row interface a real body
 * cannot use: all of them are compile errors here, and all of them would
 * otherwise be a wrong completion in front of an admin.
 *
 * The counter-test matters as much: a body that misspells a method or reads a
 * column no table has must *fail*, or the declarations would be a rubber stamp.
 */

import ts from "typescript";
import { describe, expect, it } from "vitest";

import {
  codeLibrary,
  columnNames,
  columnType,
  moduleFunctionDeclarations,
  typeName,
  type CodeScope,
  type ModuleFunctionInfo,
  type TableInfo,
} from "./codeTypes";

/** Two tables with a key between them: enough for a row type, a `Ⱶ` join
 * column, and an aggregation over the child. */
const TABLES: TableInfo[] = [
  {
    name: "invoices",
    columns: [
      { name: "id", type: "int", sqlType: "integer", required: true },
      { name: "amount", type: "float", sqlType: "double precision", required: true },
      { name: "paid", type: "bool", sqlType: "boolean", required: true },
      { name: "due", type: "date", sqlType: "date", required: false },
      {
        name: "customer",
        type: "int",
        sqlType: "integer",
        required: false,
        keyTo: "people",
      },
    ],
  },
  {
    name: "people",
    columns: [
      { name: "id", type: "int", sqlType: "integer", required: true },
      { name: "email", type: "text", sqlType: "text", required: true },
      { name: "signed_up", type: "timestamp", sqlType: "timestamptz", required: false },
    ],
  },
];

/** What one compilation is given: the ES library the sandbox really has (no DOM,
 * no Node) and nothing else — in particular none of this project's own `@types`,
 * which are not in the sandbox and would only be found because the test happens
 * to run inside `ui/admin`. */
const OPTIONS: ts.CompilerOptions = {
  allowJs: true,
  checkJs: true,
  noEmit: true,
  strict: true,
  target: ts.ScriptTarget.ES2022,
  lib: ["lib.es2022.d.ts"],
  types: [],
};

/** Two modules' functions, in the shapes v1 allows: a bare synchronous one
 * (`@saltcorn/markdown`) and a declared async one over the module's own state
 * (`@saltcorn/nominatim-geocode`). */
const MODULE_FUNCTIONS: ModuleFunctionInfo[] = [
  {
    module: "@saltcorn/markdown",
    name: "md_to_html",
    description: "Turn markdown into HTML.",
    isAsync: false,
    arguments: [],
  },
  {
    module: "@saltcorn/nominatim-geocode",
    name: "geocode_lat",
    description: "The latitude of an address.",
    isAsync: true,
    arguments: [{ name: "query", type: "Object" }],
  },
];

/** Compile one code body against the generated declarations, and return the
 * errors as their messages.
 *
 * `checkJs` is on here and **off in the editor** (see `CodeEditor.tsx`), and
 * deliberately so: this test wants every disagreement between the declarations
 * and a body reported, while an admin wants completions without a type-checker
 * arguing about a sandbox it cannot see. Same declarations, two readers.
 *
 * The files exist only in memory. Writing them to a temporary directory would
 * pull `node:fs` into a suite whose only other dependency is the code under
 * test, and the compiler takes a host, so there is no reason to.
 */
function sandboxFiles(
  body: string,
  tables: TableInfo[],
  functions: ModuleFunctionInfo[] = [],
  scope: CodeScope = { table: "invoices", event: "insert" },
): Record<string, string> {
  return {
    "sandbox.d.ts": codeLibrary(tables, scope, functions),
    // A code body is the inside of an **async** function: `return` at the top
    // level is what the action runs and `await` at it is legal, so it is wrapped
    // for the compiler exactly as the runtime wraps it.
    "body.js": `async function __body() {\n${body}\n}\n`,
  };
}

function check(
  body: string,
  tables: TableInfo[] = TABLES,
  functions: ModuleFunctionInfo[] = [],
  scope: CodeScope = { table: "invoices", event: "insert" },
): string[] {
  const files = sandboxFiles(body, tables, functions, scope);
  // The installed TypeScript's own library files, by the absolute path it
  // reports for them.
  const defaultLib = ts.getDefaultLibFilePath(OPTIONS);
  const host: ts.CompilerHost = {
    fileExists: (name) => name in files || ts.sys.fileExists(name),
    readFile: (name) => files[name] ?? ts.sys.readFile(name),
    getSourceFile: (name, languageVersion) => {
      const text = files[name] ?? ts.sys.readFile(name);
      return text === undefined
        ? undefined
        : ts.createSourceFile(name, text, languageVersion, true);
    },
    getDefaultLibFileName: () => defaultLib,
    writeFile: () => undefined,
    getCurrentDirectory: () => "",
    getCanonicalFileName: (name) => name,
    useCaseSensitiveFileNames: () => true,
    getNewLine: () => "\n",
  };
  const program = ts.createProgram(Object.keys(files), OPTIONS, host);
  return ts
    .getPreEmitDiagnostics(program)
    .map((d) => ts.flattenDiagnosticMessageText(d.messageText, " "));
}

/** The names a completion list offers immediately after `expression`.
 *
 * `check` above asks whether a finished body is *true*; this asks the question
 * an admin actually asks, which is what appears when they stop typing at a dot.
 * The two are not the same property — declarations can type-check a body written
 * from the reference and still offer nothing useful to someone writing one — and
 * the editor reaches the answer through this same language service, so it can be
 * asserted here rather than only in a browser.
 *
 * `db.` is not valid JavaScript, which is the point: the file is mid-edit, and
 * the service answers over a tree with an error in it exactly as it does in the
 * editor.
 */
function completionsAfter(expression: string, tables: TableInfo[] = TABLES): string[] {
  const files = sandboxFiles(expression, tables);
  const defaultLib = ts.getDefaultLibFilePath(OPTIONS);
  const host: ts.LanguageServiceHost = {
    getScriptFileNames: () => Object.keys(files),
    getScriptVersion: () => "1",
    getScriptSnapshot: (name) => {
      const text = files[name] ?? ts.sys.readFile(name);
      return text === undefined ? undefined : ts.ScriptSnapshot.fromString(text);
    },
    getCurrentDirectory: () => "",
    getCompilationSettings: () => OPTIONS,
    getDefaultLibFileName: () => defaultLib,
    fileExists: (name) => name in files || ts.sys.fileExists(name),
    readFile: (name) => files[name] ?? ts.sys.readFile(name),
  };
  const service = ts.createLanguageService(host);
  const source = files["body.js"];
  const info = service.getCompletionsAtPosition(
    "body.js",
    source.indexOf(expression) + expression.length,
    {},
  );
  return info === undefined ? [] : info.entries.map((entry) => entry.name);
}

describe("what the editor offers at a dot", () => {
  it("offer this server's tables after `db.`", () => {
    const names = completionsAfter("db.");
    expect(names).toContain("invoices");
    expect(names).toContain("people");
    // The two ways in that are not a table: an escape hatch for a name this UI
    // did not know about, and the authority the query runs under.
    expect(names).toContain("table");
    expect(names).toContain("asUser");
    expect(names).toContain("asAdmin");
    // A query's methods are a query's, not the database's.
    expect(names).not.toContain("rows");
  });

  it("offer the trigger table's own columns after `row.`", () => {
    const names = completionsAfter("row.");
    expect(names).toEqual(expect.arrayContaining(["id", "amount", "paid", "due", "customer"]));
    // `row` is the invoices row, so nothing from the table it points at.
    expect(names).not.toContain("email");
  });

  it("offer the chain after a table, and the terminals with it", () => {
    const names = completionsAfter("db.invoices.");
    expect(names).toEqual(
      expect.arrayContaining([
        "where",
        "select",
        "orderBy",
        "limit",
        "offset",
        "asUser",
        "rows",
        "first",
        "get",
        "count",
        "sum",
        "exists",
        "insert",
        "update",
        "delete",
      ]),
    );
  });

  it("keep offering the chain once a query has been narrowed", () => {
    const names = completionsAfter("db.invoices.where({ paid: false }).");
    expect(names).toEqual(expect.arrayContaining(["orderBy", "limit", "rows", "update", "delete"]));
  });

  it("offer a table's column names where a column name is being written", () => {
    // The string-literal union is what makes `orderBy("` useful; without it the
    // admin is typing a column name from memory.
    const names = completionsAfter('db.invoices.orderBy("');
    expect(names).toContain("amount");
    expect(names).toContain("customerⱵemail");
  });
});

describe("the types the code editor loads", () => {
  it("type-check the milestone's own example body", () => {
    // Verbatim from `run_js_code`'s documentation and the TODO's definition of
    // done, which is the body these declarations exist to support.
    const errors = check(`
      const overdue = await db.invoices
        .where({ paid: false, due: { lt: payload.today } })
        .select("id", "amount", "customerⱵemail", { chased: "remindersↃinvoice.length" })
        .orderBy("due")
        .limit(50)
        .rows();

      for (const inv of overdue) {
        await db.people.insert({ email: String(inv["customerⱵemail"]) });
      }
      return {
        chased: overdue.length,
        owed: await db.invoices.where({ paid: false }).sum("amount"),
      };
    `);
    expect(errors).toEqual([]);
  });

  it("cover the rest of the chain, both authorities and every terminal", () => {
    expect(
      check(`
        const one = await db.table("invoices").where("amount > 100").first();
        const mine = await db.asUser().invoices.where({ paid: true }).exists();
        const n = await db.invoices.where({ due: { is_null: true } }).count();
        const top = await db.invoices.orderBy("amount", "desc").limit(1).offset(0).rows();
        const person = await db.people.get(1);
        const written = await db.people.insert([{ email: "a@b.c" }, { email: "d@e.f" }]);
        const updated = await db.invoices.where({ paid: false }).asUser().update({ paid: true });
        const gone = await db.people.where({ email: { ilike: "%@example.com" } }).delete();
        // Two queries that do not depend on each other, issued together.
        const [paid, unpaid] = await Promise.all([
          db.invoices.where({ paid: true }).count(),
          db.invoices.where({ paid: false }).count(),
        ]);
        return [one, mine, n, top, person, written.length, updated.ids, gone.deleted,
                paid + unpaid];
      `),
    ).toEqual([]);
  });

  it("type-check a streamed read, and know what a row of it is", () => {
    // `.iter()` is what a body walking a table larger than one answer writes, so
    // the loop variable has to be a row of *that* table — not `any`, which would
    // complete nothing and catch nothing.
    expect(
      check(`
        let owed = 0;
        for await (const inv of db.invoices.where({ paid: false }).orderBy("due").iter(200)) {
          owed += Number(inv.amount);
          await db.people.insert({ email: String(inv["customerⱵemail"]) });
        }
        for await (const p of db.people.limit(10).iter()) owed += p.id;
        return owed;
      `),
    ).toEqual([]);
    // And the chain in front of it is checked as it always was: a column the
    // table does not have is caught at the `.orderBy()`, which is the one place
    // a streamed read is fussier than an unstreamed one.
    expect(
      check(`for await (const i of db.invoices.orderBy("nope").iter()) i.amount;`).join(" "),
    ).toMatch(/nope/);
  });

  it("type-check the body's own SQL, both spellings of its authority", () => {
    expect(
      check(`
        const ranked = await db.sql(
          "select title, rank() over (order by amount desc) as r from invoices where amount > $1",
          [100],
        );
        const mine = await db.sql("select * from invoices", [], { asUser: true });
        const fluent = await db.asUser().sql("select * from invoices");
        const plain = await db.sql("select count(*) as n from invoices");
        return [ranked.length, mine.length, fluent.length, plain[0].n];
      `),
    ).toEqual([]);

    // An option nobody implements is refused by the prelude at run time, so the
    // editor must not complete it either.
    expect(check(`return await db.sql("select 1", [], { as_user: true });`).join(" ")).toMatch(
      /as_user/,
    );
  });

  it("type-check a body that calls an endpoint and writes what it got", () => {
    // `fetch` is the second thing a body can reach, and the editor has to know
    // the web's shapes for it — the ones an author already knows.
    expect(
      check(`
        const res = await fetch("https://api.example.com/rates", {
          method: "POST",
          headers: new Headers({ authorization: "Bearer " + String(payload.token) }),
          body: { since: "2026-01-01", who: user === null ? null : user.email },
          timeout_ms: 2000,
        });
        if (!res.ok) throw new Error("rates: " + res.status + " " + res.statusText);
        const type = res.headers.get("content-type");
        const rates = await res.json();
        const copy = res.clone();
        const bytes = await copy.bytes();
        let seen = "";
        for (const [name, value] of res.headers) {
          if (name === "x-ratelimit") seen = value;
        }
        await db.people.insert({ email: String(rates.email) });
        return { type, seen, n: bytes.length, url: res.url, again: res.redirected };
      `),
    ).toEqual([]);
    // The counter-test: what the sandbox refuses, the editor refuses. A signal
    // there is nothing to drive, a method it does not send, and a misspelled
    // option are each a mistake worth catching before the trigger fires.
    expect(check(`await fetch("https://x.test/", { signal: null });`).join(" ")).toMatch(/signal/);
    expect(check(`await fetch("https://x.test/", { method: "TRACE" });`).join(" ")).toMatch(
      /TRACE/,
    );
    expect(check(`await fetch("https://x.test/", { header: {} });`).join(" ")).toMatch(/header/);
    // And a forgotten `await`, as with a query.
    expect(check(`return fetch("https://x.test/").status;`).join(" ")).toMatch(/status/);
  });

  it("type-check a body that reads and writes files", () => {
    // `fs` is the third thing a body can reach. The editor has to know that a
    // reference is not a promise, that everything touching bytes is awaited, and
    // what a listing hands back.
    expect(
      check(`
        const theFile = fs("uploads").open("the_file.txt");
        if (await theFile.exists()) {
          const theString = await theFile.text();
          const info = await theFile.stat();
          const size = info === null ? 0 : info.size;
          await fs("uploads").open("reports/" + String(size) + ".json").write({
            lines: theString.split("\\n").length,
          });
        }
        // A directory is walked, and what it hands back is acted on directly.
        for (const entry of await fs("uploads").dir("in").list()) {
          if (entry.isDirectory) continue;
          await entry.copyTo(fs("archive").open("2026/" + entry.name));
          await entry.delete();
        }
        const meta = await theFile.meta();
        await theFile.setMeta({ minRole: 40, attributes: { origin: "trigger" } });
        // Delegation, and the bytes of a download saved as they are.
        const mine = await fs("uploads").asUser().open("mine.txt").text();
        const res = await fetch("https://x.test/logo.png");
        const saved = await fs("uploads").open("logo.png").write(res);
        return { mine, saved, rule: meta.effectiveMinRole, stores: fs.stores.length };
      `),
    ).toEqual([]);
    // The counter-tests: a property the sandbox does not have (the departure
    // from `Blob` the API is deliberate about), a misspelled method, and a
    // forgotten `await`.
    expect(check(`return fs("uploads").open("a.txt").size;`).join(" ")).toMatch(/size/);
    expect(check(`return await fs("uploads").open("a.txt").readText();`).join(" ")).toMatch(
      /readText/,
    );
    expect(check(`return fs("uploads").open("a.txt").text().length;`).join(" ")).toMatch(/length/);
  });

  it("type-check a body that runs another trigger", () => {
    // `trigger` is the fourth thing a body can reach. The editor has to know
    // that the handle is not a promise, that `run()` is what is awaited, and
    // that authority is chosen on the handle.
    expect(
      check(`
        const archived = await trigger("archive_done").run({ before: payload.today });
        const nothing = await trigger("reindex").run();
        const handle = trigger("send_invoice");
        try {
          await handle.asUser().run({ id: row.id });
        } catch (e) {
          await handle.asAdmin().run({ id: row.id, why: String(e) });
        }
        return { archived, nothing, name: handle.name, known: trigger.names.length };
      `),
    ).toEqual([]);
    // The counter-tests: a verb the sandbox does not have, and the forgotten
    // `await` — a handle is not a result.
    expect(check(`await trigger("x").fire();`).join(" ")).toMatch(/fire/);
    expect(check(`return trigger("x").run().then;`).join(" ")).not.toEqual([]);
    expect(check(`return trigger.all;`).join(" ")).toMatch(/all/);
  });

  it("declare the event's own bindings, with the row typed by the trigger's table", () => {
    expect(
      check(`
        const amount = row.amount + (old === null ? 0 : old.amount);
        const who = user === null ? "nobody" : user.email;
        return { amount, who, sent: payload.sent };
      `),
    ).toEqual([]);
  });

  it("declare the module functions, in both spellings, with v1's signatures", () => {
    // A body written the way §4a writes one: the short form for a name only one
    // module supplies, and the exact form naming the module.
    expect(
      check(
        `
        const html = await modfn.md_to_html(String(row.amount));
        const lat = await modfn("@saltcorn/nominatim-geocode").geocode_lat({ city: "Kbh" });
        const also = await modfn("@saltcorn/markdown").md_to_html("# hi");
        return { html, lat, also, supplies: modfn.functions.length };
      `,
        TABLES,
        MODULE_FUNCTIONS,
      ),
    ).toEqual([]);
    // The description and the module reach the signature, which is the whole
    // point of carrying v1's own `arguments` and `description` across.
    const library = moduleFunctionDeclarations(MODULE_FUNCTIONS);
    expect(library).toContain("Turn markdown into HTML");
    expect(library).toContain("@saltcorn/nominatim-geocode");
    expect(library).toContain("query?: unknown");
    // A synchronous v1 function is still awaited here, and the declarations say
    // why rather than leaving an admin to wonder.
    expect(library).toContain("synchronous function in the module");
  });

  it("leave an ambiguous short name out, because calling it throws", () => {
    const both: ModuleFunctionInfo[] = [
      ...MODULE_FUNCTIONS,
      {
        module: "@saltcorn/other-geocode",
        name: "geocode_lat",
        description: "",
        isAsync: true,
        arguments: [],
      },
    ];
    // Both modules keep their own entry…
    const library = moduleFunctionDeclarations(both);
    expect(library).toContain("@saltcorn/other-geocode");
    // …and the short form offers `md_to_html` but not `geocode_lat`, because
    // the prelude throws on the ambiguous one rather than picking a module.
    expect(check(`return await modfn.md_to_html("x");`, TABLES, both)).toEqual([]);
    expect(check(`return await modfn.geocode_lat({});`, TABLES, both).join(" ")).toMatch(
      /geocode_lat/,
    );
  });

  it("type a model handle from `models.get`, its posterior methods as optional members", () => {
    expect(
      check(
        `const m = await models.get("House prices");\n` +
          `const one = await m.predict(row);\n` +
          `const many = await m.predict([row, { area: 90, bedrooms: 2 }]);\n` +
          `const detailed = await m.predict(row, { detail: true });\n` +
          `const old = await models.get("House prices", { fit: m.fit.id });\n` +
          `return { one, n: many.length, p: detailed.probability, table: m.table,\n` +
          `         warned: m.fit.warnings.length, r2: m.fit.metrics, same: old.name === m.name,\n` +
          `         kind: m.outcome && m.outcome.outcome };`,
      ),
    ).toEqual([]);
    // A posterior's four are there to complete, and optional, because which
    // handle a name answers is the fit's to decide at run time.
    expect(
      check(
        `const r = await models.get("Radon");\n` +
          `if (!r.draws || !r.summary || !r.writePosterior || !r.variables) throw new Error("not a posterior");\n` +
          `const d = await r.draws("alpha", { keys: [27001], thin: 10 });\n` +
          `const first = d.chains[0].draws[0][0];\n` +
          `const s = await r.summary("alpha", { elements: { counties: ["Aitkin"] } });\n` +
          `const w = await r.asUser().writePosterior?.({ variable: "alpha", statistics: { mean: "alpha_mean" } });\n` +
          `await r.writePosterior({ variable: "y_future", mode: "insert", table: "forecasts",\n` +
          `  statistics: { mean: "mean" }, coordinates: [{ axis: "day.future", field: "day" }] });\n` +
          `return { first, label: s.rows[0][0], n: r.variables.length, dims: d.dims, w };`,
      ),
    ).toEqual([]);
    expect(
      check(`const r = await models.get("Radon");\nreturn await r.draws("alpha");`).join(" "),
    ).toMatch(/possibly 'undefined'/);
    // The flat functions of the Stan milestone are gone, and a misspelling is
    // an error rather than a completion.
    expect(check(`return await models.draws("Radon", "alpha");`).join(" ")).toMatch(
      /Property 'draws' does not exist/,
    );
    expect(
      check(`const m = await models.get("House prices");\nreturn await m.predicts(row);`).join(
        " ",
      ),
    ).toMatch(/Property 'predicts' does not exist/);
    expect(check(`return await models.get();`).join(" ")).toMatch(/Expected 1-2 arguments/);
  });

  it("declare no modfn on a server whose modules supply no functions", () => {
    const library = codeLibrary(TABLES, { event: "login" });
    expect(library).not.toContain("declare const modfn");
    expect(codeLibrary(TABLES, { event: "login" }, MODULE_FUNCTIONS)).toContain(
      "declare const modfn",
    );
  });

  it("declare no row where the event has none", () => {
    // A `login` trigger's body naming `row` is a ReferenceError in the sandbox,
    // so the editor must not complete it either.
    const library = codeLibrary(TABLES, { event: "login" });
    expect(library).not.toContain("declare const row");
    expect(library).toContain("declare const user");
    expect(library).toContain("declare const payload");
    expect(library).toContain("declare const db");
  });

  it("declare a custom query's request, and no payload, for a query body", () => {
    const library = codeLibrary(TABLES, { request: true });
    expect(library).toContain("declare const body");
    expect(library).toContain("declare const query");
    expect(library).toContain("declare const user");
    expect(library).not.toContain("declare const payload");
    expect(library).not.toContain("declare const row");
  });

  it("refuse what the sandbox would refuse", () => {
    // Each of these is a mistake an admin can make, and each is a case where a
    // completion list that offered it would be lying.
    expect(check(`return await db.invoices.rowz();`).join(" ")).toMatch(/rowz/);
    expect(check(`return await db.invoicez.rows();`).join(" ")).toMatch(/invoicez/);
    expect(check(`return await db.invoices.orderBy("nope").rows();`).join(" ")).toMatch(/nope/);
    expect(
      check(`return await db.invoices.where({ paid: { gtt: 1 } }).rows();`).join(" "),
    ).toMatch(/gtt/);
    // `.update()` and `.delete()` answer a count and ids, not rows.
    expect(
      check(
        `return (await db.invoices.where({ paid: false }).update({ paid: true })).length;`,
      ).join(" "),
    ).toMatch(/length/);
    // And a forgotten `await` is a type error here, which is the cheapest place
    // to learn it: the runtime's own named error is the next cheapest.
    expect(check(`return db.invoices.count() + 1;`).join(" ")).toMatch(/Promise/);
  });

  it("survive a table with no readable fields", () => {
    // `listFields` can fail for one table while the rest load. The table is kept
    // — `db.<name>` exists in the sandbox whatever this UI knows — so what must
    // hold is that the declarations still compile.
    expect(check(`return db.mystery;`, [...TABLES, { name: "mystery", columns: [] }])).toEqual(
      [],
    );
  });
});

describe("the run a workflow step's body can read", () => {
  const STEP: CodeScope = { table: "invoices", event: "insert", run: true };

  it("declare `context` for a workflow step, and only for one", () => {
    // A step is handed the run so far; a trigger's own body is handed nothing,
    // and naming it there is a `ReferenceError` in the sandbox — so completing
    // it would be promising something the run refuses.
    expect(check(`return context.total ?? 0;`, TABLES, [], STEP)).toEqual([]);
    const outside = check(`return context.total ?? 0;`, TABLES, []);
    expect(outside.length).toBeGreaterThan(0);
    expect(outside.join(" ")).toContain("context");
  });

  it("keep the event's own bindings beside it", () => {
    expect(
      check(
        `const n = await db.invoices.where({ customer: row.id }).count();
         return { n, who: user?.email ?? null, from: payload.source, seen: context.seen };`,
        TABLES,
        [],
        STEP,
      ),
    ).toEqual([]);
  });
});

describe("the pieces the declarations are built from", () => {
  it("map a column to the type its values cross as", () => {
    const column = (over: Partial<TableInfo["columns"][number]>) =>
      columnType({ name: "c", type: "text", sqlType: "text", required: true, ...over });
    expect(column({})).toBe("string");
    expect(column({ type: "int", sqlType: "integer" })).toBe("number");
    expect(column({ type: "bool", sqlType: "boolean" })).toBe("boolean");
    // Exact by nature and a JavaScript number is not, so it crosses as a string.
    expect(column({ type: "decimal", sqlType: "numeric" })).toBe("string");
    // A rich type is stored as one of the basic types: the SQL type is what says
    // which, and an unknown one stays honestly unknown.
    expect(column({ type: "Email", sqlType: "text" })).toBe("string");
    expect(column({ type: "Weather", sqlType: "geography" })).toBe("unknown");
    // Nullability is the column's, not the type's.
    expect(column({ required: false })).toBe("string | null");
  });

  it("name the join columns a key field reaches", () => {
    const names = columnNames(TABLES[0], TABLES);
    expect(names).toContain("amount");
    expect(names).toContain("customerⱵemail");
    expect(names).toContain("customerⱵsigned_up");
    // One hop only: the union is a completion list, and every further hop
    // multiplies it by another table's width.
    expect(names.some((n) => n.split("Ⱶ").length > 2)).toBe(false);
  });

  it("turn a table name into an identifier that stays unique", () => {
    expect(typeName("invoices")).toBe("Invoices");
    expect(typeName("order_lines")).toBe("OrderLines");
    // Non-identifier characters become `_` rather than disappearing, so two
    // tables cannot collapse onto one interface name.
    expect(typeName("order lines")).not.toBe(typeName("orderlines"));
    expect(typeName("2024_totals")).toMatch(/^[A-Za-z]/);
  });
});
