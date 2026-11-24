// **Run Bash command** — the agent skill (after Saltcorn v1's
// `@saltcorn/bash` `run-bash-agent-skill.js`): an agent given the `run_bash`
// trait can run a bash command the model writes, on this server or on another
// machine over SSH.
//
// The worker this runs on cannot start a process, so the command goes to the
// server through `shell`: `shell.bash` runs it here, `shell.ssh` runs
// `ssh -o BatchMode=yes [-o ConnectTimeout=…] [-p port] [user@]host "bash -s"`
// with the command as its stdin, exactly as v1 does — so only a host the
// server's user reaches without a password (a key, or an agent through
// SSH_AUTH_SOCK) is reached at all.
//
// The server offers the tool only to an administrator's run at the admin
// console, in act mode, because this module is unsafe: the command can do
// anything the server's operating-system user can.

"use strict";

/** The tool's answer: what the command wrote (stdout, then stderr), as v1
 * answers, and its exit code when that is not 0 — what a model needs to tell
 * a failure from an empty success. */
function toolAnswer({ exitcode, stdout, stderr, timed_out }, timeout) {
  if (timed_out) throw new Error(`the command ran for longer than ${timeout || 60} seconds and was stopped`);
  let output = `${stdout || ""}${stderr || ""}`;
  if (exitcode !== 0) output += `${output && !output.endsWith("\n") ? "\n" : ""}[exit code ${exitcode}]`;
  return output;
}

class BashSkill {
  static skill_name = "Run Bash command";
  // What an agent stores: v1 had no such name to keep.
  static trait_name = "run_bash";
  static description = "Run a bash command the model writes, on the server or over SSH";

  get skill_label() {
    return "Bash";
  }

  constructor(cfg) {
    Object.assign(this, cfg);
  }

  async systemPrompt() {
    return `If you would like to run a shell command, use the run_bash tool.
The run_bash command can be used to run commands locally or on a remote server via SSH.
Passwordless SSH access may be enabled on hosts the user specifies. in the run_bash tool,
you specify the command you would like to run, optionally with an SSH host to run it on.`;
  }

  static async configFields() {
    return [
      {
        name: "timeout",
        label: "Timeout (s)",
        type: "Integer",
        sublabel: "A command is stopped after this long. Blank is 60; at most 3600.",
      },
    ];
  }

  /** The timeout to ask for: the setting, or the server's default. */
  timeoutSeconds() {
    const timeout = Math.ceil(Number(this.timeout));
    return Number.isInteger(timeout) && timeout > 0 ? timeout : null;
  }

  async run_script(shell, command, ssh_host, ssh_user, ssh_port) {
    if (!shell) throw new Error("run_bash was called where the server cannot run commands");
    if (typeof command !== "string" || !command.trim()) throw new Error("run_bash needs a command to run");
    const timeout_seconds = this.timeoutSeconds();
    const answer = ssh_host
      ? await shell.ssh(command, {
          host: ssh_host,
          user: ssh_user || null,
          port: ssh_port || null,
          timeout_seconds,
        })
      : await shell.bash(command, { timeout_seconds });
    return toolAnswer(answer, timeout_seconds);
  }

  provideTools() {
    return {
      type: "function",
      process: async (row, { shell }) => {
        return await this.run_script(shell, row.command, row.ssh_host, row.ssh_user, row.ssh_port);
      },
      function: {
        name: "run_bash",
        description: "Run a bash command, returning the stdout and stderr",
        parameters: {
          type: "object",
          required: ["command"],
          properties: {
            command: {
              description: "The bash command to run",
              type: "string",
            },
            ssh_host: {
              description:
                "the remote SSH host to run this on. Only the machine name, not user. Run local if not supplied",
              type: "string",
            },
            ssh_user: {
              description: "The user on the remote SSH machine to run as.",
              type: "string",
            },
            ssh_port: {
              description: "The SSH port if different from the default 22",
              type: "number",
            },
          },
        },
      },
    };
  }
}

BashSkill._toolAnswer = toolAnswer;

module.exports = BashSkill;
