// **Bash scripts** — the bundled module with an action that runs any bash
// script on the server, and an agent skill that runs a bash command the model
// writes, here or over SSH (after Saltcorn v1's `@saltcorn/bash`: its
// `run_bash_script` and its "Run Bash command" skill, in
// run-bash-agent-skill.js).
//
// It is an **unsafe** module (`sc_module::unsafe_modules`): `package.json`
// says so under `feldspar.unsafe`, and a server installs and loads it only
// when the operator started it with FELDSPAR_ALLOW_UNSAFE_MODULES=1. The
// worker this file runs on still cannot start a process; `shell.bash` asks the
// server to, and the server runs the script with a cleared environment (the
// base set — PATH, HOME, the locale — plus the variables below), a timeout and
// a process group of its own. None of the server's secrets (`DATABASE_URL`,
// passwords) reach the script.
//
// What the script sees
// --------------------
// - `ROW_<FIELD>`: each field of the triggering row with a value, the name in
//   upper case — `{ age: 35 }` is `ROW_AGE=35`. An object is its JSON.
// - `SC_USER_ID`, `SC_USER_ROLE`: who caused the run, when somebody did.
// - a first line `#!/usr/bin/env python3` runs it under that interpreter
//   instead of bash.
//
// Where the script comes from, and where its output goes
// ------------------------------------------------------
// As v1's: the script is the one typed into the trigger (`script_source`
// "Fixed", the default) or the value of one of the row's fields ("Field",
// `code_field`). The exit code, stdout and stderr are always the answer, and
// are also written back to the triggering row when `exitcode_field`,
// `stdout_field` or `stderr_field` name fields of it — through the call's
// `db`, on the trigger's own authority, so a row the trigger may not update
// is refused rather than written.

"use strict";

/** A variable name the server will accept: letters, digits and `_`. */
const VAR_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** The script's variables: the row's fields and the caller. */
function scriptEnv(row, user) {
  const env = {};
  for (const [key, value] of Object.entries(row || {})) {
    if (value === null || value === undefined) continue;
    const name = `ROW_${key.toUpperCase()}`;
    if (!VAR_NAME.test(name)) continue;
    env[name] = typeof value === "object" ? JSON.stringify(value) : String(value);
  }
  if (user && user.id !== undefined && user.id !== null) {
    env.SC_USER_ID = String(user.id);
    if (user.role_id !== undefined && user.role_id !== null) env.SC_USER_ROLE = String(user.role_id);
  }
  return env;
}

/** The script to run: the trigger's own, or the one in a field of the row. */
function scriptOf(configuration, row) {
  const { script_source, code, code_field } = configuration || {};
  let script;
  if (script_source === "Field") {
    if (!code_field) throw new Error("run_bash_script reads its script from a field, and names none");
    if (!row) throw new Error(`run_bash_script reads its script from the field ${code_field}, and was run with no row`);
    script = row[code_field];
    if (script === null || script === undefined || !String(script).trim())
      throw new Error(`run_bash_script reads its script from the field ${code_field}, which is empty`);
  } else {
    script = code;
    if (!script || !String(script).trim()) throw new Error("run_bash_script has no script to run");
  }
  // A script typed in a browser's textarea has Windows line ends, which bash
  // reads as part of each command.
  return String(script).replace(/\r\n/g, "\n");
}

/** What to write back to the row: the fields the trigger named, or `null`. */
function writeBack(configuration, answer) {
  const { exitcode_field, stdout_field, stderr_field } = configuration || {};
  const values = {};
  if (exitcode_field) values[exitcode_field] = answer.exitcode;
  if (stdout_field) values[stdout_field] = answer.stdout;
  if (stderr_field) values[stderr_field] = answer.stderr;
  return Object.keys(values).length > 0 ? values : null;
}

const run_bash_script = {
  description:
    "Run a bash script on the server, with the row's fields as ROW_* variables. " +
    "Answers { exitcode, stdout, stderr }.",
  configFields: [
    {
      name: "script_source",
      label: "Script source",
      type: "String",
      // Not required: a trigger saved before there was a choice has none,
      // and runs its own script, as "Fixed" does.
      default: "Fixed",
      attributes: { options: ["Fixed", "Field"] },
      sublabel: "Fixed runs the script below; Field runs the one in a field of the triggering row.",
    },
    {
      name: "code",
      label: "Script",
      type: "String",
      input_type: "textarea",
      showIf: { script_source: "Fixed" },
      sublabel:
        "The row is in ROW_<FIELD> variables: if the row is {age: 35} then ROW_AGE=35. " +
        "Also set: SC_USER_ID, SC_USER_ROLE. Start with a #! line for a shell other than bash.",
    },
    {
      name: "code_field",
      label: "Script field",
      type: "String",
      showIf: { script_source: "Field" },
      sublabel: "The field of the row that holds the script.",
    },
    {
      name: "exitcode_field",
      label: "Exit code field",
      type: "String",
      sublabel: "An Integer field of the row to write the exit code to. Blank writes nothing.",
    },
    {
      name: "stdout_field",
      label: "stdout field",
      type: "String",
      sublabel: "A String field of the row to write the script's output to.",
    },
    {
      name: "stderr_field",
      label: "stderr field",
      type: "String",
      sublabel: "A String field of the row to write the script's error output to.",
    },
    {
      name: "cwd",
      label: "Working directory",
      type: "String",
      sublabel: "Where the script runs. Blank is the server user's home directory.",
    },
    {
      name: "timeout_seconds",
      label: "Timeout (seconds)",
      type: "Integer",
      sublabel: "The script is stopped after this long. Blank is 60; at most 3600.",
    },
  ],
  returns: { object: { exitcode: "int", stdout: "string", stderr: "string", timed_out: "bool" } },
  run: async ({ row, table, user, req, configuration, shell, db }) => {
    const { cwd, timeout_seconds } = configuration || {};
    const script = scriptOf(configuration, row);
    // Checked before the script runs: a script whose output has nowhere to go
    // has already had its effects by the time that is found out.
    const writes = writeBack(configuration, { exitcode: 0, stdout: "", stderr: "" });
    if (writes && !(table && table.name && row && row.id !== undefined && row.id !== null))
      throw new Error(
        `run_bash_script writes to ${Object.keys(writes).join(", ")}, which needs a triggering row with an id`,
      );
    if (!shell)
      throw new Error(
        "run_bash_script was called where the server cannot run scripts: it runs only as an " +
          "action of this unsafe module, on a server started with FELDSPAR_ALLOW_UNSAFE_MODULES=1",
      );
    const timeout = Number(timeout_seconds);
    const answer = await shell.bash(script, {
      cwd: cwd && String(cwd).trim() ? String(cwd).trim() : null,
      env: scriptEnv(row, user || (req && req.user)),
      timeout_seconds: Number.isInteger(timeout) && timeout > 0 ? timeout : null,
    });
    const values = writeBack(configuration, answer);
    if (values) await db.table(table.name).where({ id: row.id }).update(values);
    return answer;
  },
};

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "bash",
  actions: { run_bash_script },
  // v1's spelling: the agents plugin collects `exchange.agent_skills`, and
  // this server makes each an agent trait (`run_bash`).
  exchange: { agent_skills: [require("./run-bash-agent-skill.js")] },
  // For the tests.
  _scriptEnv: scriptEnv,
  _scriptOf: scriptOf,
  _writeBack: writeBack,
};
