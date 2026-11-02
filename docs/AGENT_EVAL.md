# Evaluating the coding agent

`feldspar agent eval` runs a suite of coding tasks against named models and
reports what they cost and whether they worked. It is the only thing in this
repository that spends money: everything in `cargo test` runs against a scripted
provider (§13), so the eval is a command an operator runs deliberately.

```sh
feldspar agent eval tests/agent-eval \
  --model anthropic/claude-sonnet-4-5 \
  --strong anthropic/claude-opus-4-1 \
  --cheap anthropic/claude-haiku-4-5 \
  --environment production
```

A model is `provider/model`, naming a provider row and one of its model rows in
the database the command connects to — the same two tables the admin UI's LLM
screens edit. `provider` on its own means that provider's default model. Leave
`--model` out and the agent is created the way the admin UI creates one: the
first provider, on its default model.

Flags:

| Flag | |
|---|---|
| `--model P/M` | the executor: the model that does the work |
| `--strong P/M` | the strong role: planning, summaries and escalated steps |
| `--cheap P/M` | the cheap role |
| `--task NAME` | run only this task (repeatable) |
| `--out DIR` | write the reports here instead of into the suite |
| `--keep` | keep each task's temporary directory, to read what it did |
| database flags | as every other command: `--environment`, `--database-url`, … |

The command exits non-zero when any task fails, so it can be the last line of a
script.

## What a task is

A task is a directory with a `task.toml`:

```toml
prompt    = "Add an About page at `/about`, linked from the navigation."
fixture   = "../fixture"        # copied into a temporary store, per task
verify    = "verify.sh"         # default; its exit status is the score
setup     = "setup.sh"          # optional, run in the copy before the agent
framework = "code"              # or `react`
output    = "dist"              # `code`: where the build writes
command   = "npm run build"     # `code`: the build
checks    = ["typecheck", "test"]
workflow  = "direct"            # optional; the builder agent's default is `planned`
max_steps = 60
max_cost  = 1.50
```

A directory in the suite without a `task.toml` is not a task — which is how a
shared fixture, a README and the reports sit beside them.

For each task the harness copies the fixture into a temporary directory, defines
a local file store over it, saves an application and **the builder agent the
framework declares** (§12 — the configuration an admin gets, not a bespoke
agent), runs it on the prompt, and then runs `verify.sh` in the copy. The store,
application and agent are removed afterwards; the **run rows stay**, because
they are the transcript behind every number in the report.

The verification script runs in the copy, with `FELDSPAR_EVAL_TASK`,
`FELDSPAR_EVAL_PROJECT` and `FELDSPAR_EVAL_RUN` in its environment. Its exit
status is the whole score: what the model says about its own work does not enter
into it.

## The report

`report.json` and `report.md`, per task and totalled:

| Metric | What it is |
|---|---|
| steps | model calls, over the run **and its sessions** |
| sessions | the planner run plus every feature session; 1 for a direct run |
| in / cached / out | prompt tokens, the cached part of them, completion tokens |
| cost | from the model rows' prices; `?` when a model called had none |
| edits | applied edits by cascade step — `exact`, `whitespace`, `indentation`, `fuzzy` |
| edit fails | edits refused after the whole cascade |
| detectors | rounds a doom-loop detector fired on |
| escalations | firings that handed the next step to the strong role |
| compactions | context compactions |
| time | wall clock, including verification |

A planned run spends most of its tokens in its sessions, so every number is
rolled up over the run tree (`sc_agent_builtins::run_tree`), not read off the
planner alone.

## Results

Nothing yet. The harness and the suite are built and tested; the first run
against a real provider is TODO 11.4, and needs somebody with a key — no test in
this repository calls a vendor, and neither does anything that built this page.
When it is run, the two rows go here:

| Ran | Executor | Strong | Passed | Steps | Cost | Cache hits | Edit fails |
|---|---|---|---|---|---|---|---|
| | | | | | | | |

Paste the `report.md` table beneath it and say which of the triggers below fired.

## Reading it: the decision triggers

From R§14, and the reason the harness records what it records rather than only
pass and fail. Each one says what to change **before** reaching for a bigger
model:

- **Edit failures high** → change the edit format for that model
  (`edit_format`: `apply_patch` or `whole_file` instead of `auto`) before
  changing the model. A cascade doing a lot of `fuzzy` matching is the same
  signal one step earlier: the model is quoting the file badly, and the format
  is what decides how much that costs.
- **Localisation failures high** — the model edits the wrong file, or reads its
  way around the tree before finding anything → invest in the repo map
  (`repo_map_tokens`, the session header) rather than the model.
- **Input tokens dominate the cost** → tighten clearing and compaction, and
  check the cache is actually being hit: the report's cached column against its
  input column. A cache hit ratio that collapses across a session means the
  prefix is not stable, which is a bug and not a tuning problem.
- **A cheap executor within ~10 points of a strong one on this suite** → keep
  the cheap one. Otherwise raise the executor's tier, or add a review step,
  before concluding the harness is at fault.

Two further readings this harness makes possible, which R§14 does not name:

- **Detector firings and escalations near zero while tasks fail** means the
  model is failing confidently: the loop control cannot help, and the task is
  either under-specified or genuinely beyond the model.
- **`stuck` conclusions clustered on one task** is usually that task's
  verification script or fixture, not the model. `--keep` and the run's
  transcript settle it.

## Caveats

- Ten tasks is a slice, not a benchmark; R§14 wants thirty. A difference of one
  task is noise.
- Task order is stable (name order) but model sampling is not: re-run before
  believing a one-task change.
- The suite is React only. Nothing here says anything about the other
  frameworks, and R§15's warning applies: the transfer is plausible and
  unproven.
- Re-run it whenever the models, the prompts or the tool set change. It is the
  only thing that will notice a prompt change that made a cheap model worse.

## The harness's own test

`crates/sc-cli/tests/agent_eval.rs` runs a two-task suite — one the scripted
model solves, one it only claims to have solved — through the whole harness on
`FakeProvider`. It spends nothing and runs in `cargo test`. A harness that
scored a passing task as a failure, or reported a run's tokens as zero, would be
believed, so it has a test that says otherwise.
