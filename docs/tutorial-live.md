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
| 2 | Notifications and progress from a workflow | L2 |
| 3 | A live kanban board | L3 |
| 4 | Presence: who else has the board open | L4 |
| 5 | A collaborative notes app | L5 |
| 6 | Two processes behind one proxy | L6 |

Parts 2–6 arrive with their milestones.

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
