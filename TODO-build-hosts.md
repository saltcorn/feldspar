# Saltcorn v2 — Build hosts (milestones B1–B4)

Building an application's native app on another machine. The server stays on Linux; a Mac
registered as a build host builds the iOS targets for it. The design is
[docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §13.3, "Build hosts". Milestones are
prefixed **B**; work through them in order.

The rule everything else follows: **the build host never generates code, it only builds.** The
server writes the generated client and runtime from its own catalog before it hands a job out,
and the generated app names tables, never their ids, so a build host needs no database and
nothing on it can drift from the server's schema.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## B1 — Registering a build host

- [x] B1.1 `_fd_build_hosts`: a name, the targets it builds (`ios`, `ios_simulator`, later
      `android`), the hash of its token, when it was last seen, and what it last reported (OS,
      architecture, `feldspar` version, readiness per target).
- [x] B1.2 Settings → Build hosts: add a host (its token is shown once, with the command that
      starts the agent), rename it, change its targets, remove it (which revokes its token). The
      list shows whether each host is online and what each target still lacks.
- [x] B1.3 `feldspar build-agent --server <url> --token-file <path>` (or the token in
      `FELDSPAR_BUILD_TOKEN`; never on the command line): the same binary, started on the Mac. It
      needs no database. It checks the targets' requirements on its own machine and sends a
      heartbeat with what it found. `--once` checks a setup and exits.
- [x] B1.4 `/build-agent/*` routes that take the build host's bearer token and nothing else. A
      build host token opens no other endpoint, and no other token opens these.
- [x] B1.5 An agent that speaks another protocol version than the server is refused with a
      message saying which `feldspar` to install; the admin sees the reason on the host.

## B2 — Jobs

- [x] B2.1 `_fd_build_jobs`: a queued target build, with the application, the target, the build
      host that took it, its state and its lease. Jobs survive a restart, unlike local target
      builds, which live in memory.
- [x] B2.2 The build button: when the target's requirements do not hold on the server but an
      online build host builds it and last reported it ready, the job is queued instead of run
      here. A second press joins the queued or running job. The applications list says the
      target is ready, on that host.
- [x] B2.3 Before queueing, the server regenerates the client and runtime as for a local build.
      The project is packed as a zip when the agent fetches it, leaving out what the backup
      leaves out (`node_modules`), `.git`, and what the build makes (`android`, `ios`,
      `*-output`, `build-logs`). Signing keys (`*.jks`, `*.keystore`, `*.p12`) go only to an
      Android job.
- [x] B2.4 The agent claims a job by long-polling. The answer is the rendered target: command,
      arguments, environment (the iOS secrets included) and artifact path; the project comes
      from a second route.
- [x] B2.5 The agent unpacks into a workspace it keeps per application (`--workspace`, by default
      in Feldspar's data directory), runs the install step and the command with the local
      build's code (`run_target`), and checks that the artifact is there.
- [x] B2.6 The log goes back in chunks while the build runs. The server writes it to
      `build-logs/` as a local build does, so the admin can follow it.
- [x] B2.7 The artifact is streamed up and written into the store at the usual path
      (`ios-output/app.ipa`), so it is downloaded from Files as before.
- [x] B2.8 A heartbeat (or a piece of log) renews the lease while the job runs. A job whose lease
      runs out fails with a reason saying the build host stopped answering.
- [x] B2.9 Tests: a fake agent in the test suite claims a job, sends a log and an artifact, and the
      file is in the store; a lost agent fails its job; a wrong token opens nothing. The real
      agent builds a job end to end against a server on a real port.
- [x] B2.10 The agent refuses a plain `http://` server unless it is this machine, because the
      token, the build secrets and the signing keys would cross the network unencrypted.
- [x] B2.11 Review fixes: no job stays open forever, and no stale result is kept.
      - An agent asking for a new job fails the one it still holds: it lost that one.
      - A queued job fails once no online host builds its target.
      - Two presses of Build at once queue one job.
      - An artifact that arrives after its lease ran out is not stored, and a failed upload
        leaves no `.part` file.
      - Build commands never see `FELDSPAR_BUILD_TOKEN`.
      `--insecure-http` allows it for testing on a trusted network, with a warning. After each
      build the agent deletes the signing keys from its workspace.

## B3 — Readiness and control

- [ ] B3.1 Readiness says where a target builds: here, on a named build host, or nowhere (with the
      reasons from both). The button no longer warns "needs macOS" when a Mac is registered.
- [ ] B3.2 Cancelling a running job: the agent hears it in its next heartbeat answer and stops the
      build.
- [ ] B3.3 A timeout per job, so a build that hangs fails instead of holding the target forever.
- [ ] B3.4 The agent's workspace keeps `node_modules` and the CocoaPods between builds, and only
      replaces the project's own files.
- [ ] B3.5 `file_tail` in `crates/sc-server/src/build_agent.rs` reads the whole build log into
      memory to keep its last 32 KiB. It should seek to the end and read only that part, since an
      APK build log can be several megabytes.
- [ ] B3.6 A job is claimed only by a host that is online, speaks this protocol and reported the
      target ready. Today any host listing the target can take it and fail it.
- [ ] B3.7 Readiness on a build host counts the module settings: the heartbeat answer sends the
      target's `env` with its `requires`. A `pod` found only through `FELDSPAR_POD_DIR` set as a
      setting is reported missing today, so the host is never chosen.
- [ ] B3.8 The agent removes the target's artifact before each build, so a build that exits 0
      without writing one can't upload the last job's file as new.
- [ ] B3.9 Signing keys are removed when the agent stops mid-build too: on a refusal, on
      SIGINT/SIGTERM, and when the build panics (spawn the build as a task and fail the job).
- [ ] B3.10 `store_file` in `crates/sc-server/src/build_agent.rs` goes through the file store's
      checked resolver (symlinks resolved, kept inside the root), like every other store write.
- [ ] B3.11 The project zip is streamed on both sides instead of held in memory, and `.gradle`,
      `dist`, `build` and `.expo` stay out of it.
- [ ] B3.12 A `requires` variable can be marked secret. Its value is then masked in
      `from_environment`, the build log and the tooltip.
- [ ] B3.13 The agent: a job it can't parse is reported failed instead of stopping the agent, and a
      server that is down at startup is retried like one that goes down later.
- [ ] B3.14 A total size cap on a job's log.
- [ ] B3.15 `builds_on_hosts` also applies to the create and update answers of an application,
      not only to the list.
- [ ] B3.16 Settings → Build hosts: Remove asks first (it revokes the token), editing ends when the
      host is removed, the list refreshes while a host is "Never connected", and the error
      fallbacks are translated.
- [ ] B3.17 The folded sidebar's tooltip keeps the link's name when a target builds on a host or
      takes a variable from the environment.
- [ ] B3.18 `_fd_build_jobs`: an index on `(application_id, target)` and on `state`, and old jobs
      pruned; `latest_build_job` asks the database for the newest one only.

## B4 — Documentation

- [ ] B4.1 OPERATIONS: setting up a Mac as a build host (Xcode, CocoaPods, the distribution
      certificate in the keychain, running the agent as a service).
- [ ] B4.2 TECHNICAL_DESIGN §13.3 brought up to date.

## Later

- [ ] Android on a build host too, for a server without the Android SDK.
- [ ] Choosing between several build hosts for one target (least busy, or the one the
      application names).
- [ ] A build host keeps its own signing secrets (the distribution certificate, the keystore and
      its password), configured on the Mac. The server then sends no secrets with a job, and a
      stolen build host token no longer reveals them.
- [ ] Encrypting the build secrets at rest. The App Store Connect key and the keystore password are
      plain JSON in the database today, masked only on the wire.
