# Saltcorn v2 — Live updates (milestones L1–L6)

Ordered, checkable task list for **real-time updates in applications**: a server pushing to the
browser while the page is open. Earlier lists are archived in [docs/TODO-mvp.md](./docs/TODO-mvp.md),
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-31.md](./docs/TODO-post-mvp-31.md) and
[TODO-analytics.md](./TODO-analytics.md). This builds on streams
([docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §14.3, [docs/tutorial-streams.md](./docs/tutorial-streams.md)),
the access model (§7.3), triggers and workflows (§10.2, §10.3) and applications (§13.2).

The design comes first, then the milestones. Each milestone is prefixed **L** and its tasks are
numbered L1.1, L1.2, …. Work through them in order: a milestone assumes every earlier one is done.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. What has to work

| | Use case | Who publishes | Who may receive | Shape |
|---|---|---|---|---|
| **A** | Status of a long-running action: a toast on start and finish, a progress bar | A workflow step, an action, a code body | **The user who started it**, and nobody else | Small JSON, at most a few per second, latest value matters most |
| **B** | Kanban: one user moves a card, another user's board moves it too | A row write (the card's `column` changes) | Everyone who can **read that card** and is looking at that board | A row change, filtered to one board |
| **C** | Collaborative document editing with remote cursors | The **browser**, many times a second | Everyone who can **read the document**. Only those who can **update** it may edit | Binary CRDT updates plus ephemeral cursor state |
| **D** | The frontend wants to hear about inserts, updates and deletes on a table | Any row write | Everyone who can **read the row** | A row change |
| **E** | An external stream (MQTT, RSS) reaches the frontend | A stream provider | The stream's `min_role` | The existing envelope |

E already works, through one WebSocket per stream per page (§13.2, "The observe socket"). The
other four are not possible today.

## 2. The idea: everything is a stream, and a stream has topics

The proposal in the brief holds up: **every update flows through a stream**, and an application
declares the streams it exposes, exactly as it does now for MQTT. That keeps one entity, one
admin screen, one place to set access, one socket and one client API. A trigger can also listen to
any of these streams for free, because the stream→trigger bridge (§10.2) already exists.

What does not hold up is the access model streams have today. A stream has a single `min_role`.
That is enough for E, but not for A (each user sees only their own toasts), B and D (each user
sees only rows they may read) or C (access is per document). So a stream gains **topics**.

> A **topic** is a sub-channel of a stream, named by a string. A subscriber subscribes to one
> topic. Access is decided **per topic** (at subscribe time, and again whenever it could have
> changed) or **per element** (for every element, against the subscriber).

How topics are named and authorised is declared by the stream's provider, as a function of its
configuration. This is the same way `element_type` already works:

```rust
pub enum TopicSpec {
    /// One topic. Access is the stream's `min_role` (every stream today, E).
    Single,
    /// The topic is a user id. A subscriber gets exactly their own topic and
    /// never names it. Anonymous callers are refused. (A)
    User,
    /// The topic is the primary key of a row of `table`. Subscribing needs
    /// **read** access to that row; publishing from a client needs **update**
    /// access. Access uses §7.3's rule (role floor OR ownership formula). (B presence, C)
    Row { table: String },
    /// One topic per subscription filter. **Every element** is checked against
    /// the subscriber's read access to the row it carries. (B, D)
    PerElementRow { table: String },
}
```

The stream's `min_role` remains a **floor on opening a subscription at all**, still defaulting to
admin-only. Topic and element checks come **on top of it** and can only narrow access. A stream
an admin has not considered stays closed.

### Three new built-in providers

All three live in `sc-stream::providers` beside `mqtt`. All three are **in-process**: their
elements come from Saltcorn itself, not from a broker.

1. **`internal`** — elements are published by Saltcorn code: the `publish` action, `live.publish`
   in a code body, or (if the configuration allows it) a client. Settings:
   - the element type, as declared JSON keys (the MQTT form's repeating group, reused);
   - `topics`: `single | user | row` (and `table` when `row`);
   - `retain_last`: keep the latest element per topic for a TTL (default 1 h), sent to a new
     subscriber first. This is MQTT's *retained* message, and it lets a page reloaded during a
     long job show the current progress straight away;
   - `client_publish`: `off` (the default) | `on`, under the rule below;
   - `presence`: off | on, with `presence_fields` (user columns shown to other members; the
     default is none, so only the user id is shown).
2. **`table_changes`** — one element per committed insert, update or delete on `table`, with
   `TopicSpec::PerElementRow`. Settings: `table`, `ops` (any of insert/update/delete),
   `payload`: `row | key` (`key` sends `{op, key}` and the client refetches; use it for wide rows
   or joined reads). The element type is **derived from the table's fields**, so the generated
   TypeScript is the table's row type.
3. **`document`** — a collaborative document stored in a field of a row. `TopicSpec::Row`.
   Settings: `table`, `field` (of the new type `collab_doc`), and an optional `text_mirror`
   (a `String` field kept equal to the document's plain text, so the document can be searched,
   filtered and read over REST). Presence is always on.

E is unchanged: `mqtt` and module providers are `TopicSpec::Single`.

### Why not a separate pub/sub system beside streams

A separate "channels" entity would need its own admin screen, its own access settings, its own
socket and its own client methods, and a trigger could not listen to it. Streams already have the
envelope, the never-block delivery rule, the lagged notice, the ring, the counters and the Observe
screen. Topics are the one thing they lack, and adding them is smaller than building a second
system.

## 3. Authorisation

This is where the design is strictest. The rules:

1. **The socket authenticates exactly as the application's REST API does.** It uses the same
   function: the app's session cookie, or the native client's session header (`SessionClient`).
   A subscriber is a `(user, role)` pair taken at upgrade time and **re-read** later (rule 6).
2. **Origin is checked on every browser upgrade.** A WebSocket handshake is not covered by CORS,
   and `SameSite=Strict` does not help between sibling subdomains: `appb.example.com` is
   *same-site* with `appa.example.com`. With *Share sign-in between applications* turned on, the
   session cookie is even scoped to the base domain. So a page on app B could otherwise open app
   A's socket as the signed-in user. If a browser sends an `Origin`, it must be the app's own
   origin (or that app's preview host). A request with no `Origin` is accepted only with
   header-borne credentials, never with a cookie. This also closes the hole on the existing
   `{mount}/streams/{name}/observe` route (L1.2).
3. **Exposure is the application's decision.** A stream the app does not list in `streams` does
   not exist for that app. Neither does a `table_changes` or `document` stream whose table is
   outside the app's table subset (refused when the app is saved, and re-checked at subscribe
   time).
4. **Denial looks like absence.** An unknown stream, an unexposed stream, a stream below the
   caller's role, a row topic for a row that does not exist and a row topic for a row the caller
   may not read all get the **same** `unavailable` error. This is §7.3's rule, so subscribing is
   never a way to probe whether a row exists.
5. **Per-element checks for row changes.** For a `table_changes` element and a given subscriber:
   - **insert**: delivered if the new row is readable and matches the subscription filter;
   - **delete**: delivered as `{op:"delete", key}` if the **old** row was readable and matched.
     Nobody learns that an unreadable row was deleted;
   - **update**: four cases, depending on whether the old and new rows were each readable and
     matching: `update` (both), `enter` (new only, with the row), `leave` (old only, **key
     only**), nothing (neither). `leave` is what makes a kanban card disappear from board 7 when
     it moves to board 8, and also when it stops being readable.
   - The delivered row is **the row the app's REST read returns for that table**: the same
     projection, so anything REST hides (a users table's password hash, a field outside the
     read projection) is hidden here too. `collab_doc` fields are never included.
   - "Readable" uses the access rule exactly: role ≥ `min_role_read` short-circuits; otherwise
     the ownership formula is evaluated **reified** (the reference evaluator, §7.3) against the
     row and the subscriber's `user`, with Ⱶ/Ↄ prefetches batched once per element. This works
     the same on RLS tables, because RLS and runtime checks are specified to give the same
     verdicts.
   - The subscription filter uses the REST filter vocabulary (`FilterOp`) and is evaluated in
     Rust. It gets **parity tests** against the REST read, with the same null semantics, the way
     ownership formulas have parity tests.
6. **Access is re-checked while the socket stays open.** A subscription is a standing read, so
   one checked only at subscribe time would leak after a logout, a role change or an ownership
   change. Each connection re-reads its session and user every `SessionStore::CACHE_TTL_SECONDS`
   (60 s). A gone session closes the socket. A changed role re-checks every subscription. A
   `Row`-topic subscription is also re-checked **immediately** when its row changes, or when a
   row changes in any table its ownership formula reads (the Ⱶ paths and `AggUse` records already
   list these). A subscription that fails a re-check gets `revoked` and is dropped. Logout on this
   node closes that session's sockets at once, and L6's bus does the same on every node.
7. **Client publishing is opt-in and stamped.** A client may publish only to an `internal` stream
   with `client_publish: on`. On a `single` topic this needs the stream's `min_role`; on a `row`
   topic it needs **update** access to the row; a `user` topic never accepts client publishes.
   The value is validated against the element type. The server stamps
   `source: {user, connection}`, and a client cannot set `source` or `received_at`.
   Publishing is rate-limited per connection.
8. **Server-side publishers are trusted.** The `publish` action, `live.publish` in code bodies and
   workflow steps are admin-authored configuration, and they publish to any topic. This is the
   trust an `insert_row` action already gets.
9. **Presence reveals only what the admin chose.** A member is `{connection, user_id}` plus the
   `presence_fields` the stream's settings list, plus client state (≤ 2 KB, for example a
   cursor). Presence is visible only to members who passed the topic's read check.

## 4. Transport: one multiplexed socket per page

`GET {mount}/live` is **one WebSocket per page**, carrying any number of subscriptions. The
alternative, one socket per stream (today), costs a connection, a handshake and an auth check per
stream, and a kanban page that wants a card stream, a presence topic and a toast stream would
open three. JSON text frames, with binary payloads (document updates) in base64:

```
client → server
  {"type":"subscribe",   "sub":"s1", "stream":"cards_live", "topic"?: "7", "filter"?: {...}}
  {"type":"unsubscribe", "sub":"s1"}
  {"type":"publish",     "sub":"s2", "value": {...}}            // internal + client_publish
  {"type":"presence",    "sub":"s2", "state": {...}}            // presence-enabled topics
  {"type":"doc_update",  "sub":"s3", "update": "<base64>"}      // document streams
  {"type":"ping"}

server → client
  {"type":"ready",    "sub":"s1", "element_type":…, "replayed":n, "can_publish":bool}
  {"type":"element",  "sub":"s1", "envelope":{ "stream","topic"?, "value","received_at","source"? }}
  {"type":"lagged",   "sub":"s1", "dropped":n}
  {"type":"presence", "sub":"s2", "members":[…]}  then  {"type":"presence_diff", …}
  {"type":"doc_sync", "sub":"s3", "update":"<base64>", "read_only":bool}
  {"type":"doc_update","sub":"s3","update":"<base64>"}
  {"type":"revoked",  "sub":"s1"}                 // access lost: re-checked and refused
  {"type":"error",    "sub"?:"s1", "code":"unavailable"|"invalid"|"rate_limited"|…, "message"}
  {"type":"pong"}
```

- **Refusals before the upgrade** (no session where the app needs one, a bad Origin) are HTTP
  statuses, for §14.3's reason: a browser cannot read a failed handshake's body. Everything
  after the upgrade is per-subscription and uses `error` frames.
- **Delivery is at-most-once and never blocks the publisher** (§14.3's rule). A slow socket gets
  `lagged`. A reconnecting client **resubscribes and gets `ready` again**. For `table_changes`,
  the client helper treats a reconnect or a `lagged` as **resync**: `useLiveRows` refetches.
  Replaying a gap by sequence number is left out on purpose (see §8).
- **Limits** (configuration, with survivable defaults): 64 subscriptions per connection, 16
  connections per user per app, 64 KB per frame (1 MB for a document's first `doc_sync`), client
  publishes 20/s and presence updates 20/s per connection (presence is coalesced to the latest
  state per 50 ms). A ping every 30 s; a connection silent for 90 s is closed.
- **The per-stream app socket is removed.** `{mount}/streams/{name}/observe` and
  `observeStream_x()` are replaced by the live socket (no backwards compatibility in the
  prototype). The admin's `/api/streams/{id}/observe` stays, because the admin Observe screen
  watches every topic.

## 5. Inside the server

```
 publishers                          sc-live (new)                      sockets
 ──────────                          ─────────────                      ───────
 publish action ─┐
 live.publish() ─┼─► LiveHub::publish ─► internal provider sink ─┐
 client publish ─┘        (L6: via sc-bus)                       │
                                                                 ├─► stream broadcast (exists)
 row write ─► TableEvents ─► after-commit ─► table_changes sink ─┤        │
                                                                 │        ├─► trigger bridge (exists)
 doc_update ─► DocRoom (yrs) ─► document sink ───────────────────┘        │
                                                                          └─► Fan-out task per stream
 mqtt / module providers ─► sink (exists) ──────────────────────────────►      │ topic index:
                                                                               │ (stream, topic) → subs
                                                                               └─► per-subscriber check
                                                                                    ─► connection queue
```

- **`sc-live`** (new crate, placed above `sc-stream` and the ownership evaluator, below
  `sc-server`) holds the hub, the topic index, the protocol types, the subscription state machine
  and the **authorisation decisions as pure functions** (`may_subscribe`, `deliver_to`,
  `may_publish`), so they can be tested with tables of cases rather than sockets. `sc-server`
  mounts the route and connects the seams, as it does for streams.
- **Fan-out is indexed by topic.** Each running stream has one receiver per process that routes
  each element to the subscriptions on its topic (`HashMap<(stream, topic), subs>`). Sending
  every element to every socket and filtering there would cost elements × sockets, and a
  per-user `job_status` stream with ten thousand users is exactly that case.
- **Per-element checks are grouped by user.** For one `table_changes` element, the verdict is
  computed once per distinct `(user, filter)`, not once per connection. A subscriber whose role
  meets `min_role_read` skips the evaluator. One common formula shape gets a fast path:
  `owner_field === user.id` is a field comparison with no isolate.
- **Row changes are published after commit.** `TableEvents::emit` currently runs **before** a
  `SharedTx` commits (a workflow step's write, an import). Live delivery cannot do that, because
  it would broadcast rows that are then rolled back. `SharedTx` gains `after_commit` callbacks
  (dropped on rollback). A write outside a shared transaction has already committed when it
  emits, so it publishes immediately.
- **Writes nobody watches stay free.** The row layer asks `TableEvents::observes(table, op)`
  before doing any work for an event (`Catalog::observes_writes`). Live answers true only while
  some connection is **subscribed** to a `table_changes` stream on that table with that op.
  Having such a stream defined, or exposed by an app, is not enough. Otherwise a write costs one
  lookup in a map of counters, so an update fetches its pre-image only when somebody will read
  it. Live updates cost nothing until somebody is watching.
- **The cost is per table, not per application.** Applications share one data layer (§13.2).
  While anyone watches `cards`, every write to `cards` pays, whichever app made it. When a
  write is watched, it pays: the pre-image read for an update or delete (which a trigger on that
  table already needs, so the two share it), and for a write inside a `SharedTx`, registering an
  after-commit callback. Nothing else.
- **`emit` does no access work.** It hands `{op, old, new}` to the stream's sink, which is a
  synchronous, non-blocking broadcast send (§14.3), and returns. Filter evaluation, ownership
  checks and socket writes all run in the per-stream fan-out task, **off the write's request
  path**. A thousand subscribers make fan-out slower, not the `INSERT`. If fan-out falls behind,
  the broadcast channel reports `lagged` to those subscribers, and they resync. It never
  back-pressures the writer.
- **The `TableEvents` slot holds one listener.** `Catalog::set_table_events` replaces what was
  there, and today that is the trigger dispatcher (`sc-server::triggers`). Installing live there
  would silently switch off every table trigger. So the seam becomes a fan-out: the catalog
  holds a list of listeners, `observes` is true if any listener observes, and `emit` calls only
  those that do (L3.2).
- **Documents use `yrs`**, the Rust port of Yjs, whose binary format is Yjs's own. That way the
  browser uses the standard `yjs` package and every editor binding that exists for it
  (ProseMirror/TipTap, CodeMirror, Monaco, Quill). The server keeps one `DocRoom` per open
  `(stream, row)`: it loads the field's stored state, applies authorised updates, relays them,
  and **persists** the merged state (debounced 2 s, and when the last member leaves) in a
  transaction holding a row lock, merging with what is stored. CRDT merges are idempotent, so a
  race with another process loses nothing. A persist writes the field and `text_mirror` through
  the row layer as a system write, so update triggers and `table_changes` streams see it (the
  `collab_doc` bytes themselves are never in an element). REST and `update_rows` **refuse** to
  write a `collab_doc` field, because a whole-value overwrite would discard concurrent edits.
  The only server-side writer is `live.document(…)` in a code body, which applies a CRDT update.
- **Cursors are presence.** Yjs's *awareness* (cursor, selection, user colour) is ephemeral
  per-connection state. That is what presence is, so the client adapter maps awareness onto
  presence frames, and only one mechanism exists.

## 6. In the application

The generated client (`src/feldspar/client.ts`) gets one typed accessor per exposed stream,
based on its `TopicSpec` and element type. It also has one shared connection that reconnects with
backoff and resubscribes:

```ts
live.job_status.subscribe(handlers)                       // User: no topic argument
live.cards_live.subscribe({ where: { board: 7 } }, handlers)   // PerElementRow
live.board_presence.join(boardId, handlers)               // Row + presence
live.board_presence.publish(boardId, value)               // only if client_publish
live.doc_body.open(docId)                                 // Document → a Y.Doc + awareness
```

React apps also get hooks (`src/feldspar/live-react.ts`, emitted only for the React framework).
The non-React core is framework-neutral, so the Vue plugin and Saltcorn UI can use it too:

```tsx
useStream(live.job_status, (env) => toast(env.value.message));              // A, E
const cards = useLiveRows(client.cards, { where: { board: 7 } });             // B, D
const { members, setState } = usePresence(live.board_presence, boardId);      // B, C
const { doc, awareness, readOnly, status } = useDocument(live.doc_body, docId); // C
```

`useLiveRows` reads with the REST client, subscribes with the **same filter**, applies
`insert/update/delete/enter/leave`, keeps the requested order, and refetches on `resync`. With
`payload: key`, or a `select` that has joins, it refetches the changed keys instead of trusting
the element.

## 7. Server-side publishing (A)

- **`publish` action** (`sc-core-actions`): settings `stream`, `topic` (a formula; for a `user`
  stream the default is `user.id`, the event's user), `value` (a map of formulas). It works as a
  trigger body and as a workflow `Action` step. A workflow run carries the user who started it
  (`_fd_runs.user_id`), so `user.id` is "the user who clicked the button". A `user` topic with
  no user (a periodic trigger) is a run error that names the trigger, not a silent drop.
- **`live.publish(stream, topic, value)`** in a JS code body and in a Python body, beside `db`,
  `fetch`, `fs` and `trigger`. Inside a long loop, this is how a progress bar moves.
- **Durable notifications are a table.** Live delivery is ephemeral: a toast published while the
  user is offline is lost, and `retain_last` keeps only the latest one. An app that needs a
  notification inbox inserts a row into a `notifications` table and exposes a `table_changes`
  stream on it filtered to `recipient === user.id`. That is D, and needs nothing new.

## 8. Deliberately left out

- **Gap replay by sequence number.** Resync-by-refetch is simpler and always correct. Replay
  would need a per-topic history, which streams deliberately do not keep (§14.3).
- **Writes made outside Saltcorn** (direct SQL, another tool) produce no `table_changes`
  elements. Postgres logical decoding could add them later as a different provider.
- **Sharing one external subscription across a cluster.** In L6, internal, table-change and
  document elements cross processes over the bus. An MQTT stream is still subscribed once per
  process (§14.3's limitation, unchanged).
- **Offline document editing.** Yjs supports it on the client (`y-indexeddb`), and the server
  needs nothing extra, but it is not built or tested here.
- **A seeded `notifications` stream.** Bootstrap seeds no entity nobody asked for (§7.4's rule).
  The tutorial creates one.

---

# Part II — Milestones

## Ground rules for every milestone

- **Runnable after every milestone.** When a milestone's last task is ticked, `feldspar serve`
  starts, the admin UI loads, a React app can be built against the new client, and the
  milestone's **Try it** walkthrough works by hand in two browser windows signed in as
  different users. The milestone's last task turns that walkthrough into a part of
  `docs/tutorial-live.md` and a definition-of-done test.
- **Authorisation has its own tests, not just happy paths.** Every milestone that adds a way to
  receive something adds tests that a user who must not receive it **does not**, including
  denial-looks-like-absence (the same `unavailable` frame for "missing" and "forbidden").
- **Two users, two connections.** Socket tests use a real server and real WebSocket clients
  (the `app_streams.rs` pattern), not just unit tests of the hub.
- **Clients.** An `sc-api` schema change regenerates `client.ts` in every UI that has a copy
  (`ui/admin`, `ui/ide`, `ui/builder`, `ui/analytics`). The generated app client is checked by
  `typescript_typecheck.rs`.
- **Browser checks.** Anything that renders (toasts, the kanban demo, the editor) gets a
  puppeteer check against a scratch DB, as in the analytics milestones.
- **Migrations.** No backwards compatibility. If stored data must change shape, idempotent SQL
  goes in `TABLES_RENAME.sql`, for Postgres and SQLite.
- **Documentation follows the code.** Each milestone updates `docs/TECHNICAL_DESIGN.md` (a new
  §14.10 "Live updates", and edits to §13.2 and §14.3) and grows `docs/tutorial-live.md` by one
  part.

---

## L1 — One live socket, with the access rules (E, done properly)

Moves the existing app-side stream observing onto `{mount}/live`, with the Origin check and
re-checking. No new providers yet. Everything later builds on this.

### Phase 1: the crate and the protocol
- [x] **L1.1** Create `sc-live`: the protocol frame types (serde, tagged on `type`), the
  `Subscription` state machine, the topic index and the limits as configuration. Decide where it
  sits in the layers and record it in TECHNICAL_DESIGN §2. Unit-test the frame round-trips,
  including that an unknown client frame type is an `invalid` error and not a dropped
  connection.
- [x] **L1.2** Origin check for browser upgrades: one function used by `{mount}/live`, the
  existing `{mount}/streams/{name}/observe` (until L1.7 removes it) and the admin sockets
  (`/api/streams/{id}/observe`, chat, LSP). Test: with a shared session cookie, a sibling
  subdomain's Origin is refused with 403, the app's own Origin and its preview host are
  accepted, and a request with no Origin is refused when it carries a cookie.
- [x] **L1.3** `TopicSpec` on `StreamProvider` (default `Single`), and `topic` as an optional
  field of the envelope. The envelope is a wire contract, so the change goes in the CHANGELOG
  and the docs. `mqtt` and `PollingProvider` stay `Single`.

### Phase 2: the socket
- [x] **L1.4** Mount `GET {mount}/live`. Authenticate with the same function the app's REST API
  uses (cookie or native header). Anonymous is allowed and holds the public role, and each
  subscription decides. Pre-upgrade refusals are statuses; everything after is a frame.
- [x] **L1.5** Subscribe and unsubscribe for `Single` streams: exposure (`exposes_stream`), the
  stream's `min_role`, and the one `unavailable` code for unknown, unexposed and forbidden
  streams. `ready` + ring replay + `element` + `lagged` + `status`, reusing `observe.rs`'s
  sending code rather than copying it. Tests: two subscriptions on one socket both receive; a
  role below the floor gets `unavailable`, the same frame as a misspelt name.
- [x] **L1.6** Re-checking: each connection re-reads its session and user every
  `CACHE_TTL_SECONDS`. A gone session closes the socket with a reason. A lowered role sends
  `revoked` for every subscription that no longer passes. Logout on this node closes that
  session's sockets immediately (a hook on `SessionStore::logout`). The clock is a parameter, so
  the test runs in milliseconds.
- [x] **L1.7** Remove the per-stream app route `{mount}/streams/{name}/observe` and
  `observeStream_x()`. Keep the admin observe socket. Update tutorial-streams step 6.
- [x] **L1.8** Limits: subscriptions per connection, connections per user per app, frame size,
  ping/pong and idle close. Each limit has a test that crosses it and checks the error frame.

### Phase 3: the client
- [x] **L1.9** Generated client: one shared `LiveConnection` (lazy open, reconnect with capped
  exponential backoff and jitter, resubscribe everything, `resync` to every handler after a
  reconnect), and `live.<stream>.subscribe(handlers)` typed from the element type. Update
  `typescript_typecheck.rs`.
- [x] **L1.10** React hooks file `src/feldspar/live-react.ts`, emitted for React apps only:
  `useStream(accessor, onElement)` with cleanup on unmount and a `status` value
  (`connecting | open | reconnecting`). Add it to the React scaffold's `AGENTS.md` contract
  (§13.3) so the coding agent knows about it.
- [x] **L1.11** Admin: the Streams list shows live subscriber counts per stream (from the hub),
  beside the existing counters.

### Try it
1. Run the tutorial-streams broker and `boiler` stream. Set its min role to Member and expose it
   on a React app.
2. In the app, `useStream(live.boiler, …)` shows the latest temperature. `mosquitto_pub` a
   reading and it updates without a reload.
3. Open the browser's network tab: one socket, `/live`.
4. Sign out in another tab: within a minute the socket closes. Sign in as a user below Member:
   the subscription gets `unavailable`.

- [x] **L1.12** Tutorial part 1 (`docs/tutorial-live.md`: the live socket, observing an external
  stream from React) and its definition-of-done test. TECHNICAL_DESIGN §14.10 started; §13.2's
  observe-socket paragraph rewritten.

---

## L2 — Internal streams and server-side publishing (A)

### Phase 1: the provider
- [ ] **L2.1** The `internal` provider: config spec (declared keys, `topics: single|user|row`,
  `table` when `row`, `retain_last` + TTL, `client_publish`, `presence`, `presence_fields`), and
  `element_type` and `TopicSpec` from the configuration. Validation: the `row` table exists and
  has a single-column primary key, and every `presence_fields` entry is a column of `users`.
- [ ] **L2.2** `LiveHub`: a process-wide registry from stream name to the internal provider's
  sink, filled in by `subscribe` and emptied when the subscription is dropped.
  `LiveHub::publish(stream, topic, value)` validates against the element type (counted as
  `malformed` and refused on a mismatch) and delivers. Publishing to a stopped or unknown stream
  is an error the caller sees.
- [ ] **L2.3** `retain_last`: the latest envelope per topic, with a TTL and a cap per stream,
  sent after `ready` (counted in `replayed`).

### Phase 2: topic access
- [ ] **L2.4** `User` topics: the subscriber names no topic and gets their own user id;
  anonymous gets `unavailable`. Test: user X's elements never reach user Y, including through
  `retain_last`.
- [ ] **L2.5** `Row` topics: subscribing checks read access to the row with the §7.3 rule
  (`may_subscribe` in `sc-live`, a pure function over row, user and table access, plus a test
  table). A missing row and an unreadable row give the same `unavailable`. Test on a runtime-check
  table and on an RLS table.
- [ ] **L2.6** Re-checking `Row` topics when they could have changed: on a write to the topic's
  row, or to any table its ownership formula reads (Ⱶ paths and `AggUse`), re-check the affected
  subscriptions and send `revoked` to those that now fail. Test: removing a user's share row
  (the tutorial-ownership `shares` pattern) revokes their subscription without waiting for the
  60 s interval.

### Phase 3: publishers
- [ ] **L2.7** The `publish` action in `sc-core-actions`: `stream` (a picker over internal
  streams), `topic` (a formula, defaulting to `user.id` for `user` streams), `value` (a map of
  formulas). A `user` topic with no user is an error naming the trigger. Works as a trigger body
  and as a workflow Action step. Test: a workflow started by user X publishes "started", sleeps,
  publishes "finished", and only X's socket receives both.
- [ ] **L2.8** `live.publish(stream, topic, value)` in JS code bodies (beside `db`) and in
  Python bodies (`sc-python`), with the same validation. Not bound in formulas. Test: a
  `run_js_code` loop publishing ten progress elements arrives in order.
- [ ] **L2.9** Admin: the stream form renders the internal provider's settings (it already
  renders whatever the spec declares, so check the repeating group and the conditional `table`
  field). The Observe screen gets a **topic** column and a topic filter box. The trigger form's
  action picker lists `publish`.
- [ ] **L2.10** Client: `live.<s>.subscribe(handlers)` for `user` streams (no topic) and
  `live.<s>.subscribe(topic, handlers)` for `row` streams, typed. React:
  `useStream(live.job_status, …)` and a `useLatest(live.job_status)` helper that returns the
  latest element (what a progress bar wants).

### Try it
1. Create an internal stream `job_status`: keys `message` (text), `progress` (float, optional),
   topics `user`, `retain_last` on, min role Member. Expose it on a React app.
2. Create a workflow `long_report`: `publish` "Started" → `wait` 10 s with progress publishes in
   a code step → `publish` "Done". Expose it as a trigger.
3. As Alice, click the button: a toast says "Started", a progress bar moves, and a toast says
   "Done". Bob, signed in at the same time, sees nothing.
4. Reload Alice's page halfway through: the progress bar shows the current value straight away.

- [ ] **L2.11** Tutorial part 2 (notifications and progress from a workflow) and its
  definition-of-done test. TECHNICAL_DESIGN §14.10 internal streams, topics, `publish`; §10.1
  `live` in code bodies.

---

## L3 — Table change streams (B, D)

### Phase 1: getting the event out of the write path, correctly
- [ ] **L3.1** `SharedTx::after_commit(callback)`: run after commit, dropped on rollback. Test: a
  workflow step whose write is rolled back publishes nothing; one that commits publishes once,
  after the commit.
- [ ] **L3.2** Make the `TableEvents` seam hold more than one listener:
  `Catalog::add_table_events` instead of the replacing `set_table_events`, with `observes` as the
  OR over listeners and `emit` called only on the listeners that observe. Update the callers
  (`sc-server::triggers` and the tests that install a dispatcher). Test: with both the trigger
  dispatcher and live installed, a table trigger still fires.
- [ ] **L3.3** The live `TableEvents` listener: `observes(table, op)` is true only while a
  connection is **subscribed** to a `table_changes` stream on that table with that op (a counter
  per `(table, op)` that the hub keeps, read without awaiting anything). A stream that is defined
  and exposed but has no subscriber does not count. `emit` hands `{op, old, new}` to the stream's
  sink after commit, and does no filter or access work. Tests: an unobserved update does not
  fetch its pre-image (the existing choke-point test pattern); after the last subscriber
  unsubscribes, `observes` is false again; and a write's latency does not grow with the number
  of subscribers (fan-out work runs off the write path).

### Phase 2: the provider and per-element access
- [ ] **L3.4** The `table_changes` provider: `table`, `ops`, `payload: row|key`. The element type
  comes from the table's fields (re-derived when the schema changes, through the
  `SchemaChanged` observer, so a new column reaches the generated types at the next build). The
  element is `{op, key, row?}`. Saving the stream refuses a table that does not exist. Saving an
  app refuses an exposed `table_changes` stream whose table is outside the app's subset.
- [ ] **L3.5** Projection: the delivered row is the REST read projection for that table, from
  the same function REST uses (no second list of hidden fields). `collab_doc` fields are left
  out. Test on `users`: no password hash.
- [ ] **L3.6** Subscription filters: the REST `where` vocabulary, validated on subscribe
  (`invalid` for an unknown field), evaluated in Rust. **Parity tests**: for a matrix of filters
  × rows (with nulls), the Rust verdict equals "the REST read with this filter returns the row".
- [ ] **L3.7** `deliver_to` in `sc-live`, a pure function: (old, new, readable_old,
  readable_new, matches_old, matches_new) → `insert | update | enter | leave | delete | nothing`.
  Test it with the full truth table, including that `leave` and `delete` carry no row.
- [ ] **L3.8** Readability per subscriber: the role floor short-circuits; otherwise the
  ownership formula is evaluated reified, with Ⱶ/Ↄ prefetches batched once per element and
  verdicts computed once per distinct user. Fast path for `field === user.id`. Tests: the
  ownership tutorial's formulas (owner, shares via Ↄ, publisher via Ⱶ) × two users × each op.
  A row a user could never read produces no frame of any kind for that user.
- [ ] **L3.9** Counters for the hub: elements checked, delivered, suppressed by access,
  suppressed by filter, and evaluator time. Shown on the stream's row in the admin list.

### Phase 3: the client
- [ ] **L3.10** `live.<s>.subscribe({ where }, handlers)` typed with the table's row type and the
  REST client's filter type (one type, shared).
- [ ] **L3.11** `useLiveRows(client.<table>, { where, orderBy, select })`: fetch, subscribe with
  the same filter, apply the five ops while keeping order, refetch on `resync` and `lagged`, and
  refetch changed keys when `payload: key` or when `select` has joins. Unit-test the reducer in
  TypeScript (vitest, the `ui/admin` setup) without a server.

### Try it
1. Tables `boards` and `cards` (`board` → boards, `column` text, `position` float, `title`). An
   ownership formula on `cards`: readable if the user is a member of the card's board (an Ↄ over
   `board_members`).
2. A `table_changes` stream `cards_live` on `cards`, min role Member, exposed on a React kanban
   app that uses `useLiveRows(client.cards, { where: { board: id }, orderBy: "position" })`.
3. Alice and Bob both open board 1. Alice drags a card to "Done": it moves on Bob's screen.
   Alice moves a card to board 2, where Bob is not a member: it disappears from Bob's board 1
   and nothing about it reaches Bob's socket (check the frames in devtools).
4. Remove Bob from board 1's members: Bob's next update for that board is nothing, and his
   existing cards leave.

- [ ] **L3.12** Tutorial part 3 (a live kanban board) and its definition-of-done test, with a
  puppeteer check of two browser contexts. TECHNICAL_DESIGN §14.10 table changes and the
  after-commit rule; §10.2's note that `TableEvents` emits before a shared commit, and what live
  does about it.

---

## L4 — Client publishing and presence (B: who is here, C's cursors)

- [ ] **L4.1** Client `publish` on `internal` streams with `client_publish: on`, following §3
  rule 7: `min_role` on `single`, update access on `row`, never on `user`. The value is
  validated, and `source: {user, connection}` is stamped by the server (a client-sent `source`
  is ignored). The sender does not get its own element back unless it asks (`echo: true` on
  subscribe). `can_publish` goes in `ready`. Tests: a reader who may not update the row gets
  `unavailable` on publish, and a forged `source` never reaches another subscriber.
- [ ] **L4.2** Rate limits per connection for publish and presence, answered with
  `rate_limited` rather than a closed socket. Presence is coalesced to the latest state per
  50 ms.
- [ ] **L4.3** Presence on `internal` (`presence: on`) streams: per `(stream, topic)` membership
  per connection. `presence` snapshot after `ready`, then `presence_diff` (join, leave, state).
  A member is `{connection, user_id, …presence_fields, state}`, with `presence_fields` read from
  `users` at join time. A member leaves on unsubscribe, close or `revoked`. State is capped at
  2 KB. Test: a user who fails the topic check never appears in, and never receives, the
  member list.
- [ ] **L4.4** Client: `live.<s>.join(topic, handlers)` → `{ members, setState, publish, leave }`.
  React: `usePresence(accessor, topic)` and `usePublish(accessor, topic)`.
- [ ] **L4.5** Admin: the Observe screen shows current presence per topic.

### Try it
1. Add an internal stream `board_presence`: topics `row` of `boards`, presence on,
   `presence_fields: name`, `client_publish: on`, keys `dragging` (integer, optional).
2. In the kanban app, avatars show who else has the board open. While Alice drags a card,
   Bob sees it highlighted (`setState({ dragging: cardId })`).
3. Close Alice's tab: her avatar disappears from Bob's board within a second.

- [ ] **L4.6** Tutorial part 4 (presence on the kanban board) and its definition-of-done test.

---

## L5 — Collaborative documents (C)

- [ ] **L5.1** The `collab_doc` type in `sc-types`: stored as bytea / BLOB, read over REST as
  `null` or not at all (decide, and test it), and refused as a write target by REST,
  `insert_row`/`update_rows` and the admin row editor, with a sentence saying why. It cannot be
  used in filters, sorting, formulas or ownership formulas.
- [ ] **L5.2** Add `yrs`. A `DocRoom` per open `(stream, row)`: load the stored state (an empty
  doc if it is null), apply incoming updates, relay them to the other members, and close the room
  when the last member leaves. Test that a Yjs update made by the JS `yjs` package (a recorded
  fixture) applies in `yrs` and produces the same text. This checks the format compatibility we
  rely on.
- [ ] **L5.3** The `document` provider: `table`, `field` (must be `collab_doc`), `text_mirror`
  (optional `String` field), and `root` (the shared type's name, default `content`). Topic is
  `Row`. `doc_sync` on subscribe with `read_only` = !update access. A `doc_update` from a
  read-only member is refused with `unavailable` and not applied. Re-checks (L2.6) apply: a
  member who loses update access becomes read-only (`doc_sync` again with `read_only: true`),
  and one who loses read access is `revoked`.
- [ ] **L5.4** Persistence: debounced 2 s and when the room closes. In one transaction:
  `SELECT … FOR UPDATE`, merge the stored state with the room's state, write the field and the
  `text_mirror` through the row layer as a system write. Test: two rooms (simulating two
  processes) persisting interleaved edits lose neither, and the mirror equals the merged text.
- [ ] **L5.5** Limits: maximum document size (5 MB default, refusing further updates with a
  sentence), maximum update size, and update rate.
- [ ] **L5.6** `live.document(stream, key)` in code bodies: read the text, apply a text edit as a
  CRDT update (for an agent or a workflow that appends to a document). The only server-side
  writer.
- [ ] **L5.7** Client: `live.<doc>.open(key)` → `{ doc: Y.Doc, awareness, readOnly, status,
  close }`. The awareness is `y-protocols/awareness`, synced through presence frames, so
  existing editor bindings work unchanged. `yjs` and `y-protocols` are added to the React
  scaffold's `package.json` only when the app exposes a document stream. React:
  `useDocument(accessor, key)`.
- [ ] **L5.8** A worked example in the scaffold docs (`AGENTS.md` contract): TipTap with
  `@tiptap/extension-collaboration` and `collaboration-cursor`, and CodeMirror with
  `y-codemirror.next`, so the coding agent can write either.

### Try it
1. Table `notes` (`title`, `body` collab_doc, `body_text` string), ownership formula
   `owner === user.id || sharesↃnote.some(s => s.user === user.id)`, with `can_edit` on shares.
2. A `document` stream `note_body` on `notes.body`, mirror `body_text`, exposed on a React app
   with a TipTap editor.
3. Alice and Bob edit the same note: both see each other's text and named cursors as they type.
   Carol, shared read-only, sees the edits live and cannot type.
4. Close both tabs and reopen: the text is there. `GET /api/notes` shows it in `body_text`.

- [ ] **L5.9** Tutorial part 5 (a collaborative notes app) and its definition-of-done test,
  with a puppeteer check of two contexts typing into one document.

---

## L6 — The bus: more than one process

§16 plans `sc-bus`. Until now, everything above is process-local. This milestone makes it work
when two `feldspar serve` processes share one database.

- [ ] **L6.1** `sc-bus`: a `BusDriver` trait (`publish(subject, bytes)`, `subscribe(prefix)`) and
  the in-process driver. Move `LiveHub::publish`, table-change elements and document updates
  onto it, so the single-process path is the in-process driver and not a special case.
- [ ] **L6.2** Postgres LISTEN/NOTIFY driver: one dedicated listening connection with reconnect
  and backoff. A payload over ~7.5 KB goes into `_fd_bus_spill` (id, bytes, created_at), and the
  NOTIFY carries the id. A sweeper deletes spill rows older than 5 minutes. SQLite uses the
  in-process driver (one process anyway). The driver is a setting, `bus_driver`.
- [ ] **L6.3** Table changes across processes: the process that commits publishes to the bus,
  and every process (itself included) fans out to its own sockets. Elements carry the origin
  node id, so nothing is delivered twice.
- [ ] **L6.4** Presence across processes: each node announces its members per topic and sends a
  heartbeat. A node silent for 3 heartbeats has its members removed everywhere.
- [ ] **L6.5** Documents across processes: each process with an open room applies updates from
  the bus. Persistence is already merge-safe (L5.4). Test with two server instances on one
  database: edits made through each reach a client on the other.
- [ ] **L6.6** Session invalidation over the bus: `SessionStore::invalidate` carries a logout or a
  deleted user to every node, which closes that session's live sockets (L1.6). This also closes
  the 60 s window §7.2 describes.
- [ ] **L6.7** OPERATIONS.md: choosing a bus driver, Postgres connection count (+1 per process),
  what the spill table is, and that MQTT streams are still subscribed once per process.
- [ ] **L6.8** Tutorial part 6 (two processes behind one proxy, the kanban board still live)
  and its definition-of-done test, which starts two servers.

---

## Open questions

- Should the admin UI itself use the live socket (workflow run screens, model fit progress,
  which `fit_progress.rs` currently does its own way)? If yes, it would be a follow-up that
  replaces those routes.
- Should the MQTT provider offer `TopicSpec::Row`-style topics mapped from `source.topic`, so an
  app could subscribe to one sensor? For now a client filters on `source.topic` itself.
- A small per-topic history (the last *n*, not only the last 1) for `internal` streams, for a
  chat-like use? A chat that matters is a table plus `table_changes` (D), which suggests no.
