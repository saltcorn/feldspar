# Tutorial: Live updates — the server pushing to the page

A page usually asks and is answered. Some things should arrive without being asked for: a reading
from a sensor, a toast when a long job finishes, a kanban card that someone else just moved, the
text a colleague is typing into the same document. This tutorial builds those up one part at a
time, and every part rests on the same two ideas:

- **Everything that moves is a stream.** A page subscribes to streams its application exposes,
  and the server decides, per subscription and while it stays open, who may receive what.
- **A page holds one socket**, `{mount}/live`, however many streams it watches. The generated
  client opens it on first use, reconnects by itself, and types every element from the stream's
  declared element type.

| Part | What it builds | Milestone |
|---|---|---|
| 1 | The live socket, observing an external stream from React | L1 (this page) |
| 2 | Notifications and progress from a workflow | L2 (this page) |
| 3 | A live kanban board | L3 |
| 4 | Presence: who else has the board open | L4 (this page) |
| 5 | A collaborative notes app | L5 |
| 6 | Two processes behind one proxy | L6 |

Part 3 and parts 5–6 arrive with their milestones.

## Part 1 — The live socket

This part takes the MQTT `boiler` stream from [the streams tutorial](tutorial-streams.md) and
shows its latest temperature in the React app from [the React tutorial](tutorial-react-todo.md),
updating without a reload. It assumes both: a broker running on `localhost:1883`, the `boiler`
stream receiving readings, and the `todo` application served at `http://todo.localhost:3032`
with a **Member (40)** role and a user `member@example.com` who holds it. Create one more user,
`visitor@example.com`, with the **Public** role: someone signed in, but below Member.

### Step 1 — Open the stream to Members, and expose it

A stream with no minimum role is admin-only, because a flow nobody has thought about the access
of is not public. Open **Streams → boiler → Edit**, set **Minimum role to observe** to
**Member (40)**, and save. Nothing about the connection to the broker changes: the role is not
part of the flow.

Then open **Applications → todo**, tick `boiler` in the **Streams** picker, and save. The form
says what that does: a page subscribes to the stream over the application's live socket at
`/api/live`, and the generated client gets an accessor for it. Rebuild the application (or let
the next build do it) so `src/feldspar/` is regenerated.

### Step 2 — Show the latest reading

`src/feldspar/live-react.ts` is generated with the rest of the runtime, and since the app now
exposes a stream it exports `live`, one accessor per stream. Add a component:

```tsx
import { useState } from "react";
import { live, useStream } from "../feldspar/live-react";
import { useT } from "../feldspar/i18n";

export function BoilerReading() {
  const { t } = useT();
  const [reading, setReading] = useState<number | null>(null);
  const { status, error } = useStream(live.boiler, (envelope) => {
    setReading(envelope.value.temperature);
  });
  if (error) return <p>{t("The boiler is not available to you.")}</p>;
  return (
    <p title={status}>
      {reading === null ? t("Waiting for a reading…") : t("{temperature} °C", { temperature: reading })}
    </p>
  );
}
```

and put `<BoilerReading />` on a page. `envelope.value` is typed from the keys the stream
declares — `temperature: number`, `unit: string | null` — so `envelope.value.pressure` would not
compile. The hook subscribes when the component mounts, closes the subscription when it
unmounts, and returns the connection's `status` (`connecting`, `open`, `reconnecting`) and the
server's refusal, if there was one.

Sign in to the app as `member@example.com` and open the page. The last readings the server has
seen arrive at once (the subscription's `ready` says how many of them are history), and then:

```bash
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": 64.5, "unit": "C"}'
```

The number changes without a reload.

### Step 3 — One socket

Open the browser's developer tools, **Network**, filter by **WS**, and reload. There is one
socket, `/api/live`, however many components subscribe and however many streams they watch. Click
it and the **Messages** tab shows the protocol, which is all JSON:

```
→ {"type":"subscribe","sub":"s1","stream":"boiler"}
← {"type":"ready","sub":"s1","stream":"boiler","element_type":{…},"replayed":3,"can_publish":false,…}
← {"type":"element","sub":"s1","envelope":{"stream":"boiler","value":{"temperature":64.5,"unit":"C"},…}}
```

`sub` is the client's own name for a subscription; every frame about it carries it back. The
envelope is the one the Observe screen shows and a trigger's `only_if` reads.

### Step 4 — Who may watch, and for how long

The socket authenticates once, with the application's own session, exactly as its API does. The
server then goes on checking.

**Sign out in another tab.** The page's socket closes with an `error` frame of `signed_out` at
once: a sign-out on this server closes that session's sockets immediately. (Were the session
ended some other way — by another server process, or by its expiry — the socket notices at its
next re-check, within a minute.) The client tries to reconnect and, now anonymous, is told the
stream is `unavailable`, so the component shows its refusal.

**Sign in as `visitor@example.com`.** The subscription gets `unavailable` — the same answer as a
stream the app does not expose, or one that does not exist. Which of the three it was is not
something the server tells a page.

**Lower a Member's role while they watch.** Change `member@example.com`'s role to Public in
**Users**. Within two minutes their subscription gets `revoked`, and stops: the socket re-checks
once a minute, and the server's copy of who a session belongs to is itself up to a minute old.
Raising the stream's own minimum role has the same effect on everyone below it.

**And from another site.** The socket accepts a connection only from a page on its own host. With
**Share sign-in between applications** turned on, a browser sends the session cookie to every
application on the base domain — so a page on another application could otherwise open this
one's socket as the signed-in user. It is refused before the socket opens, with a 403.

### What part 1 is

- A page subscribes with `useStream(live.<stream>, onElement)`, or outside React with
  `api.live.<stream>.subscribe({ element, ready, lagged, resync, error })`.
- A stream's **Minimum role to observe** is the floor for every subscription; unknown, unexposed
  and forbidden are one answer, `unavailable`.
- Access is re-checked while the socket is open: sign-outs close it, lowered roles revoke.
- Delivery is at-most-once and never blocks the stream. A page that falls behind is told
  (`lagged`); a page that reconnects is told to `resync`, because what arrived while it was away
  is not replayed beyond the last few elements.

The design is [TECHNICAL_DESIGN.md §14.10](TECHNICAL_DESIGN.md), and the definition of done for
this part is `crates/sc-server/tests/tutorial_live.rs`.

## Part 2 — Notifications and progress from a workflow

Part 1 watched something outside Saltcorn. This part has Saltcorn itself say things to the page:
a toast when a long report starts, a progress bar while it runs, and a toast when it is ready —
**to the person who clicked the button, and nobody else**. It builds on part 1's `todo`
application, its Member role and `member@example.com`. Add one more Member,
`colleague@example.com`, to be the person who must not see anything.

### Step 1 — An internal stream, one topic per user

A stream whose elements Saltcorn publishes itself is an **internal** stream. Open
**Streams → New**, name it `job_status`, pick the **Internal** provider, and fill in:

- **Declared keys of an element** — what every element carries, exactly as an MQTT stream's JSON
  payload declares it:

  ```json
  [
    { "name": "message", "type": "text", "required": true },
    { "name": "progress", "type": "float" }
  ]
  ```

- **Topics** — `user`. Every element is published *to a user*, and a page that subscribes is put
  on its own user's topic without naming it. Nobody can subscribe to somebody else's, and a page
  nobody is signed in to cannot subscribe at all.
- **Keep the latest element per topic for a new subscriber** — on, kept for an hour. A page
  opened (or reloaded) in the middle of a job is handed that user's latest element first, so a
  progress bar starts where the job is rather than at nothing.
- **Minimum role to observe** — Member (40).

The other two topic kinds are `single` (one flow for everybody above the minimum role — an
announcement) and `row` (a topic per row of a table, which a subscriber may join only if they may
read that row — part 3 and part 4 use it).

Then open **Applications → todo** and tick `job_status` in **Streams**.

### Step 2 — A workflow that reports as it goes

Open **Triggers → New**: name `long_report`, event **None** (it is run by a page), body
**Workflow**, minimum role Member. Draw:

| Step | Kind | What it does |
|---|---|---|
| `started` | Action: **publish** | stream `job_status`, topic blank, value `message` = `"Report started"` |
| `count` | Set | `steps` = `[1, 2, 3, 4, 5]` |
| `each` | For each | over `context.steps` as `step`, body `wait`; then `done` |
| `wait` | Wait | until `2000` (two seconds of pretend work) |
| `progress` | Action: **run_js_code** | the code below |
| `done` | Action: **publish** | value `message` = `"Report ready"` |

`publish` puts one element on an internal stream. Its **topic** is a formula, and for a stream
with a topic per user it defaults to `user.id` — the event's user. A workflow run carries the user
who started it, so a `publish` step ten seconds and five steps after the click still reaches the
person who clicked. Its **value** is one formula per declared key.

The `progress` step shows the other way to publish, from code — `live` sits beside `db`, `fetch`
and `trigger` in a code body (and in a Python body, as `live.publish`):

```js
live.publish("job_status", user.id, {
  message: "Working",
  progress: context.step / context.steps.length,
});
```

A publish never waits: it hands the element to the stream and returns. Everything is checked —
the stream exists and is internal, the topic is a user's id, the value has a `message` that is
text — and a refusal is an error the step fails with, naming what was wrong.

Then tick `long_report` in the application's **Triggers**, and rebuild the application. The
generated client now has `api.runLongReport()` and `live.job_status`, typed from the keys.

### Step 3 — Toasts and a progress bar

```tsx
import { useRef, useState } from "react";
import { api } from "../feldspar/hooks";
import { live, useLatest, useStream } from "../feldspar/live-react";
import { useT } from "../feldspar/i18n";

export function LongReport() {
  const { t } = useT();
  const [toasts, setToasts] = useState<string[]>([]);
  // How many of the elements arriving are history: what the bar shows, not news.
  const replaying = useRef(0);
  useStream(
    live.job_status,
    (envelope) => {
      if (replaying.current > 0) {
        replaying.current -= 1;
        return;
      }
      // An element with progress moves the bar; one without is a toast.
      const { message, progress } = envelope.value;
      if (progress === null) setToasts((shown) => [...shown, message]);
    },
    { ready: (info) => { replaying.current = info.replayed; } },
  );
  const latest = useLatest(live.job_status);
  const progress = latest?.value.progress ?? null;
  return (
    <section>
      <button onClick={() => void api.runLongReport({})}>{t("Run the report")}</button>
      {progress !== null && <progress value={progress} max={1} />}
      <ul>
        {toasts.map((toast, index) => (
          <li key={index}>{toast}</li>
        ))}
      </ul>
    </section>
  );
}
```

`useLatest` is the latest element of a stream, or `undefined` before the first: what a progress
bar wants — it shows while the latest element carries progress, and goes once the report is
ready. `useStream` is called for every element, which is what a toast wants. Both share the
page's one socket. The `ready` handler says how many of the elements that follow are history — on
this stream, the retained one — so a reloaded page does not toast it again.

### Step 4 — Two people at once

Sign in as `member@example.com` in one browser and as `colleague@example.com` in another, and
open the page in both. Click **Run the report** as the member: "Report started" appears, the bar
fills in five steps, and "Report ready" appears. The colleague's page shows nothing, and its
socket received nothing — in the developer tools, the member's frames carry
`"topic":"<the member's id>"`, and the colleague's socket has no `element` frames at all.

A subscriber does not name a topic on a `user` stream, so there is nothing to forge: the server
puts the subscription on the signed-in user's own id. Naming one is refused as a malformed
frame, and a page nobody is signed in to is told the stream is `unavailable`.

### Step 5 — Reload halfway

Click again and reload the member's page while the bar is moving. The subscription's `ready` says
`"replayed":1`, the bar is back at the job's current progress at once, and it carries on from
there. That is **Keep the latest element per topic**: the stream keeps each user's latest element
for an hour (and at most ten thousand users' worth), and hands it to a new subscriber first. With
it off, a new subscriber is handed nothing — right for a stream of toasts, where a minute-old
message is not news.

What is *not* kept is everything else: a toast published while the member had no page open is
gone. A notification that has to wait for its reader belongs in a table, and part 3's row changes
are how a page hears about it.

### What part 2 is

- An **internal** stream is published to by Saltcorn: the `publish` action (in a trigger or a
  workflow step) and `live.publish(stream, topic, value)` in a JavaScript or Python code body.
  Server-side publishers are trusted configuration, as an `insert_row` is, and may publish to any
  topic; every publish is checked for shape and refused, by name, when it is wrong.
- **Topics** split a stream: `single`, a topic per `user` (each subscriber gets exactly their own),
  or per `row` of a table (each subscriber must be able to read the row, and is revoked the
  moment they no longer can).
- A `user` publish with nobody to tell — a periodic trigger has no user — is an error naming the
  trigger, not a toast dropped on the floor.
- **Keep the latest element** hands a new subscriber the latest element on its topic, for a time.

The definition of done for this part is `part_two_notifications_and_progress` in
`crates/sc-server/tests/tutorial_live.rs`.

## Part 4 — Presence: who else has the board open

A board that moves by itself is half of working together; the other half is seeing who else is
looking at it. This part puts the faces of everybody who has a board open above it, and
highlights a card while somebody drags it — on every other screen, as it happens. It is
**presence**: a topic's members, each with a little state of its own, that the server keeps and
tells the others about.

It builds on part 1's `todo` application, its Member role, `member@example.com` and part 2's
`colleague@example.com`, and on a board of part 3's shape. What this part needs of it
is two tables — `boards` (`id`, `title`) and `board_members` (`id`, `board` → `boards`, `member`
a user's id) — with the read rule part 3 gives `boards`, on the **Ownership** tab:

```
board_membersↃboard.some(m => m.member === user.id)
```

Put `member@example.com` and `colleague@example.com` on board 1 in `board_members`. Add one more
Member, `outsider@example.com`, who is on no board. And give people names to be shown by: in
**Tables → users**, add a field `name` (String), and fill it in for the three of them.

### Step 1 — A stream for who is on a board

Open **Streams → New**, name it `board_presence`, pick **Internal**, and fill in:

- **Declared keys of an element**:

  ```json
  [{ "name": "dragging", "type": "int" }]
  ```

- **Topics** — `row`, of the table `boards`. A page joins one board's topic, and only if it may
  read that board — the same read the REST API makes, so `outsider@example.com` is refused.
- **Members can see who else is here** — on. **User fields shown to other members** — `["name"]`.
  A member is shown to the others as their user id, the fields listed here and nothing else.
- **Pages may publish** — on. A page may publish on a board's topic only if it may **update**
  that board; here every member may, because the read rule is the board's only rule.
- **Minimum role to observe** — Member (40).

Then tick `board_presence` in the application's **Streams** and rebuild. The client now has
`live.board_presence`, typed with `join` and `publish` because the settings allow them.

### Step 2 — Faces, and the card being dragged

```tsx
import { live, usePresence } from "../feldspar/live-react";
import { useT } from "../feldspar/i18n";

type Card = { id: number; title: string };

export function BoardPresence({ board, cards }: { board: number; cards: Card[] }) {
  const { t } = useT();
  const { members, me, setState } = usePresence(live.board_presence, board);
  // Somebody else's drag: the card they are holding, and who.
  const dragged = new Map<number, string>();
  for (const member of members) {
    const state = member.state as { dragging?: number | null } | null;
    if (member.connection !== me && typeof state?.dragging === "number") {
      dragged.set(state.dragging, String(member["name"] ?? t("Somebody")));
    }
  }
  return (
    <section>
      <ul className="faces">
        {members.map((member) => (
          <li key={member.connection} title={String(member["name"] ?? "")}>
            {String(member["name"] ?? "?").slice(0, 1)}
            {member.connection === me ? ` (${t("you")})` : ""}
          </li>
        ))}
      </ul>
      <ul>
        {cards.map((card) => (
          <li
            key={card.id}
            draggable
            onDragStart={() => setState({ dragging: card.id })}
            onDragEnd={() => setState({ dragging: null })}
            className={dragged.has(card.id) ? "being-dragged" : undefined}
          >
            {card.title}
            {dragged.has(card.id) && ` — ${dragged.get(card.id)}`}
          </li>
        ))}
      </ul>
    </section>
  );
}
```

`usePresence(live.board_presence, board)` joins board `board`'s topic while the component is
mounted and leaves when it unmounts. `members` is everybody with that board open, this page
included; `me` is which of them is this page. A member is `{ connection, user_id, state }` and
the fields the stream shows — here `name`. There is one member per open page, not per user: the
same person with two tabs open is two faces, and each tab can tell which one is itself.

`setState` is this page's state, shown to everybody else as its member's `state`. Call it as
often as a drag fires: the client sends at most one state every 60 ms (the latest), and the
server holds the ones that come quicker than every 50 ms and applies the latest. A state is at
most 2 KB of JSON.

### Step 3 — Two people on one board

Sign in as `member@example.com` in one browser and `colleague@example.com` in another, and open
board 1 in both. Each sees two faces. Start dragging a card as the member: on the colleague's
screen it is highlighted with the member's name, and it stops being when the member lets go. In
the developer tools the colleague's socket shows a `presence` frame after `ready` — the list —
and then `presence_diff` frames: `joined`, `updated` with the member's `{"dragging":3}`, `left`.

Now sign in as `outsider@example.com` and open board 1. The subscription is refused with the same
`unavailable` a board that does not exist gets, and the outsider is in nobody's list: presence is
shown only to members who passed the board's read check, and someone refused is never a member.

### Step 4 — Publishing on a board

A state is what a member *is* doing. An element is something that *happened*. On a stream whose
settings let pages publish, a page can send one to everybody else on the topic:

```ts
const { publish } = usePresence(live.board_presence, board);
publish({ dragging: cardId });
```

(or `usePublish(live.board_presence, board)` for a component that does not join). The server
checks the value against the declared keys, and stamps the envelope's `source` with who sent it
— `{"user": …, "connection": …}` — whatever the page put there, so nobody can publish in someone
else's name. The sender does not get its own element back unless it subscribed with
`echo: true`. A page that may read the board but not update it — part 3's read-only share, say —
gets `can_publish: false` in `ready`, and a publish is answered `unavailable`. Publishing is
limited to 20 a second per page; past that a publish is answered `rate_limited` and the socket
stays open.

### Step 5 — Closing the tab

Close the colleague's tab. On the member's screen the colleague's face is gone within a second:
the socket closed, so the member left the room, and the others were sent `left`. The same happens
when a page unmounts the component, and when access is revoked — take the colleague off the
board in `board_members` and their face goes from everybody's list at once.

The admin sees all of it too: **Streams → board_presence → Observe** has a **Present now** card
listing, per board, who has it open and their state.

### What part 4 is

- **Presence** is a stream setting. A topic's members are the pages subscribed to it — one per
  connection — each shown as `{connection, user_id, …presence_fields, state}`, and a page is sent
  the list after `ready` (`presence`) and every change after that (`presence_diff`).
- **Only members see members.** Joining is subscribing, which the topic's read check decides;
  unsubscribing, a revoke and a closed socket leave.
- **Pages may publish** when the stream's settings say so: on a `single` stream under its minimum
  role, on a `row` topic only with **update** access to the row, on a `user` topic never. The
  server stamps `source`, and the sender is left out unless it asks for `echo`.
- **Limits**: 20 publishes and 20 presence updates a second per page (`rate_limited` past them),
  presence states coalesced every 50 ms, and at most 2 KB each (`too_large`).

The definition of done for this part is `part_four_presence_on_the_board` in
`crates/sc-server/tests/tutorial_live.rs`.
