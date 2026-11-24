// `node --test plugins/bash/test` — the variables a script is given.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const { _scriptEnv: scriptEnv } = require("../index.js");

test("the row's fields are ROW_ variables and the caller is SC_USER_", () => {
  const env = scriptEnv({ age: 35, name: "Ada", tags: ["a"], gone: null, "bad-name": 1 }, { id: 7, role_id: 40 });
  assert.deepStrictEqual(env, {
    ROW_AGE: "35",
    ROW_NAME: "Ada",
    ROW_TAGS: '["a"]',
    SC_USER_ID: "7",
    SC_USER_ROLE: "40",
  });
});

test("no row and no caller is no variables", () => {
  assert.deepStrictEqual(scriptEnv(null, null), {});
});

const { _scriptOf: scriptOf, _writeBack: writeBack, actions } = require("../index.js");

test("the script is the trigger's own unless it says to read a field", () => {
  assert.strictEqual(scriptOf({ code: "echo a\r\necho b" }, null), "echo a\necho b");
  assert.strictEqual(scriptOf({ script_source: "Fixed", code: "ls" }, { cmd: "pwd" }), "ls");
  assert.strictEqual(scriptOf({ script_source: "Field", code_field: "cmd" }, { cmd: "pwd" }), "pwd");
  assert.throws(() => scriptOf({ script_source: "Field", code_field: "cmd" }, { cmd: "" }), /empty/);
  assert.throws(() => scriptOf({ script_source: "Field" }, { cmd: "pwd" }), /names none/);
  assert.throws(() => scriptOf({}, null), /no script/);
});

test("only the fields the trigger named are written back", () => {
  const answer = { exitcode: 3, stdout: "out", stderr: "err", timed_out: false };
  assert.strictEqual(writeBack({}, answer), null);
  assert.deepStrictEqual(writeBack({ exitcode_field: "code", stdout_field: "log" }, answer), {
    code: 3,
    log: "out",
  });
});

/** A `db` that records the one chain the action writes with. */
function recordingDb() {
  const writes = [];
  return {
    writes,
    table: (name) => ({
      where: (where) => ({ update: async (values) => writes.push({ name, where, values }) }),
    }),
  };
}

test("the action writes its answer back to the triggering row", async () => {
  const db = recordingDb();
  const plans = [];
  const shell = {
    bash: async (script, plan) => {
      plans.push({ script, ...plan });
      return { exitcode: 1, stdout: "hi\n", stderr: "", timed_out: false };
    },
  };
  const answer = await actions.run_bash_script.run({
    row: { id: 9, cmd: "echo hi" },
    table: { name: "jobs" },
    configuration: {
      script_source: "Field",
      code_field: "cmd",
      exitcode_field: "status",
      stdout_field: "output",
    },
    shell,
    db,
  });
  assert.strictEqual(answer.stdout, "hi\n");
  assert.strictEqual(plans[0].script, "echo hi");
  assert.strictEqual(plans[0].env.ROW_CMD, "echo hi");
  assert.deepStrictEqual(db.writes, [
    { name: "jobs", where: { id: 9 }, values: { status: 1, output: "hi\n" } },
  ]);
});

test("a write-back with no row to write to is refused before the script runs", async () => {
  let ran = false;
  const shell = { bash: async () => ((ran = true), { exitcode: 0, stdout: "", stderr: "" }) };
  await assert.rejects(
    actions.run_bash_script.run({ row: null, table: null, configuration: { code: "ls", stdout_field: "out" }, shell, db: recordingDb() }),
    /needs a triggering row/,
  );
  assert.strictEqual(ran, false);
});

const BashSkill = require("../run-bash-agent-skill.js");

test("the skill is v1's, under the trait name run_bash", async () => {
  assert.strictEqual(BashSkill.skill_name, "Run Bash command");
  assert.strictEqual(BashSkill.trait_name, "run_bash");
  const skill = new BashSkill({ timeout: 5 });
  assert.strictEqual(skill.skill_label, "Bash");
  assert.match(await skill.systemPrompt(), /run_bash tool/);
  const tool = skill.provideTools();
  assert.strictEqual(tool.function.name, "run_bash");
  assert.deepStrictEqual(tool.function.parameters.required, ["command"]);
});

test("a command runs locally, or over SSH when a host is given", async () => {
  const asked = [];
  const shell = {
    bash: async (script, opts) => (asked.push(["bash", script, opts]), { exitcode: 0, stdout: "hi\n", stderr: "" }),
    ssh: async (script, opts) => (asked.push(["ssh", script, opts]), { exitcode: 1, stdout: "", stderr: "nope" }),
  };
  const tool = new BashSkill({ timeout: 5 }).provideTools();
  assert.strictEqual(await tool.process({ command: "echo hi" }, { shell }), "hi\n");
  assert.strictEqual(
    await tool.process({ command: "ls /x", ssh_host: "db1", ssh_user: "deploy", ssh_port: 2222 }, { shell }),
    "nope\n[exit code 1]",
  );
  assert.deepStrictEqual(asked, [
    ["bash", "echo hi", { timeout_seconds: 5 }],
    ["ssh", "ls /x", { host: "db1", user: "deploy", port: 2222, timeout_seconds: 5 }],
  ]);
});

test("a command past its timeout is an error, and no shell is refused", async () => {
  const shell = { bash: async () => ({ exitcode: -1, stdout: "", stderr: "", timed_out: true }) };
  const tool = new BashSkill({}).provideTools();
  await assert.rejects(tool.process({ command: "sleep 99" }, { shell }), /longer than 60 seconds/);
  await assert.rejects(tool.process({ command: "true" }, {}), /cannot run commands/);
});
