// `@saltcorn/data/plugin-helper`, partitioned per export (TODO §2).
//
// v1's plugin-helper is imported by every built-in pattern and by every
// third-party one, and it is not cleanly on one side of the vendoring line: most
// of it renders, and a few exports exist to query v1's database or to feed v1's
// builder. The vendored file is kept whole and unedited; this module is what
// every import of it reaches — the build redirects the vendored patterns' own
// imports here too, so a refused export is refused inside the bundle as well.
//
// Every upstream export is on **exactly one** of the three lists below, and
// `sc-viewpattern`'s `bundle_shape` test walks the vendored module's exports to
// hold that true after a refresh.
import * as upstream from "../vendor/saltcorn-data/plugin-helper.js";

/** Rendering: kept, and answered by v1's own code. Several of these read rows or
 * metadata, but only through `Table`, `View` and `getState()`, which are host
 * shims over this server's own row layer. */
export const KEPT = [
  "add_free_variables_to_joinfields",
  // The builder's `tables` and `views` (TODO "The builder" 5.3): over `View.find`
  // and `Table.find`, which in a Saltcorn UI call list the application's views
  // and the tables of its subset, so it sees what the application sees.
  "build_schema_data",
  "calcfldViewConfig",
  "calcfldViewOptions",
  "calcrelViewOptions",
  "displayType",
  "field_picker_fields",
  "getActionConfigFields",
  "get_child_views",
  "get_inbound_relation_opts",
  "get_inbound_self_relation_opts",
  "get_link_view_opts",
  "get_many_to_many_relation_opts",
  "get_parent_views",
  "initial_config_all_fields",
  "link_view",
  "pathToState",
  "picked_fields_to_query",
  "readState",
  "readStateStrict",
  "run_action_column",
  // Constructors of v1's SQL-expression objects, used by a type's
  // `distance_operators`; pure data, and base-plugin/types.ts builds with them.
  "sqlBinOp",
  "sqlFun",
  "stateFieldsToQuery",
  "stateFieldsToWhere",
  "stateToQueryString",
  "strictParseInt",
] as const;

/** Refused: reachable, and fatal on call with the export's name and the reason. */
export const REFUSED: Record<string, string> = {
  generate_joined_query:
    "it assembles a query for v1's own data layer; a view reads rows through Table.getJoinedRows",
  json_list_to_external_table:
    "it builds a v1 external table in memory; this server's tables come from its table providers",
};

/** Absent: `undefined` to a plugin, because a plugin feature-detects it. Inside
 * the bundle it is a refusal like the others, so a built-in pattern that reaches
 * it gets a sentence rather than "is not a function". */
export const ABSENT: Record<string, string> = {
  // `const results = runCollabEvents ? await runCollabEvents(…) : [];`
  // — @saltcorn/kanban. Real-time collaboration events; there is no socket
  // transport for applications here.
  runCollabEvents: "it emits real-time collaboration events, and applications have no socket transport here",
};

function refusal(name: string, reason: string) {
  const refuse = (): never => {
    throw new Error(
      `@saltcorn/data/plugin-helper's ${name} is not available in this version of Saltcorn: ${reason}.`,
    );
  };
  Object.defineProperty(refuse, "name", { value: name });
  return refuse;
}

/** The vendored module's own export names, for the partition test. */
export const UPSTREAM_EXPORTS: string[] = Object.keys(upstream).sort();

export const add_free_variables_to_joinfields = upstream.add_free_variables_to_joinfields;
export const build_schema_data = upstream.build_schema_data;
export const calcfldViewConfig = upstream.calcfldViewConfig;
export const calcfldViewOptions = upstream.calcfldViewOptions;
export const calcrelViewOptions = upstream.calcrelViewOptions;
export const displayType = upstream.displayType;
export const field_picker_fields = upstream.field_picker_fields;
export const getActionConfigFields = upstream.getActionConfigFields;
export const get_child_views = upstream.get_child_views;
export const get_inbound_relation_opts = upstream.get_inbound_relation_opts;
export const get_inbound_self_relation_opts = upstream.get_inbound_self_relation_opts;
export const get_link_view_opts = upstream.get_link_view_opts;
export const get_many_to_many_relation_opts = upstream.get_many_to_many_relation_opts;
export const get_parent_views = upstream.get_parent_views;
export const initial_config_all_fields = upstream.initial_config_all_fields;
export const link_view = upstream.link_view;
export const pathToState = upstream.pathToState;
export const picked_fields_to_query = upstream.picked_fields_to_query;
export const readState = upstream.readState;
export const readStateStrict = upstream.readStateStrict;
// An action column or segment placed with `minRole` is hidden from a weaker
// viewer when it renders, but v1's action routes (a page's
// `/page/:name/action/:rndid`, every pattern's `run_action`) find it by its
// `rndid` or column index and run it without asking again. Every one of them
// runs it through here, so here is where a viewer below the floor is refused.
export const run_action_column: typeof upstream.run_action_column = async (args) => {
  const floor = args && args.col ? args.col.minRole : undefined;
  const role = (args && args.req && args.req.user && args.req.user.role_id) || 100;
  if (floor && +floor !== 100 && +floor < +role) {
    throw new Error("You do not have permission to run this action");
  }
  return upstream.run_action_column(args);
};
export const sqlBinOp = upstream.sqlBinOp;
export const sqlFun = upstream.sqlFun;
export const stateFieldsToQuery = upstream.stateFieldsToQuery;
export const stateFieldsToWhere = upstream.stateFieldsToWhere;
export const stateToQueryString = upstream.stateToQueryString;
export const strictParseInt = upstream.strictParseInt;

export const generate_joined_query = refusal("generate_joined_query", REFUSED.generate_joined_query);
export const json_list_to_external_table = refusal(
  "json_list_to_external_table",
  REFUSED.json_list_to_external_table,
);

export const runCollabEvents = refusal("runCollabEvents", ABSENT.runCollabEvents);
