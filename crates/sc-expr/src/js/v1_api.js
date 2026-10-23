// The **Saltcorn 1 `Table` and `Field`**, in JavaScript, over the plan seam.
//
// One source, two hosts: this file is compiled into the code isolates beside
// the `db` prelude and concatenated into `sc-module`'s host script, so the
// `Table` a `run_js_code` body gets and the `Table` an installed v1 plugin gets
// are the same text. Two implementations that agreed today would be two
// implementations of v1's `Where` translation, and they would disagree by the
// third bug fixed in one of them.
//
// It is JavaScript for the reason the `db` prelude is: what crosses into Rust
// is a **plan**, so v1's whole `Where` vocabulary, its `selopts` and its
// `joinFields` are lowered *here*, against a seam that resolves every name it is
// handed and trusts none of them. No SQL is assembled anywhere in this file —
// `getJoinedQuery` answers a statement, and even that one is rendered by the
// host out of the plan this file sent it.
//
// # The division
//
// v1's `Table.findOne` is synchronous and eight years of plugins are written
// that way, so **metadata is local and synchronous, data is a host call and
// asynchronous** — which is v1's own division, and is why the port is possible
// at all. The local half is the *schema snapshot*: every table and field this
// catalog has, built by `sc_api::code_host::schema` and handed to the isolate
// before the run starts. Nothing in this file makes a host call to answer a
// property.
//
// # What is here and what throws
//
// Everything this version does not implement is reachable as a property and
// **fatal on call**, naming the path — `NOT_IMPLEMENTED` below is the one place
// that list lives, so a method implemented later is deleted from it in the same
// edit that implements it, and the two can never disagree. A method that
// answered `undefined` would not fail; it would compute the wrong answer inside
// somebody's trigger.
(() => {
  "use strict";

  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });

  // -------------------------------------------------------------------------
  // The refusal tier — one list, for the whole of both classes
  // -------------------------------------------------------------------------

  // The reasons, written once each so that the sixteen schema-editing methods
  // say the same sentence rather than sixteen paraphrases of it.
  const SCHEMA =
    "this server introspects the schema it is given, so a table is created and " +
    "altered in the database itself rather than through the API";
  const HISTORY_CAPTURED =
    "row history is recorded by the database itself, for every write to a table " +
    "with history on, so there is no history row to insert by hand";
  const HISTORY_COMPRESSED =
    "row history is merged as it is written (one entry per row per user action) " +
    "and expired by the table's keep_days setting, so there is nothing to compress";
  const SYNC =
    "the mobile offline sync of Saltcorn 1 has no counterpart on this server";
  const STORED =
    "this server recomputes calculated fields itself, on read; there is nothing " +
    "to drive by hand";
  const PORTS =
    "import and export are this server's own, through the API and the admin UI";
  const BUILDER =
    "these are Saltcorn 1's view builder talking to itself, and this server " +
    "builds its views another way";
  const VIEWS =
    "an application's views, pages and settings are saved through this " +
    "server's admin UI and API, not by the code that renders them";
  const SNAPSHOT =
    "a view reads its application from a snapshot this server rebuilds itself " +
    "whenever a view, page or application is saved";
  const AUTHORITY =
    "this server decides who may see a view or page from its min_role before " +
    "it runs, and every row it reads under the viewer's own authority";
  const TRIGGERS =
    "triggers are this server's own, edited in its admin UI; a view finds the " +
    "ones its application declares, by name";
  const TABLE_EVENTS =
    "table events fire this server's triggers from its own write path, not " +
    "from the code that renders a view";
  const FILES =
    "files are not reachable from a view in this version: an application's " +
    "file stores are served by the application itself";
  const USERS =
    "users are this server's own accounts, and a view neither reads nor " +
    "writes them";
  const CRASHES =
    "a view's failures go to this server's log, which is read there";
  const PLUGINS =
    "a module is installed through this server's Modules screen, not " +
    "registered by the code that renders a view";
  const REALTIME =
    "this server has no socket transport for applications, so there are no " +
    "rooms and no dynamic updates to emit";
  // Every v1 `Table` and `Field` member this version does not implement, and
  // why. `Table.`/`Field.` is a static, `table.`/`field.` an instance method —
  // the two spellings v1 itself uses.
  //
  // A `null` reason is a method that is simply not built yet: it says it is not
  // available and does not pretend to have a principle behind it. There are
  // none today — every member left on this list is here for a reason that is
  // written down beside it.
  const NOT_IMPLEMENTED = {
    // Schema editing (§9): a v1 plugin that edits the schema is a plugin
    // editing a schema this server introspects, which is a different argument.
    "Table.create": SCHEMA,
    "Table.update": SCHEMA,
    "Table.rename": SCHEMA,
    "Table.delete": SCHEMA,
    "table.update": SCHEMA,
    "table.rename": SCHEMA,
    "table.delete": SCHEMA,
    "table.add_unique_constraint": SCHEMA,
    "table.remove_unique_constraint": SCHEMA,
    "table.enable_fkey_constraint": SCHEMA,
    "table.resetSequence": SCHEMA,
    "table.repairCompositePrimary": SCHEMA,
    "Field.create": SCHEMA,
    "field.update": SCHEMA,
    "field.delete": SCHEMA,
    "field.alter_sql_type": SCHEMA,
    "field.toggle_not_null": SCHEMA,
    "field.enable_fkey_constraint": SCHEMA,
    "field.add_unique_constraint": SCHEMA,
    "field.remove_unique_constraint": SCHEMA,

    // Row history: `get_history`, `restore_row_version`, `undo_row_changes` and
    // `redo_row_changes` are answered (TODO-history H6.1); these two are
    // Saltcorn 1's shadow table talking to itself.
    "table.insert_history_row": HISTORY_CAPTURED,
    "table.compress_history": HISTORY_COMPRESSED,

    // Offline sync.
    "table.latestSyncInfo": SYNC,
    "table.latestSyncInfos": SYNC,

    // Stored calculated fields: v1's recalculation entry points.
    "table.update_stored_calculateds": STORED,
    "table.recalculate_for_stored": STORED,

    // Import and export.
    "Table.create_from_csv": PORTS,
    "table.import_csv_file": PORTS,
    "table.import_json_file": PORTS,
    "table.dump_to_json": PORTS,

    // The view builder's own helpers. `get_join_field_options`,
    // `get_relation_options` and `get_relation_data` are answered (TODO "The
    // builder" 5.2), because the builder's options are computed from them.
    "table.field_options": BUILDER,
    "table.slug_options": BUILDER,
    "table.getTags": BUILDER,
    "table.getFormulaExamples": BUILDER,
    "field.fill_fkey_options": BUILDER,
    "field.generate": BUILDER,
    "field.validate": BUILDER,

    // Saltcorn UI (TODO "Saltcorn UI" §5, 4.7): the members of v1's other
    // models that the view runtime reaches and this server does not implement.
    // Installed by `sc-module`'s host script onto its `View`, `Page`,
    // `Trigger`, `File`, `User`, `Crash` and `getState()`; a code body has none
    // of those, so to one these lines are only names.
    "View.create": VIEWS,
    "View.update": VIEWS,
    "View.delete": VIEWS,
    "view.delete": VIEWS,
    "view.clone": VIEWS,
    "View.state_refresh": SNAPSHOT,
    "view.get_config_flow": BUILDER,
    "view.connected_objects": BUILDER,
    "view.inbound_connected_objects": BUILDER,
    "view.getTags": BUILDER,
    "view.authorize": AUTHORITY,
    "view.authorise_get": AUTHORITY,
    "view.authorise_post": AUTHORITY,
    "view.openDataStream": FILES,
    "view.runServerSide": SYNC,
    "Page.create": VIEWS,
    "Page.update": VIEWS,
    "page.delete": VIEWS,
    "page.clone": VIEWS,
    "Page.state_refresh": SNAPSHOT,
    "page.is_root_page_for_roles": BUILDER,
    "page.connected_objects": BUILDER,
    "page.getTags": BUILDER,
    "page.authorize": AUTHORITY,
    "Trigger.create": TRIGGERS,
    "Trigger.update": TRIGGERS,
    "trigger.delete": TRIGGERS,
    "Trigger.findDB": TRIGGERS,
    "Trigger.findAllWithTableName": TRIGGERS,
    "Trigger.getAllTableTriggers": TABLE_EVENTS,
    "Trigger.runTableTriggers": TABLE_EVENTS,
    "Trigger.emitEvent": TABLE_EVENTS,
    "File.find": FILES,
    "File.findOne": FILES,
    "File.create": FILES,
    "File.from_contents": FILES,
    "File.from_req_files": FILES,
    "File.upload": FILES,
    "File.allDirectories": FILES,
    "File.ensure_file_store": FILES,
    "User.find": USERS,
    "User.findOne": USERS,
    "User.create": USERS,
    "User.authenticate": USERS,
    "User.hashPassword": USERS,
    "Crash.find": CRASHES,
    "Crash.findOne": CRASHES,
    "state.setConfig": VIEWS,
    "state.deleteConfig": VIEWS,
    "state.refresh": SNAPSHOT,
    "state.refresh_views": SNAPSHOT,
    "state.refresh_pages": SNAPSHOT,
    "state.refresh_triggers": SNAPSHOT,
    "state.registerPlugin": PLUGINS,
    "state.authorizeView": AUTHORITY,
    "state.emitRoom": REALTIME,
    "state.emitDynamicUpdate": REALTIME,
  };

  // What a refused member says. The same sentence in a code body and in a
  // module, because it is the same method that is missing.
  const notAvailable = (path) =>
    "the Saltcorn v1 API " + path + " is not available in this version of Saltcorn" +
    (NOT_IMPLEMENTED[path] ? ": " + NOT_IMPLEMENTED[path] : "");

  // A member that is reachable and fatal on call, naming the path used. Its
  // `name` is the path so that a stack, a log line and a `String(fn)` all say
  // which method the plugin reached for.
  const refusal = (path) => {
    const f = function () {
      throw new Error(notAvailable(path));
    };
    Object.defineProperty(f, "name", { value: path });
    return f;
  };

  // Install every refusal whose path starts with `prefix` onto `target`.
  //
  // A name that is **already there** is a hard error at build time rather than
  // a silent overwrite: that is what stops the list and the implementation from
  // disagreeing, and it fires the moment anybody implements a method without
  // deleting its line above.
  const installRefusals = (target, prefix) => {
    for (const path of Object.keys(NOT_IMPLEMENTED)) {
      if (path.indexOf(prefix) !== 0) continue;
      const name = path.slice(prefix.length);
      if (name.indexOf(".") >= 0) continue;
      if (name in target) {
        throw new Error(
          path + " is both implemented and on the v1 refusal list; delete its line"
        );
      }
      Object.defineProperty(target, name, {
        value: refusal(path), writable: false, configurable: false, enumerable: false,
      });
    }
    return target;
  };

  // Reachable so a test can walk every refusal and assert each names itself,
  // and so an admin screen can list what a v1 plugin will not find here. Inert:
  // it is a list of strings.
  fixed("__scV1Refused", () => Object.keys(NOT_IMPLEMENTED).slice());

  // The installer itself, for the v1 models `sc-module`'s host script builds
  // (`View`, `Page`, `getState()` …): the same list, and the same build-time
  // refusal to shadow a member that is implemented.
  fixed("__scV1InstallRefusals", installRefusals);

  // -------------------------------------------------------------------------
  // v1's `Where`, translated (§5)
  // -------------------------------------------------------------------------

  const isObject = (v) =>
    v !== null && typeof v === "object" && !Array.isArray(v);

  const kindOf = (v) => Object.prototype.toString.call(v);

  // The v1 keys and operators this server deliberately does not carry, and what
  // to write instead. Each is a SQL construct the plan seam does not express or
  // a feature this server does not have — and a translator that dropped one on
  // the floor would compute the wrong answer inside somebody's trigger.
  const REFUSED_WHERE = {
    inSelect:
      "a subquery in a where is not part of this server's plan seam; read the " +
      "inner query first and pass its values with { in: [...] }",
    inSelectWithLevels:
      "recursive subqueries are not part of this server's plan seam",
    json:
      "a JSON path condition is not part of this server's plan seam; select the " +
      "value with a formula and compare that",
    slugify: "slug matching is not part of this server's where vocabulary",
    day_only:
      "date truncation is not part of this server's where vocabulary; compare " +
      "against the two ends of the day instead",
    eq:
      "the two-expression form of eq compares two SQL expressions, which this " +
      "server's plan seam does not carry; write the comparison as a formula string",
    sql: "raw SQL in a where is not part of this server's plan seam",
  };

  // The value side of a comparison. Two v1 spellings are refused here rather
  // than passed on, because both reach SQL as text in v1 and as nothing at all
  // here.
  const literal = (where, value) => {
    if (typeof value === "symbol") {
      throw new Error(
        "`" + where + "`: a Symbol is Saltcorn 1's raw-SQL escape, which this " +
        "server does not run; write the condition as a formula string instead"
      );
    }
    if (kindOf(value) === "[object RegExp]") {
      throw new Error(
        "`" + where + "`: a regular expression is Saltcorn 1's ~ operator, which " +
        "this server does not carry; { ilike: \"...\" } is the nearest thing"
      );
    }
    if (typeof value === "function") {
      throw new Error("`" + where + "`: a function is not a value to compare against");
    }
    return value;
  };

  const refuseKey = (key) => {
    if (Object.prototype.hasOwnProperty.call(REFUSED_WHERE, key)) {
      throw new Error(
        "`" + key + "` in a where is not supported: " + REFUSED_WHERE[key]
      );
    }
  };

  // v1's implicit `%…%`: `{ ilike: "tol" }` matches anywhere in the column,
  // which is what every v1 search box means by it. `fullMatch: true` is the
  // caller saying the pattern is already whole.
  const likePattern = (field, value, fullMatch) => {
    if (typeof value !== "string") {
      throw new Error(
        "`" + field + "`: ilike takes the text to look for, e.g. { ilike: \"tol\" }"
      );
    }
    return fullMatch ? value : "%" + value + "%";
  };

  // The conditions one field's value asks for, as a list — a list because v1
  // says two things about one column in one object (`{ gt: 100, lt: 500 }`) and
  // this server's filter object takes one comparison per key.
  const fieldConditions = (field, value) => {
    if (value === null) return [{ is_null: true }];
    if (Array.isArray(value)) {
      // v1: an array of conditions on one field is their conjunction.
      return value.reduce((acc, part) => acc.concat(fieldConditions(field, part)), []);
    }
    // The three v1 values that are not values here reach `literal` **before**
    // the operator branch: a RegExp has no own keys, so an object walk would
    // read v1's `~` as an empty condition and quietly match every row.
    if (typeof value === "symbol" || typeof value === "function" ||
        kindOf(value) === "[object RegExp]") {
      literal(field, value);
    }
    if (!isObject(value) || kindOf(value) === "[object Date]") {
      return [{ eq: literal(field, value) }];
    }
    const keys = Object.keys(value);
    // v1's two modifiers are read off the object rather than translated: they
    // change what a sibling key means and are not conditions of their own.
    const equal = value.equal === true;
    const fullMatch = value.fullMatch === true;
    const out = [];
    for (const key of keys) {
      if (key === "equal" || key === "fullMatch") continue;
      refuseKey(key);
      const operand = value[key];
      switch (key) {
        case "gt":
          out.push(equal ? { gte: literal(field, operand) } : { gt: literal(field, operand) });
          break;
        case "lt":
          out.push(equal ? { lte: literal(field, operand) } : { lt: literal(field, operand) });
          break;
        case "gte":
        case "lte":
        case "ne": {
          const one = {};
          one[key] = literal(field, operand);
          out.push(one);
          break;
        }
        case "in":
          if (!Array.isArray(operand)) {
            throw new Error("`" + field + "`: in takes an array of values");
          }
          out.push({ in: operand.map((v) => literal(field, v)) });
          break;
        case "ilike":
          out.push({ ilike: likePattern(field, operand, fullMatch) });
          break;
        case "like":
          out.push({ like: likePattern(field, operand, fullMatch) });
          break;
        case "not":
          out.push({ not: operand });
          break;
        case "or":
          if (!Array.isArray(operand)) {
            throw new Error("`" + field + "`: or takes an array of conditions");
          }
          out.push({ or: operand });
          break;
        case "and":
          if (!Array.isArray(operand)) {
            throw new Error("`" + field + "`: and takes an array of conditions");
          }
          out.push({ and: operand });
          break;
        default:
          throw new Error(
            "`" + key + "` is not a Saltcorn 1 where operator on `" + field +
            "`; the operators are: gt, lt, gte, lte, ne, in, not, ilike, like, " +
            "or, and, with equal and fullMatch as modifiers"
          );
      }
    }
    // `{}` says nothing about the column in v1 and says nothing here: an empty
    // object that matched every row and an empty object that matched none are
    // both worse than no condition at all.
    return out;
  };

  // v1's `_fts`: a search box's text, looked for across a table's text fields.
  // `stateFieldsToWhere` (plugin-helper.ts) writes it for a view's `_fts` or
  // `_fts_<table>` state, which is what Filter's search bar and List's own
  // search bar set, with the view's fields beside the term.
  //
  // v1 turns it into SQL over the text fields **concatenated**
  // (`@saltcorn/db-common/internal`, `ftsFieldsSqlExpr`): Postgres matches it
  // with `to_tsvector … @@ to_tsquery('term:*')` (stemmed words, prefix-matched),
  // and SQLite with `LIKE '%term%'`. There is no text-search operator in this
  // server's filter vocabulary, and one clause has to mean the same on both
  // databases, so here it is SQLite's reading applied per field: the rows where
  // **any** text field contains the term, case-insensitively. Two differences
  // follow, and both are narrower rather than wrong:
  // - a term that only occurs across two fields ("Moby Melville") matches
  //   nothing, where v1's concatenation could match it;
  // - Postgres's stemming ("books" finding "book") is not done.
  // v1 also searches a key's summary field when the key has `include_fts`, with
  // a subquery; a subquery is not part of this seam (`inSelect` above), so keys
  // are not searched.
  const ftsClause = (value) => {
    if (!isObject(value)) {
      throw new Error("`_fts` takes v1's { searchTerm, fields }, e.g. what stateFieldsToWhere writes");
    }
    const term = value.searchTerm === undefined || value.searchTerm === null ? "" : String(value.searchTerm);
    // v1: an empty term is `LIKE '%%'`, which every row satisfies.
    if (term === "") return null;
    const textFields = (Array.isArray(value.fields) ? value.fields : []).filter((f) => {
      if (!f || typeof f.name !== "string") return false;
      const sqlName = (isObject(f.type) && f.type.sql_name) || f.sql_type;
      // v1's rule: text columns, and a calculated one only if it is stored.
      return sqlName === "text" && (!f.calculated || f.stored);
    });
    // v1 searches `''` when there is no text field, which matches nothing.
    if (textFields.length === 0) return { formula: "false" };
    const pattern = "%" + term + "%";
    const parts = textFields.map((f) => keyed(f.name, { ilike: pattern }));
    return parts.length === 1 ? parts[0] : { or: parts };
  };

  // One field's conditions, as clauses of this server's filter object. The
  // three that are not comparisons — `not`, `or`, `and` — are combinators
  // *about that field*, so they nest the field name back inside themselves.
  const fieldClauses = (field, value) =>
    fieldConditions(field, value).map((cond) => {
      if (Object.prototype.hasOwnProperty.call(cond, "not")) {
        const inner = cond.not;
        // `{ id: { not: { in: [...] } } }` is v1's spelling of NOT IN, and
        // `nin` is what this server's vocabulary calls it — one operator rather
        // than a negated one, which is the statement anybody reading the SQL
        // expects.
        if (isObject(inner) && Object.keys(inner).length === 1 && Array.isArray(inner.in)) {
          return keyed(field, { nin: inner.in.map((v) => literal(field, v)) });
        }
        return { not: onField(field, inner) };
      }
      if (Object.prototype.hasOwnProperty.call(cond, "or")) {
        return { or: cond.or.map((part) => onField(field, part)) };
      }
      if (Object.prototype.hasOwnProperty.call(cond, "and")) {
        return { and: cond.and.map((part) => onField(field, part)) };
      }
      return keyed(field, cond);
    });

  const keyed = (field, cond) => {
    const out = {};
    out[field] = cond;
    return out;
  };

  // Everything one field's value says, as a single clause.
  const onField = (field, value) => conjoin(fieldClauses(field, value));

  // A list of clauses as one: nothing constrains nothing, one is itself — so
  // the common where is flat — and more than one is their conjunction.
  const conjoin = (clauses) => {
    if (clauses.length === 0) return {};
    if (clauses.length === 1) return clauses[0];
    return { and: clauses };
  };

  // v1's where-expression, as this server's filter object.
  //
  // Written as a list of clauses rather than one object with the fields' names
  // as keys because v1 says several things about one column in one place and
  // this server's filter takes one comparison per key: `{ pages: { gt: 100,
  // lt: 500 } }` is two clauses on `pages`, and an object cannot hold both.
  const translateWhere = (where) => {
    if (where === undefined || where === null) return null;
    if (!isObject(where)) {
      throw new Error(
        "a where is an object of conditions, e.g. { author: \"Tolstoy\" }"
      );
    }
    const clauses = [];
    for (const key of Object.keys(where)) {
      const value = where[key];
      // v1 drops an undefined value rather than filtering on it: an option that
      // was not passed is not a condition that nothing satisfies.
      if (value === undefined) continue;
      refuseKey(key);
      if (key === "_fts") {
        const clause = ftsClause(value);
        if (clause !== null) clauses.push(clause);
        continue;
      }
      if (key === "or" || key === "and") {
        if (!Array.isArray(value)) {
          throw new Error("`" + key + "` takes an array of where-expressions");
        }
        const parts = value.map(translateWhere).filter((p) => p !== null);
        const combined = {};
        combined[key] = parts;
        clauses.push(combined);
        continue;
      }
      if (key === "not") {
        const inner = translateWhere(value);
        if (inner !== null) clauses.push({ not: inner });
        continue;
      }
      // v1's "match nothing", which a view writes when its state rules a query
      // out. There is no `false` in the filter vocabulary, so it is said in the
      // other spelling this seam already carries.
      if (key === "_false") {
        if (value) clauses.push({ formula: "false" });
        continue;
      }
      clauses.push.apply(clauses, fieldClauses(key, value));
    }
    if (clauses.length === 0) return null;
    return conjoin(clauses);
  };

  // -------------------------------------------------------------------------
  // v1's `selopts`, lowered (§5)
  // -------------------------------------------------------------------------

  const SELOPTS = [
    "fields", "orderBy", "orderDesc", "limit", "offset", "forUser", "forPublic",
  ];

  const bound = (name, value) => {
    if (typeof value !== "number" || !isFinite(value) || value < 0 || value % 1 !== 0) {
      throw new Error("`" + name + "` is a whole number of rows, e.g. { " + name + ": 10 }");
    }
    return value;
  };

  // One `orderBy` entry: v1's field name, or the `{ field, desc }` object.
  const orderKey = (entry, orderDesc) => {
    if (typeof entry === "string") {
      return { field: entry, dir: orderDesc ? "desc" : "asc" };
    }
    if (isObject(entry) && typeof entry.field === "string") {
      const desc = entry.desc === undefined ? orderDesc : !!entry.desc;
      return { field: entry.field, dir: desc ? "desc" : "asc" };
    }
    if (isObject(entry) && typeof entry.operator === "string") {
      throw new Error(
        "`orderBy: { operator: \"" + entry.operator + "\" }` is not supported: " +
        "this server orders by a field, a join path or a formula"
      );
    }
    throw new Error(
      "`orderBy` is a field name or { field, desc }, e.g. { orderBy: \"title\" }"
    );
  };

  // Whose view of the data this is. v1 says it with a *user*, and this server's
  // plan says it with an authority — `{ user: id }`, the named user through the
  // very same ownership functions `authority: "user"` goes through. It can only
  // narrow: the body already runs as admin and could read everything by leaving
  // the option out.
  const whoFor = (opts) => {
    if (opts.forUser !== undefined && opts.forUser !== null &&
        opts.forPublic !== undefined && opts.forPublic !== false) {
      throw new Error(
        "`forUser` and `forPublic` both say whose view of the data this is; pass one"
      );
    }
    if (opts.forPublic) return "public";
    const user = opts.forUser;
    if (user === undefined || user === null) return null;
    if (typeof user === "number" || typeof user === "string") return { user: user };
    if (isObject(user)) {
      if (user.id === undefined || user.id === null) {
        throw new Error(
          "`forUser` is the user to read as, and this one has no id; " +
          "pass the user row or its id"
        );
      }
      return { user: user.id };
    }
    throw new Error("`forUser` is a user row or a user id");
  };

  // v1's `selopts`, as the fields of a plan. An **unknown key is refused** for
  // the reason a misspelled fetch option is: an option that silently does
  // nothing is the worst way to learn it was spelled wrong.
  const translateSelopts = (opts) => {
    const out = {};
    if (opts === undefined || opts === null) return out;
    if (!isObject(opts)) {
      throw new Error(
        "the second argument is an options object, e.g. { orderBy: \"title\", limit: 10 }"
      );
    }
    for (const key of Object.keys(opts)) {
      if (SELOPTS.indexOf(key) < 0) {
        throw new Error(
          "`" + key + "` is not an option this server implements; the options " +
          "are: " + SELOPTS.join(", ")
        );
      }
    }
    if (opts.fields !== undefined && opts.fields !== null) {
      const fields = Array.isArray(opts.fields) ? opts.fields : [opts.fields];
      for (const f of fields) {
        if (typeof f !== "string") {
          throw new Error("`fields` is a list of field names");
        }
      }
      out.select = fields.slice();
    }
    if (opts.orderBy !== undefined && opts.orderBy !== null) {
      const desc = !!opts.orderDesc;
      const entries = Array.isArray(opts.orderBy) ? opts.orderBy : [opts.orderBy];
      out.order = entries.map((e) => orderKey(e, desc));
    } else if (opts.orderDesc) {
      throw new Error("`orderDesc` says which way to sort, and there is no `orderBy` to sort by");
    }
    if (opts.limit !== undefined && opts.limit !== null) out.limit = bound("limit", opts.limit);
    if (opts.offset !== undefined && opts.offset !== null) out.offset = bound("offset", opts.offset);
    const authority = whoFor(opts);
    if (authority !== null) out.authority = authority;
    return out;
  };

  // Both are pure — they hold no authority and reach nothing — so they are
  // reachable for the same reason `__scSchema` is: it is hygiene, not a
  // boundary, and a test that asserts what v1's vocabulary becomes is worth
  // more than the privacy of a function that only rearranges an object.
  fixed("__scV1Where", translateWhere);
  fixed("__scV1Selopts", translateSelopts);

  // -------------------------------------------------------------------------
  // The reads (§3, §6)
  // -------------------------------------------------------------------------

  // Every read below is **one plan** — the same object a `db.books.rows()`
  // sends — so it goes through the catalog's name resolution, the shared
  // operator vocabulary, §7.3's ownership rule, the row cap and the call
  // budget. No host operation was added for any of them except the one
  // `getJoinedQuery` needs, and no SQL is assembled here or anywhere in this
  // file.

  // The two join characters, which are what makes §6 possible: v1's
  // `joinFields` is a forward path and v1's `aggregations` is an inverse
  // relation, and this server's expression language already says both
  // (docs/AGG_EXPRS.md).
  const JOIN = "Ⱶ";
  const INVERSE = "Ↄ";

  // A name that is about to be **built into a formula string**. Every other
  // name a plan carries is a JSON value the host resolves against the catalog;
  // these are text this file concatenates, and a name with a quote or a dot in
  // it would be a formula that says something else rather than a name the
  // catalog refuses.
  const identifier = (what, value) => {
    if (typeof value !== "string" || !/^[A-Za-z_][A-Za-z_0-9]*$/.test(value)) {
      throw new Error(
        "`" + what + "` is the name of a table or a field, and `" + String(value) +
        "` is not one"
      );
    }
    return value;
  };

  // One v1 read, as a select plan: v1's where, v1's selopts, and whatever the
  // method itself projects.
  const selectPlan = (table, where, opts, extra) => {
    const plan = { op: "select", table: table };
    const filter = translateWhere(where);
    if (filter !== null) plan.where = filter;
    return Object.assign(plan, translateSelopts(opts), extra || {});
  };

  // The only selopt a question **about** rows rather than about a row can
  // honour: an ordering or a bound over a count says nothing, and v1 passes
  // neither. An unknown key is refused for the reason it is in `selopts`.
  const authorityOf = (opts, method) => {
    if (opts === undefined || opts === null) return null;
    if (!isObject(opts)) {
      throw new Error(method + "'s options are an object, e.g. { forUser: user }");
    }
    for (const key of Object.keys(opts)) {
      if (key !== "forUser" && key !== "forPublic") {
        throw new Error(
          "`" + key + "` is not an option of " + method + "; the options are: " +
          "forUser, forPublic"
        );
      }
    }
    return whoFor(opts);
  };

  // The alias `distinctValues` counts under. It is grouped rather than selected
  // because distinct values *are* groups — and the plan seam has no count-free
  // grouping, for the good reason that a group with no aggregate is a question
  // nobody asked. `_fd_` is this server's own prefix, so it cannot collide with
  // a field of the table being grouped.
  const DISTINCT_COUNT = "_fd_n";

  // v1's aggregate names, in the expression language this server aggregates
  // with. Everything v1 can spell that this server cannot compute is refused
  // naming itself — an aggregation quietly computing the wrong number inside
  // somebody's list view is the failure this is here to prevent.
  //
  // The plain half first, because both callers share it: over a table's own
  // rows an aggregate is the plan's own `{ fn, arg }`, and there are five.
  const plainAggregate = (what, spelling) => {
    if (typeof spelling !== "string") {
      throw new Error(
        "`" + what + ".aggregate` is the aggregate to compute, e.g. \"sum\""
      );
    }
    const lower = spelling.trim().toLowerCase();
    if (lower === "count") return { fn: "count", arg: null };
    if (lower === "sum" || lower === "avg" || lower === "min" || lower === "max") {
      return { fn: lower, arg: true };
    }
    return null;
  };

  const aggregateCall = (what, spelling, field) => {
    const over = () => "\"" + identifier(what + ".field", field) + "\"";
    const plain = plainAggregate(what, spelling);
    if (plain !== null) {
      return plain.arg === null ? "length" : plain.fn + "(" + over() + ")";
    }
    const agg = spelling.trim();
    const lower = agg.toLowerCase();
    if (lower === "count distinct" || lower === "countunique") {
      return "distinct(" + over() + ").length";
    }
    // v1's `Latest ts` / `Earliest ts`: the named field of the child row that is
    // latest (or earliest) by another of its fields — which is exactly what
    // `maxBy`/`minBy` are, down to the trailing member access.
    const ordered = /^(latest|earliest)[ \t]+([A-Za-z_][A-Za-z_0-9]*)$/i.exec(agg);
    if (ordered) {
      const pick = ordered[1].toLowerCase() === "latest" ? "maxBy" : "minBy";
      return pick + "(\"" + ordered[2] + "\")." + identifier(what + ".field", field);
    }
    throw new Error(
      "`" + agg + "` is not an aggregate this server computes: they are count, " +
      "count distinct, sum, avg, min, max, and `Latest <field>` / `Earliest <field>`"
    );
  };

  // What v1 can write in an aggregation that this server does not carry. Each
  // is a SQL construct the plan seam does not express, and each names what it
  // is rather than being dropped.
  const REFUSED_AGGREGATION = {
    ontable:
      "an aggregation over a table other than the one the key points from is " +
      "not part of this server's expression language; read that table directly",
    valueFormula:
      "Saltcorn 1's valueFormula is SQL text; write the value as a projection " +
      "formula instead, e.g. { alias: \"x\", formula: \"childrenↃparent.sum(...)\" }",
    subselect: "a subquery aggregation is not part of this server's plan seam",
    through: "a two-hop aggregation is not part of this server's expression language",
    rename_object: "this is Saltcorn 1's view builder talking to itself",
    where:
      "restricting which child rows count is not part of this version's " +
      "aggregations; read the child table and aggregate the rows in your code",
  };

  // What v1 can write in a join field, likewise.
  const REFUSED_JOIN_FIELD = {
    through:
      "a join through a second key is not part of this version; read the " +
      "intermediate row first, or write the two hops as a projection formula",
    ontable: "a join field names a key of this table, and `ontable` names another",
    rename_object: "this is Saltcorn 1's view builder talking to itself",
    lookupFunction: "a lookup function is JavaScript this server would have to run per row",
  };

  const refuseOptions = (what, opts, allowed, refused) => {
    if (!isObject(opts)) {
      throw new Error("`" + what + "` is an object, e.g. { " + allowed.join(", ") + " }");
    }
    for (const key of Object.keys(opts)) {
      if (Object.prototype.hasOwnProperty.call(refused, key)) {
        throw new Error(
          "`" + what + "." + key + "` is not supported: " + refused[key]
        );
      }
      if (allowed.indexOf(key) < 0) {
        throw new Error(
          "`" + key + "` is not part of `" + what + "`; it is written { " +
          allowed.join(", ") + " }"
        );
      }
    }
  };

  // v1's join field, as a Ⱶ-path projection: `{ ref: "home", target: "name" }`
  // over `patients` is the formula `homeⱵname`, which is a projection of the
  // very same select plan and goes through `ownership::join_guard` like every
  // other path.
  const joinProjection = (alias, opts) => {
    refuseOptions("joinFields." + alias, opts, ["ref", "target"], REFUSED_JOIN_FIELD);
    return {
      alias: alias,
      formula:
        identifier("joinFields." + alias + ".ref", opts.ref) + JOIN +
        identifier("joinFields." + alias + ".target", opts.target),
    };
  };

  // v1's aggregation, as an inverse-relation projection: `{ table: "readings",
  // ref: "patient_id", field: "temperature", aggregate: "avg" }` is the formula
  // `readingsↃpatient_id.avg("temperature")` — again a projection of the same
  // plan, so a joined read is one statement.
  const aggregationProjection = (alias, opts, where) => {
    const what = where + "." + alias;
    refuseOptions(
      what, opts, ["table", "ref", "field", "aggregate"], REFUSED_AGGREGATION
    );
    return {
      alias: alias,
      formula:
        identifier(what + ".table", opts.table) + INVERSE +
        identifier(what + ".ref", opts.ref) + "." +
        aggregateCall(what, opts.aggregate, opts.field),
    };
  };

  // What v1's `getJoinedRows` takes. Anything else it can be handed —
  // `starts_with`, `searchTerm`, `nullPositions` — is refused by the same rule
  // an unknown selopt is, and for the same reason.
  const JOIN_OPTS = [
    "where", "joinFields", "aggregations",
    "orderBy", "orderDesc", "limit", "offset", "forUser", "forPublic",
    // v1's `starFields` adds `a.*` to a query that otherwise names the table's
    // columns — for the users table, whose extra columns v1 does not list. A
    // joined read here is always the whole row (below), so it is accepted and
    // there is nothing for it to add.
    "starFields",
  ];
  const JOIN_SELOPTS = ["orderBy", "orderDesc", "limit", "offset", "forUser", "forPublic"];

  // -------------------------------------------------------------------------
  // The writes (§4)
  // -------------------------------------------------------------------------

  // v1 says whose write this is with an **argument** rather than an option:
  // `insertRow(row, user)`, `updateRow(v, id, user)`, `deleteRows(where, user)`.
  // Omitted, v1 means unrestricted — which is the plan's own default authority,
  // the admin's. Given, it is *that* user, and it lowers to the same
  // `{ user: id }` `forUser` does: the named user loaded where users live and
  // checked through the very same ownership functions `asUser()` goes through.
  //
  // It can only ever narrow. The body already runs as admin and could write
  // anything by leaving the argument out, so naming somebody is a body
  // volunteering to be treated as them.
  const asUserArg = (user, method) => {
    if (user === undefined || user === null) return null;
    if (typeof user === "number" || typeof user === "string") return { user: user };
    if (isObject(user)) {
      if (user.id === undefined || user.id === null) {
        throw new Error(
          "`" + method + "`'s user argument is who to write as, and this one has no " +
          "id; pass the user row or its id"
        );
      }
      return { user: user.id };
    }
    throw new Error("`" + method + "`'s user argument is a user row or a user id");
  };

  // v1's fourth argument to `updateRow`, which is `noTrigger` positionally and
  // an options object in the spellings that grew later.
  //
  // **A write on this server is an event** (§4) — that is what a write through
  // the row layer *is*, and it is why `insertRow` here is worth having at all —
  // so a write no trigger sees is not something this server can do. Refused
  // naming it, rather than accepted and ignored: a plugin that passed it did so
  // to stop a cascade, and silently firing one would be the surprise, not the
  // refusal.
  const WRITE_OPTS_REFUSED = {
    noTrigger:
      "a write on this server is an event, and there is no way to make one that " +
      "no trigger sees; the cascade is bounded by the run's own trigger budget",
    resultCollector: "Saltcorn 1's result collector is its own form machinery",
    restore_of_version: "this server keeps no row history to restore from",
    syncTimestamp: "the mobile offline sync of Saltcorn 1 has no counterpart here",
  };

  const refuseWriteOpts = (method, opts) => {
    if (opts === undefined || opts === null || opts === false) return;
    if (opts === true) {
      throw new Error(
        "`" + method + "`'s fourth argument is Saltcorn 1's `noTrigger`: " +
        WRITE_OPTS_REFUSED.noTrigger
      );
    }
    if (!isObject(opts)) {
      throw new Error("`" + method + "`'s fourth argument is an options object");
    }
    // Every key, because there is no option of a v1 write this version
    // implements: the ones v1 has are the four above, and each says what it is.
    const keys = Object.keys(opts);
    if (keys.length > 0) {
      const key = keys[0];
      throw new Error(
        "`" + method + "." + key + "` is not supported: " +
        (WRITE_OPTS_REFUSED[key] || "this server has no counterpart for it")
      );
    }
  };

  // The row a write addresses, as a filter on the primary key. A plan's update
  // and delete both refuse to run without one (§4: a whole table rewritten by an
  // *omitted* argument is not something this seam allows), so the key v1 passes
  // becomes the `where` that names the row.
  const pkWhere = (spec, id, method) => {
    const pk = spec.primary_key || [];
    if (pk.length !== 1) {
      throw new Error(
        "`" + spec.name + "` has a composite primary key (" + pk.join(", ") + "), " +
        "so there is no single id for " + method + " to address a row by; write the " +
        "condition with deleteRows or db." + spec.name + " instead"
      );
    }
    if (id === undefined || id === null) {
      throw new Error("`" + method + "` takes the primary key of the row to write");
    }
    const clause = {};
    clause[pk[0]] = { eq: id };
    return clause;
  };

  // -------------------------------------------------------------------------
  // `Field`: a view of the snapshot, not a record (§7)
  // -------------------------------------------------------------------------

  // v1 code assigns to a field and expects the assignment to matter — that is
  // what `Field.update` is for. Here it would change a copy of a snapshot and
  // nothing else, so it is **refused at the property**: a Proxy rather than
  // `Object.freeze`, because a frozen object swallows the assignment silently
  // in a non-strict body, which is the failure this is preventing.
  const readOnly = (obj, what) => {
    const refuse = (prop) => {
      throw new TypeError(
        what + " is this server's schema as it is, not a record to edit: " +
        "assigning to `" + String(prop) + "` would change a copy and nothing else"
      );
    };
    return new Proxy(obj, {
      set: (_t, prop) => refuse(prop),
      defineProperty: (_t, prop) => refuse(prop),
      deleteProperty: (_t, prop) => refuse(prop),
    });
  };


  // v1's `Field.labelToName` and `Field.nameToLabel`, which plugins use to
  // build a column from a form label and a label from a column.
  const labelToName = (label) =>
    String(label).toLowerCase().replace(/ /g, "_").replace(/[^a-z0-9_]/g, "");
  // v1's own, which does not capitalise: `label.split("_").join(" ")`. A label
  // that differed from v1's would be a form, a builder field list and a page's
  // fixed-state picker each naming a field differently from v1 (TODO "The
  // builder" 5.7 found it: a view's `id` state field is "id" in v1).
  const nameToLabel = (name) => String(name).split("_").join(" ");

  // One field of one table, with v1's property names on it. `send` is the run's
  // sender, for the one member of `Field` with I/O behind it.
  const makeField = (api, table, spec, send) => {
    const field = {
      // Its id **is** its name: this server identifies a field by name (§9),
      // and a plugin keying a map by `f.id` gets a stable key either way.
      id: spec.name,
      name: spec.name,
      label: spec.label,
      // v1's two type properties, both carried because v1 code reads both: an
      // object for a plain field, the string `"Key to authors"` for a key —
      // or, where the host resolves types, what an instantiated v1 `Field` has.
      type: (api.typeOf && api.typeOf(spec)) || spec.type,
      typename: spec.typename,
      required: spec.required,
      is_unique: spec.is_unique,
      primary_key: spec.primary_key,
      calculated: spec.calculated,
      stored: spec.stored,
      expression: spec.expression,
      is_fkey: spec.is_fkey,
      reftable_name: spec.reftable_name,
      reftype: spec.reftype,
      refname: spec.refname,
      attributes: spec.attributes || {},
      fieldview: spec.fieldview,
      sublabel: spec.sublabel,
      // v1's `table_id` is a number and this server's tables are named (§9).
      table_id: spec.table_id,
      sql_name: spec.sql_name,
      sql_type: spec.sql_type,
      type_name: typeof spec.type === "string" ? spec.type : spec.type && spec.type.name,
      // v1's `pretty_type`: what a key is called, or the type's own name.
      pretty_type: spec.reftable_name ? "Key to " + spec.reftable_name : spec.typename,
      // v1's `form_name` differs from `name` only inside a subform, which this
      // server has no counterpart for; the name is the answer either way.
      form_name: spec.name,
    };
    // The field's own table, resolved rather than nested: a copy of every table
    // inside every field of it is a snapshot several times its own size.
    Object.defineProperty(field, "table", {
      get: () => api.Table.findOne(spec.table_id), enumerable: false,
    });
    // The one member of `Field` with I/O behind it: `Table.distinctValues` from
    // the other end, and the same plan. v1's own signature is `(req, where)`,
    // and the request half of it is a v1 web request this server does not have
    // — so this takes the where alone, and says so in the tutorial.
    Object.defineProperty(field, "distinct_values", {
      enumerable: false,
      value: (where) => api.Table.findOne(spec.table_id).distinctValues(spec.name, where),
    });
    // v1's `listKey`: how a list shows the field when no fieldview is named —
    // the type's `listAs`, else its `showAs`, else the value under its name.
    Object.defineProperty(field, "listKey", {
      enumerable: false,
      get: () => {
        const t = field.type;
        if (t && typeof t.listAs === "function") return (r) => t.listAs(r[spec.name]);
        if (t && typeof t.showAs === "function") return (r) => t.showAs(r[spec.name]);
        return spec.name;
      },
    });
    installRefusals(field, "field.");
    const hosted = api.fieldOf && api.fieldOf(field, spec);
    if (hosted) return hosted;
    return readOnly(field, "a field of `" + table + "`");
  };

  // -------------------------------------------------------------------------
  // `Table`: metadata synchronously (§2), rows through the sender (§3)
  // -------------------------------------------------------------------------

  // v1's `table.get_parent_relations` and `get_child_relations`: which tables a
  // table's keys point at, and which tables' keys point at it (TODO "Saltcorn
  // UI" Phase 10). Metadata, and all of it in the snapshot — v1 answers them
  // from its field cache — so they are answered rather than refused: a view
  // pattern's configuration steps list a column's join fields and a table's
  // child lists with them. Ported from v1's `models/table.ts`, including the
  // sentence a key to a table that is not there fails with.
  //
  // **Restricted to the tables the host lets the run see** (TODO "The builder"
  // 5.2): a Saltcorn UI view sees its application's table subset, so a key to a
  // table outside it is not a relation the builder may offer. A key to an
  // invisible table is skipped rather than refused — the table exists, and the
  // application is simply not allowed to join to it. A code isolate passes no
  // predicate and sees every table.
  const isKey = (f) => f.is_fkey && f.type !== "File";
  const isStored = (f) => !f.calculated || f.stored;
  const visible = (api, name) => !api.tableVisible || api.tableVisible(String(name));
  const visibleKey = (api, f) => isKey(f) && visible(api, f.reftable_name);
  const tableNamed = (api, name) => {
    const table = api.Table.findOne(name);
    if (!table) throw new Error("Unable to find table '" + name);
    return table;
  };
  // v1's `get_join_field_options(allow_double, allow_triple)`: the join paths a
  // field picker offers, each key followed one, two or three tables deep.
  const joinFieldOptions = (api, name, allowDouble, allowTriple) => {
    const result = [];
    for (const f of tableNamed(api, name).fields) {
      if (!visibleKey(api, f)) continue;
      const table = tableNamed(api, f.reftable_name);
      const subOne = { name: f.name, table: table.name, subFields: [], fieldPath: f.name };
      for (const pf of table.fields.filter(isStored)) {
        const subTwo = { name: pf.name, subFields: [], fieldPath: f.name + "." + pf.name };
        if (visibleKey(api, pf) && allowDouble) {
          const table1 = tableNamed(api, pf.reftable_name);
          subTwo.table = table1.name;
          for (const gpf of table1.fields.filter(isStored)) {
            const subThree = {
              name: gpf.name,
              subFields: [],
              fieldPath: f.name + "." + pf.name + "." + gpf.name,
            };
            if (allowTriple && visibleKey(api, gpf)) {
              const gpfTable = api.Table.findOne(gpf.reftable_name);
              if (gpfTable) {
                subThree.table = gpfTable.name;
                for (const ggpf of gpfTable.fields.filter(isStored)) {
                  subThree.subFields.push({
                    name: ggpf.name,
                    fieldPath: f.name + "." + pf.name + "." + gpf.name + "." + ggpf.name,
                  });
                }
              }
            }
            subTwo.subFields.push(subThree);
          }
        }
        subOne.subFields.push(subTwo);
      }
      result.push(subOne);
    }
    return result;
  };
  // v1's `get_relation_data(unique)`: the keys of other tables that point at
  // this one, unique ones by default (a one-to-one relation), with their tables.
  const relationData = (api, name, unique) => {
    const result = [];
    for (const field of api.Field.find({ reftable_name: name, is_unique: unique })) {
      const relationTable = visible(api, field.table_id) ? api.Table.findOne(field.table_id) : undefined;
      if (relationTable) result.push({ relationTable: relationTable, relationField: field });
    }
    return result;
  };
  // v1's `get_relation_options()`: those relations as a picker shows them.
  const relationOptions = (api, name) =>
    relationData(api, name, true).map(({ relationTable, relationField }) => ({
      relationPath: relationTable.name + "." + relationField.name,
      relationFields: relationTable.fields.filter((f) => f.type !== "Key").map((f) => f.name),
    }));
  const parentRelations = (api, name, allowDouble, allowTriple) => {
    const parent_relations = [];
    const parent_field_list = [];
    for (const f of tableNamed(api, name).fields) {
      if (!visibleKey(api, f)) continue;
      const table = tableNamed(api, f.reftable_name);
      for (const pf of table.fields.filter(isStored)) {
        parent_field_list.push(f.name + "." + pf.name);
        if (visibleKey(api, pf) && allowDouble) {
          const table1 = tableNamed(api, pf.reftable_name);
          for (const gpf of table1.fields.filter(isStored)) {
            parent_field_list.push(f.name + "." + pf.name + "." + gpf.name);
            if (allowTriple && visibleKey(api, gpf)) {
              const gpfTable = api.Table.findOne(gpf.reftable_name);
              for (const ggpf of gpfTable ? gpfTable.fields.filter(isStored) : []) {
                parent_field_list.push(f.name + "." + pf.name + "." + gpf.name + "." + ggpf.name);
              }
            }
          }
          parent_relations.push({ key_field: pf, through: f, table: table1 });
        }
      }
      parent_relations.push({ key_field: f, table: table });
    }
    // A unique key to this table is a one-to-one relation, read from its end.
    for (const relation of api.Field.find({ reftable_name: name, is_unique: true })) {
      const related = visible(api, relation.table_id) ? api.Table.findOne(relation.table_id) : undefined;
      if (!related) continue;
      for (const relfield of related.fields) {
        parent_field_list.push(related.name + "." + relation.name + "->" + relfield.name);
        parent_relations.push({ key_field: relation, ontable: related });
      }
    }
    return { parent_relations: parent_relations, parent_field_list: parent_field_list };
  };
  const childRelations = (api, name, allowJoinAggregations) => {
    const child_relations = [];
    const child_field_list = [];
    for (const f of api.Field.find({ reftable_name: name })) {
      if (!f.is_fkey || !visible(api, f.table_id)) continue;
      const table = api.Table.findOne(f.table_id);
      if (!table) throw new Error("Unable to find table with id: " + f.table_id);
      child_field_list.push(table.name + "." + f.name);
      child_relations.push({ key_field: f, table: table });
    }
    if (allowJoinAggregations) {
      for (const f of tableNamed(api, name).fields) {
        if (!visibleKey(api, f)) continue;
        const refTable = tableNamed(api, f.reftable_name);
        for (const rel of childRelations(api, refTable.name, false).child_relations) {
          child_field_list.push(f.name + "->" + rel.table.name + "." + rel.key_field.name);
          child_relations.push({ key_field: rel.key_field, table: rel.table, through: f });
        }
      }
    }
    return { child_relations: child_relations, child_field_list: child_field_list };
  };

  const makeTable = (api, spec, send, runTrigger) => {
    const fields = spec.fields.map((f) => makeField(api, spec.name, f, send));
    const byName = new Map(fields.map((f) => [f.name, f]));
    const pk = spec.primary_key || [];
    // The sender, or a sentence saying why there is none. A run that was given
    // no database host still has the whole schema — that is what the snapshot
    // is — so its `Table` answers every property and refuses every read, which
    // is the honest division rather than a read that answers no rows.
    const ask = (method) => {
      if (typeof send !== "function") {
        throw new Error(
          "`" + spec.name + "." + method + "` reads the database, and nothing here " +
          "can: this run was given no database host, only the schema"
        );
      }
      return send;
    };
    // One plan, sent. Written as a function of the method's name so that the
    // refusal above names the method the plugin actually called.
    const ships = (method, plan) => ask(method)(plan);
    // v1's `getJoinedRows` and its two relatives, which differ only in what
    // they do with the answer. Every option is lowered here (§6): `joinFields`
    // to Ⱶ-paths, `aggregations` to Ↄ-relations, and both as `Selection`s of an
    // ordinary select plan — so a joined read is **one** statement and needs no
    // host operation of its own.
    const joinedPlan = (opts, method) => {
      const given = opts === undefined || opts === null ? {} : opts;
      if (!isObject(given)) {
        throw new Error(
          method + " takes an options object, e.g. { joinFields: { town: { ref: " +
          "\"home\", target: \"name\" } } }"
        );
      }
      for (const key of Object.keys(given)) {
        if (JOIN_OPTS.indexOf(key) < 0) {
          throw new Error(
            "`" + key + "` is not an option of " + method + " on this server; the " +
            "options are: " + JOIN_OPTS.join(", ")
          );
        }
      }
      // The row itself is the table's own fields: a plan's empty `select` is
      // the whole row, and this read has projections to add to it, so what the
      // whole row *is* has to be said.
      const select = spec.fields.map((f) => f.name);
      if (given.joinFields !== undefined && given.joinFields !== null) {
        const joins = given.joinFields;
        if (!isObject(joins)) {
          throw new Error("`joinFields` is an object of { ref, target } by alias");
        }
        for (const alias of Object.keys(joins)) {
          select.push(joinProjection(alias, joins[alias]));
        }
      }
      if (given.aggregations !== undefined && given.aggregations !== null) {
        const aggs = given.aggregations;
        if (!isObject(aggs)) {
          throw new Error(
            "`aggregations` is an object of { table, ref, field, aggregate } by alias"
          );
        }
        for (const alias of Object.keys(aggs)) {
          select.push(aggregationProjection(alias, aggs[alias], "aggregations"));
        }
      }
      const selopts = {};
      for (const key of JOIN_SELOPTS) {
        if (given[key] !== undefined) selopts[key] = given[key];
      }
      return selectPlan(spec.name, given.where, selopts, { select: select });
    };
    const table = {
      // A table's id is its name too, and for the same reason a field's is.
      id: spec.name,
      name: spec.name,
      label: spec.label,
      description: spec.description,
      min_role_read: spec.min_role_read,
      min_role_write: spec.min_role_write,
      // The ownership *formula's source*, because what a v1 plugin does with it
      // is show it or log it, and v1's field where — and only where — the
      // formula says exactly what such a field says.
      ownership_formula: spec.ownership_formula,
      ownership_field_id: spec.ownership_field_id,
      fields: Object.freeze(fields),
      // v1's `pk_name` is the first key; the whole of a composite one is beside
      // it, because this server allows a composite primary key and v1 did not.
      pk_name: pk.length ? pk[0] : undefined,
      // v1's getter answers `null` unless the key has two or more columns, and
      // v1's patterns test it for truth: `edit.ts` builds an object id — and
      // takes the upsert path — whenever it is truthy, so a one-column key's
      // `["id"]` turned every insert through an Edit view into an update of
      // `{}`.
      composite_pk_names: pk.length > 1 ? Object.freeze(pk.slice()) : null,
      // v1's `constraints`: the unique keys, indexes and row constraints, as
      // v1 shapes them (`{ type, configuration }`).
      constraints: Object.freeze((spec.constraints || []).slice()),
      // v1's `delete_url(row, moreQuery)`: where a Delete link posts. A URL, not
      // a deletion — the route behind it is the application's.
      delete_url: (row, moreQuery) =>
        pk.length > 1
          ? "/delete/" + spec.name + "?" +
            pk.map((k) => encodeURIComponent(k) + "=" + encodeURIComponent(row[k])).join("&") +
            (moreQuery ? "&" + moreQuery : "")
          : "/delete/" + spec.name + "/" + encodeURIComponent(row[pk[0]]) +
            (moreQuery ? "?" + moreQuery : ""),
      // What the table is called in SQL. No tenant schema qualifies it here,
      // and nothing on this server will run SQL a plugin builds from it.
      sql_name: '"' + spec.name + '"',
      // Awaited in v1 (`await table.getFields()`), and awaiting an array is an
      // array — so the v1 spelling and this one are the same line.
      getFields: () => Object.freeze(fields),
      getForeignKeys: () => fields.filter((f) => f.is_fkey),
      // Awaited in v1; answered from the snapshot (see `parentRelations`).
      get_parent_relations: (allowDouble, allowTriple) =>
        Promise.resolve(parentRelations(api, spec.name, allowDouble, allowTriple)),
      get_child_relations: (allowJoinAggregations) =>
        Promise.resolve(childRelations(api, spec.name, allowJoinAggregations)),
      // The builder's join and relation pickers (see `joinFieldOptions`).
      get_join_field_options: (allowDouble, allowTriple) =>
        Promise.resolve(joinFieldOptions(api, spec.name, allowDouble, allowTriple)),
      get_relation_options: () => Promise.resolve(relationOptions(api, spec.name)),
      get_relation_data: (unique) =>
        Promise.resolve(relationData(api, spec.name, unique === undefined ? true : unique)),
      owner_fieldname: () => spec.ownership_field_id || undefined,
      // v1's getter, `sqlsanitize(this.name)` (`@saltcorn/db-common/internal`):
      // what Filter's search bar and List's `_fts_search_bar` name a table's
      // search state with, `_fts_<santized_name>` (v1's spelling). Without it
      // the state was `_fts_undefined`, which matched no table.
      santized_name: (() => {
        const s = String(spec.name).replace(/[^\p{Letter}_0-9]*/gu, "");
        return s[0] >= "0" && s[0] <= "9" ? "_" + s : s;
      })(),
      // v1's `is_owner(user, row)` (`models/table.ts`), which v1's patterns ask
      // **synchronously** before drawing what only an owner may use: a List's
      // Delete link for a viewer below `min_role_write` (`viewable_fields.ts`),
      // Show's and Edit's owner-only parts. It decides what is drawn, never what
      // is allowed: the write behind the link is checked by the row layer, under
      // the table's own ownership rule, as every write is.
      //
      // The formula is v1's: a JavaScript expression over the row's columns and
      // `user`, evaluated here as v1's `get_expression_function` evaluates it.
      // One this worker cannot evaluate — this server's own `Ⱶ` join, say — is
      // not ownership as far as drawing goes, so it answers false, and the
      // viewer sees what a non-owner sees.
      is_owner: (user, row) => {
        if (!user) return false;
        if (spec.ownership_formula) {
          try {
            const names = fields.map((f) => f.name).filter((n) => /^[A-Za-z_$][\w$]*$/.test(n));
            // eslint-disable-next-line no-new-func
            const test = new Function("{" + names.join(",") + "}", "user", "return (" + spec.ownership_formula + ");");
            return !!test(row || {}, user);
          } catch (_error) {
            return false;
          }
        }
        const owner = spec.ownership_field_id;
        // v1: a user owns their own row in `users` when nothing else is said.
        if (spec.name === "users" && !owner) {
          return !!user.id && String(row && row.id) === String(user.id);
        }
        return (
          typeof owner === "string" && !!row &&
          (row[owner] === user.id || (isObject(row[owner]) && row[owner].id === user.id))
        );
      },
      to_json: () => ({
        id: spec.name,
        name: spec.name,
        label: spec.label,
        description: spec.description,
        min_role_read: spec.min_role_read,
        min_role_write: spec.min_role_write,
        ownership_formula: spec.ownership_formula,
        ownership_field_id: spec.ownership_field_id,
        primary_key: pk.slice(),
        fields: spec.fields.map((f) => Object.assign({}, f)),
      }),

      // ---------------------------------------------------------------------
      // The reads (§3)
      // ---------------------------------------------------------------------

      // v1's `getRows(where, selopts)`: one select plan, and the rows as this
      // server's wire shape — which is what `db.books.rows()` answers, because
      // it is the same plan.
      getRows: (where, opts) =>
        ships("getRows", selectPlan(spec.name, where, opts)),
      // v1's `getRow`: the first row the same read finds, or null. The bound is
      // this method's own and overrides a `limit` in the options, as v1's does
      // — one row is what the caller asked for by calling this rather than the
      // other one.
      getRow: (where, opts) =>
        ships("getRow", selectPlan(spec.name, where, opts, { limit: 1 }))
          .then((rows) => (rows.length ? rows[0] : null)),
      // v1's `countRows`: an aggregate plan, so the counting is the database's
      // and the row cap never enters into it.
      countRows: (where, opts) => {
        const plan = {
          op: "aggregate", table: spec.name,
          aggregate: [{ alias: "count", fn: "count", arg: null }],
        };
        const filter = translateWhere(where);
        if (filter !== null) plan.where = filter;
        const authority = authorityOf(opts, "countRows");
        if (authority !== null) plan.authority = authority;
        return ships("countRows", plan).then((r) =>
          r === null || r === undefined || r.count === null ? 0 : r.count
        );
      },
      // v1's `distinctValues`: a grouped select, answering v1's plain array of
      // values rather than rows. It groups rather than selecting distinct
      // because distinct values *are* groups; the count it groups with is not
      // answered, and is there because a group with no aggregate is a question
      // this seam (rightly) does not ask.
      distinctValues: (field, where) => {
        const name = identifier("distinctValues", field);
        const plan = {
          op: "aggregate", table: spec.name, group: [name],
          aggregate: [{ alias: DISTINCT_COUNT, fn: "count", arg: null }],
          order: [{ field: name, dir: "asc" }],
        };
        const filter = translateWhere(where);
        if (filter !== null) plan.where = filter;
        return ships("distinctValues", plan).then((groups) =>
          groups.map((g) => g[name])
        );
      },
      // v1's `aggregationQuery(aggregations, { where, groupBy })`: the plan's
      // own `aggregate`, answering **one object** ungrouped and an array of
      // them grouped — which is v1's own convention and this server's, because
      // a grouped answer has no one value to be.
      aggregationQuery: (aggregations, opts) => {
        if (!isObject(aggregations)) {
          throw new Error(
            "aggregationQuery takes the values to compute, e.g. " +
            "{ n: { aggregate: \"count\" }, longest: { field: \"pages\", aggregate: \"max\" } }"
          );
        }
        const plan = { op: "aggregate", table: spec.name, aggregate: [] };
        for (const alias of Object.keys(aggregations)) {
          const one = aggregations[alias];
          refuseOptions(
            "aggregationQuery." + alias, one, ["field", "aggregate"],
            Object.assign({
              table:
                "aggregationQuery aggregates this table's own rows; the child " +
                "table's are `getJoinedRows`'s aggregations",
              ref:
                "aggregationQuery aggregates this table's own rows; the key back " +
                "to it is `getJoinedRows`'s aggregations",
            }, REFUSED_AGGREGATION)
          );
          // The same five aggregates a relation's are, without the relation:
          // `Latest x` and `count distinct` are shapes of a *subquery* over
          // child rows, and this aggregates the table's own.
          const plain = plainAggregate("aggregationQuery." + alias, one.aggregate);
          if (plain === null) {
            throw new Error(
              "`" + one.aggregate + "` is not an aggregate over this table's own " +
              "rows: they are count, sum, avg, min, max"
            );
          }
          plan.aggregate.push({
            alias: alias,
            fn: plain.fn,
            arg: plain.arg === null
              ? null
              : identifier("aggregationQuery." + alias + ".field", one.field),
          });
        }
        if (opts !== undefined && opts !== null) {
          if (!isObject(opts)) {
            throw new Error(
              "aggregationQuery's second argument is an options object, e.g. " +
              "{ where: { pages: { gt: 100 } }, groupBy: \"author\" }"
            );
          }
          for (const key of Object.keys(opts)) {
            if (["where", "groupBy", "forUser", "forPublic"].indexOf(key) < 0) {
              throw new Error(
                "`" + key + "` is not an option of aggregationQuery; the options " +
                "are: where, groupBy, forUser, forPublic"
              );
            }
          }
          const filter = translateWhere(opts.where);
          if (filter !== null) plan.where = filter;
          if (opts.groupBy !== undefined && opts.groupBy !== null) {
            const by = Array.isArray(opts.groupBy) ? opts.groupBy : [opts.groupBy];
            plan.group = by.map((g) => identifier("groupBy", g));
          }
          const authority = whoFor(opts);
          if (authority !== null) plan.authority = authority;
        }
        return ships("aggregationQuery", plan);
      },
      // v1's `getJoinedRows` (§6), and the row-at-a-time spelling of it.
      getJoinedRows: (opts) =>
        ships("getJoinedRows", joinedPlan(opts, "getJoinedRows")),
      getJoinedRow: (opts) => {
        const plan = joinedPlan(opts, "getJoinedRow");
        plan.limit = 1;
        return ships("getJoinedRow", plan).then((rows) => (rows.length ? rows[0] : null));
      },
      // v1's `getJoinedQuery`: the statement the same plan renders, as
      // `{ sql, values }` — v1's own shape — or `{ notAuthorized: true }` where
      // the ownership rule says this reader may not have it.
      //
      // **This server will not run it for you.** There is no v1 `db` module in
      // this version, so what this answers is for the plugin that inspects or
      // logs the query, and nothing here will take it back as SQL.
      getJoinedQuery: (opts) => {
        const plan = joinedPlan(opts, "getJoinedQuery");
        plan.render = true;
        return ships("getJoinedQuery", plan);
      },

      // ---------------------------------------------------------------------
      // The writes (§4)
      // ---------------------------------------------------------------------

      // v1's `insertRow(row, user)`, answering the **primary key** of the row
      // written — which is what v1 answers and what a plugin then passes to
      // `updateRow` or puts in a link.
      //
      // One insert plan, so the row is coerced against its columns, validated,
      // File-field-checked and **observed by triggers** exactly as a write
      // through the API is. That is the whole reason this method is worth
      // having: a v1 plugin's write is an event here, and the trigger an admin
      // wrote sees it.
      insertRow: (row, user) => {
        if (Array.isArray(row)) {
          throw new Error(
            "`insertRow` writes one row; pass one object, or call it once per row"
          );
        }
        const plan = { op: "insert", table: spec.name, values: row };
        const authority = asUserArg(user, "insertRow");
        if (authority !== null) plan.authority = authority;
        return ships("insertRow", plan).then((written) =>
          written === null || written === undefined ? undefined : written[pk[0]]
        );
      },
      // v1's `tryInsertRow`: the same write, with the failure answered rather
      // than thrown — `{ success: id }` or `{ error: message }`, which is the
      // shape a v1 plugin branches on.
      tryInsertRow: (row, user) =>
        table.insertRow(row, user).then(
          (id) => ({ success: id }),
          (e) => ({ error: e && e.message ? e.message : String(e) })
        ),
      // v1's `updateRow(values, id, user, noTrigger)`, with **v1's return
      // convention**: `undefined` is success and a **string is the error**.
      //
      // That convention is the reason this method does not simply reject. Eight
      // years of plugins are written as `const err = await t.updateRow(v, id,
      // user); if (err) …`, and a rejection there is an unhandled one — the
      // refusal a v1 plugin is written to *read* would become a crash it never
      // sees. What it costs is stated where the tutorial documents it: a body
      // that ignores the answer does not learn the write failed, which is v1's
      // own trade and not one invented here. `db.<table>.update()` is the
      // surface that throws.
      updateRow: (values, id, user, opts) => {
        refuseWriteOpts("updateRow", opts);
        const plan = {
          op: "update", table: spec.name, values: values,
          where: pkWhere(spec, id, "updateRow"),
        };
        const authority = asUserArg(user, "updateRow");
        if (authority !== null) plan.authority = authority;
        return ships("updateRow", plan).then(
          // **Nothing written is a refusal here, not a success.** A delegated
          // write resolves the rows it touches through the delegated *read*, so
          // a row this user may not see is a row that matched nothing — and a
          // withheld row is the same not-found an absent one gets, deliberately
          // (§7.3). v1 answers "not authorized" for the first and this method
          // has one string for both, because telling them apart is exactly what
          // the rule declines to do.
          (r) => (r && r.updated
            ? undefined
            : "no row of `" + spec.name + "` with that id was updated: either there " +
              "is no such row, or this user may not write it"),
          (e) => (e && e.message ? e.message : String(e))
        );
      },
      // v1's `tryUpdateRow`: the same write in the other of v1's two shapes.
      tryUpdateRow: (values, id, user, opts) =>
        table.updateRow(values, id, user, opts).then((error) =>
          typeof error === "string" ? { error: error } : { success: true }
        ),
      // v1's `deleteRows(where, user)`. Each matched row is deleted through the
      // row layer one at a time, so each is its own event — and a delete with
      // **no condition** is refused by the plan seam, because a whole table
      // emptied by an omitted argument is not something an accident should be
      // able to cause.
      deleteRows: (where, user) => {
        const plan = { op: "delete", table: spec.name };
        const filter = translateWhere(where);
        if (filter !== null) plan.where = filter;
        const authority = asUserArg(user, "deleteRows");
        if (authority !== null) plan.authority = authority;
        return ships("deleteRows", plan).then(() => undefined);
      },
      // ---------------------------------------------------------------------
      // Row history (§14.12, TODO-history H6.1)
      // ---------------------------------------------------------------------

      // v1's `get_history(id)`: every version of the row, **oldest first**, each
      // the whole row as it was just after that version, with v1's `_version`,
      // `_time` and `_userid` and this server's `_deleted` (the version that
      // deleted it), `_op`, `_changeset` and `_source`. The table must have
      // history on.
      //
      // v1 answered every row's history when called without an id. Here a row's
      // versions are rebuilt from the change log, one row at a time, so the id
      // is required; what one action did across rows is a changeset, which the
      // admin's changeset browser shows.
      get_history: (id) => {
        if (id === undefined || id === null) {
          throw new Error(
            "`get_history` takes the primary key of the row whose versions to read; " +
            "this server rebuilds a row's versions from its change log one row at a time"
          );
        }
        pkWhere(spec, id, "get_history");
        return ships("get_history", {
          op: "history", what: "versions", table: spec.name, key: id,
        }).then((versions) =>
          versions.map((v) => Object.assign({}, v, { _time: new Date(v._time) }))
        );
      },
      // v1's `restore_row_version(id, version, user)`: the row set back to how it
      // was just after `version`, as one write through the row layer (so write
      // rules and triggers apply), in a `restore` changeset of its own. A row
      // deleted since is inserted again. A row already as it was is left alone,
      // as v1's write of the same values would.
      restore_row_version: (id, version, user) => {
        pkWhere(spec, id, "restore_row_version");
        if (typeof version !== "number" || !Number.isInteger(version) || version < 1) {
          throw new Error(
            "`restore_row_version` takes the version to go back to, a number from " +
            "the row's `get_history`"
          );
        }
        const plan = {
          op: "history", what: "restore_version", table: spec.name, key: id,
          version: version,
        };
        const authority = asUserArg(user, "restore_row_version");
        if (authority !== null) plan.authority = authority;
        return ships("restore_row_version", plan).then(() => undefined);
      },
      // v1's `undo_row_changes(id, user)` and `redo_row_changes(id, user)`: step
      // the row back one version, and forward again, as v1 did with its
      // `_restore_of_version`. Each step is a restore. Nothing to step to is not
      // an error, as it was not in v1.
      undo_row_changes: (id, user) => {
        pkWhere(spec, id, "undo_row_changes");
        const plan = { op: "history", what: "undo_row", table: spec.name, key: id };
        const authority = asUserArg(user, "undo_row_changes");
        if (authority !== null) plan.authority = authority;
        return ships("undo_row_changes", plan).then(() => undefined);
      },
      redo_row_changes: (id, user) => {
        pkWhere(spec, id, "redo_row_changes");
        const plan = { op: "history", what: "redo_row", table: spec.name, key: id };
        const authority = asUserArg(user, "redo_row_changes");
        if (authority !== null) plan.authority = authority;
        return ships("redo_row_changes", plan).then(() => undefined);
      },

      // v1's `toggleBool(id, field, user)`: the boolean at the other value.
      //
      // **Two round trips, not one statement.** v1 writes `SET f = NOT f`, and
      // an assignment on this seam is a value rather than an expression over the
      // row — so this reads the row and writes the negation back. The difference
      // is a race: two toggles of the same row interleaved end where they
      // started, where v1's single statement would have ended toggled. It is
      // written down rather than hidden because a plugin toggling a flag from
      // two events at once is a real shape, and the fix is a `db.sql()`.
      toggleBool: (id, field, user) => {
        const name = identifier("toggleBool", field);
        if (!byName.has(name)) {
          throw new Error("`" + spec.name + "` has no field `" + name + "` to toggle");
        }
        const where = pkWhere(spec, id, "toggleBool");
        const authority = asUserArg(user, "toggleBool");
        const read = { op: "select", table: spec.name, where: where, limit: 1 };
        if (authority !== null) read.authority = authority;
        return ships("toggleBool", read).then((rows) => {
          if (!rows.length) {
            throw new Error(
              "there is no row of `" + spec.name + "` with that id to toggle `" +
              name + "` on"
            );
          }
          const values = {};
          values[name] = !rows[0][name];
          const plan = {
            op: "update", table: spec.name, values: values, where: where,
          };
          if (authority !== null) plan.authority = authority;
          return ships("toggleBool", plan).then(() => undefined);
        });
      },
      // v1's `run_trigger(trigger, row, user)`: the named trigger, run on this
      // row as its payload.
      //
      // Through **the** dispatcher — the same one a table event and a
      // `trigger("name").run()` go through — so `only_if`, the role floor and
      // the cascade bound all still apply, and a v1 plugin cannot start a run
      // that a body in this server's own language could not. The trigger is a
      // name or the object v1 passes, which carries one.
      run_trigger: (trigger, row, user) => {
        const name = isObject(trigger) ? trigger.name : trigger;
        if (typeof name !== "string" || name === "") {
          throw new Error(
            "`run_trigger` takes the trigger to run, by name or as the object " +
            "carrying one"
          );
        }
        if (typeof runTrigger !== "function") {
          throw new Error(
            "`" + spec.name + ".run_trigger` runs another trigger, and nothing here " +
            "can: this run was given no trigger host"
          );
        }
        const request = {
          trigger: name,
          payload: row === undefined || row === null ? {} : row,
        };
        const authority = asUserArg(user, "run_trigger");
        if (authority !== null) request.authority = authority;
        return runTrigger(request);
      },
    };
    // v1's `pk_type` is what a caller branches on, so it is the type's *name*
    // — `"Integer"`, `"String"` — and not the type object `type` already
    // carries.
    Object.defineProperty(table, "pk_type", {
      enumerable: true,
      get: () => {
        const field = byName.get(table.pk_name);
        return field ? field.type_name : undefined;
      },
    });
    // v1's `getField` walks a dotted path — `"author.name"` is the *author's*
    // name field, on the table the key points at — which is the one place v1's
    // metadata is a graph rather than a list.
    Object.defineProperty(table, "getField", {
      enumerable: false,
      value: (path) => {
        if (typeof path !== "string") {
          throw new Error("getField takes a field name, e.g. getField(\"author\")");
        }
        const parts = path.split(".");
        let field = byName.get(parts[0]);
        for (let i = 1; i < parts.length; i++) {
          if (!field || !field.reftable_name) return undefined;
          const target = api.Table.findOne(field.reftable_name);
          if (!target) return undefined;
          field = target.getField(parts[i]);
        }
        return field;
      },
    });
    installRefusals(table, "table.");
    return readOnly(table, "table `" + spec.name + "`");
  };

  // -------------------------------------------------------------------------
  // The factory
  // -------------------------------------------------------------------------

  // One run's `Table` and `Field`, over one run's **sender** and the schema
  // snapshot that run resolved at invoke.
  //
  // `send` is one function — a plan in, a promise of the answer out — and it is
  // very nearly the whole of what this file can reach: in a code body it is the
  // run's own `db` sender, closed over the token that says whose call this is;
  // in a module it is the ask channel back to the server. Passing the sender
  // rather than the token is what lets the *same text* serve both, which is the
  // point of there being one file.
  //
  // `runTrigger` is the second and last of them, for the one v1 `Table` method
  // that is not about rows: `run_trigger`. It is the run's own trigger sender —
  // `trigger.__scRun` in a code body — so a v1 plugin's run is counted against
  // the same trigger budget and carries the same chain a `trigger("x").run()`
  // does. A run given none has a `run_trigger` that says so, exactly as a run
  // given no `send` has reads that say so.
  //
  // A run with **no sender** gets a `Table` whose metadata is whole and whose
  // reads say so by name, because a run with no database host really can answer
  // the one and not the other. A run with **no snapshot** gets classes that say
  // so by name rather than classes that know no tables: a `Table.findOne`
  // answering undefined for everything would have a plugin compute the wrong
  // answer instead of failing.
  fixed("__scMakeV1Api", (send, snapshot, runTrigger, typeOf, fieldOf, tableVisible) => {
    const api = {};
    // Which tables the run may see by listing or relating, when the host
    // restricts it: a Saltcorn UI view sees its application's table subset
    // (TODO "The builder" 5.2). `Table.find`, `Field.find` and the relation
    // helpers answer inside it; `Table.findOne` by name does not, because the
    // save checks already refuse a view naming a table outside the subset, and
    // the run refuses it again. A code isolate passes nothing and sees all.
    Object.defineProperty(api, "tableVisible", {
      value: typeof tableVisible === "function" ? tableVisible : null,
      enumerable: false,
    });
    // What a table's field *is*, when the host has v1's own `Field` class: v1's
    // patterns write to a table's fields (Filter sets a field's `fieldview`
    // before it fills the field's options), which a read-only snapshot object
    // refuses. A host without the class passes nothing, and a field stays the
    // read-only record below.
    Object.defineProperty(api, "fieldOf", {
      value: typeof fieldOf === "function" ? fieldOf : null,
      enumerable: false,
    });
    // How a field's `type` is resolved, when the host has v1's type registry: an
    // instantiated v1 `Field` holds the type *object* (with its fieldviews), or
    // `"Key"`, or `"File"` — not the snapshot's data. A code isolate has no
    // registry and passes nothing, and a field keeps the snapshot's `type`.
    Object.defineProperty(api, "typeOf", {
      value: typeof typeOf === "function" ? typeOf : null,
      enumerable: false,
    });
    const absent = (what) => {
      throw new Error(
        what + " is not available here: this run was given no schema snapshot, " +
        "so there is nothing for it to answer from"
      );
    };
    if (!snapshot || !Array.isArray(snapshot.tables)) {
      api.Table = installRefusals({
        findOne: () => absent("Table.findOne"),
        find: () => absent("Table.find"),
      }, "Table.");
      api.Field = installRefusals({
        find: () => absent("Field.find"),
        findOne: () => absent("Field.findOne"),
        findCached: () => absent("Field.findCached"),
        labelToName: labelToName,
        nameToLabel: nameToLabel,
      }, "Field.");
      return api;
    }

    // `_fd_*` is where this server keeps its own rows, and a `Table.find()`
    // that listed them would put them in front of a plugin that only ever
    // wanted the application's tables.
    const specs = snapshot.tables.filter((t) => !t.is_system);
    const visibleSpec = (spec) => !api.tableVisible || api.tableVisible(spec.name);
    // Built on demand and **kept**, so that `Table.findOne("books")` twice is
    // the same object twice: v1's is a state cache and plugins compare what
    // comes out of it.
    const built = new Map();
    const tableOf = (spec) => {
      if (!built.has(spec.name)) {
        built.set(spec.name, makeTable(api, spec, send, runTrigger));
      }
      return built.get(spec.name);
    };

    // A v1 `where` over the tables' own properties: `Table.findOne("books")`,
    // `Table.findOne({ name: "books" })` and `Table.find({ min_role_read: 1 })`
    // are all this, and none of them is a query.
    const matches = (spec, where) => {
      if (typeof where === "string" || typeof where === "number") {
        return spec.name === String(where);
      }
      if (!isObject(where)) return true;
      for (const key of Object.keys(where)) {
        const wanted = where[key];
        const held = key === "id" ? spec.name : spec[key];
        if (held !== wanted) return false;
      }
      return true;
    };

    api.Table = installRefusals({
      // Synchronous, which is the whole reason the snapshot exists. A table
      // this catalog has not got is `undefined`, as it is in v1 — an absence
      // this server really knows about, and not a stub declining to answer.
      findOne: (where) => {
        if (where === undefined || where === null) return undefined;
        const spec = specs.find((s) => matches(s, where));
        return spec ? tableOf(spec) : undefined;
      },
      find: (where, selopts) => {
        let out = specs.filter((s) => matches(s, where) && visibleSpec(s));
        if (selopts !== undefined && selopts !== null) {
          if (!isObject(selopts)) {
            throw new Error("Table.find's second argument is an options object");
          }
          for (const key of Object.keys(selopts)) {
            // `cached` is v1's "from the state cache"; the snapshot is that
            // cache, so it changes nothing (`build_schema_data` passes it).
            if (key !== "orderBy" && key !== "limit" && key !== "cached") {
              throw new Error(
                "`" + key + "` is not an option of Table.find; the options are: " +
                "orderBy, limit, cached"
              );
            }
          }
          if (selopts.orderBy) {
            const by = selopts.orderBy;
            out = out.slice().sort((a, b) =>
              String(a[by] === undefined ? "" : a[by]) <
              String(b[by] === undefined ? "" : b[by]) ? -1 : 1
            );
          }
          if (selopts.limit !== undefined) out = out.slice(0, bound("limit", selopts.limit));
        }
        return out.map(tableOf);
      },
    }, "Table.");

    // Every field of every table, which is what v1's `Field.find` reads out of
    // v1's `_sc_fields` — a table this server does not have, and a snapshot it
    // does.
    const allFields = () => {
      const out = [];
      for (const spec of specs) {
        if (!visibleSpec(spec)) continue;
        const table = tableOf(spec);
        for (const field of table.fields) out.push(field);
      }
      return out;
    };
    const fieldMatches = (field, where) => {
      if (typeof where === "string") return field.name === where;
      if (!isObject(where)) return true;
      for (const key of Object.keys(where)) {
        if (field[key] !== where[key]) return false;
      }
      return true;
    };
    const findFields = (where) => allFields().filter((f) => fieldMatches(f, where));

    api.Field = installRefusals({
      find: findFields,
      findOne: (where) => findFields(where)[0],
      // v1's cached spelling of the same question. Everything here is cached —
      // the snapshot is the cache — so the two answer alike rather than one of
      // them going to a database this server would not let it reach anyway.
      findCached: findFields,
      labelToName: labelToName,
      nameToLabel: nameToLabel,
    }, "Field.");

    return api;
  });
})();
