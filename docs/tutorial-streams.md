# Tutorial: Streams — watching something that moves

Everything else Saltcorn holds is at rest: a table has rows, a file has bytes, a model has a
fit. A **stream** is the thing that moves — a temperature sensor publishing to a broker, a
market feed, a queue of jobs from another system. You create one from a **stream provider**,
fill in the settings it declares, and from then on its elements can be watched in the admin UI,
handed to a trigger, or read by an application over a WebSocket.

Saltcorn has one built-in stream provider, and it is the one GOALS names: **MQTT**. This page
walks the whole thing with a real broker on your own machine: a stream, the Observe screen, a
trigger that stores what arrives, and an application whose generated client reads it live.

> **The broker half is yours to run.** The tests that ship with Saltcorn cover the MQTT
> provider's settings, its element type and its payload decoder offline — no broker, no
> container, no network. What no test can assert is that a real broker's publishes arrive, so
> that part is a recipe you run by hand, and steps 1, 2 and 4 are it.

This assumes a server started with `--base-domain localhost`, as
[tutorial-ownership.md](tutorial-ownership.md) sets one up. Any installation will do; the
subdomains below just assume that one.

## Step 1 — A broker, in one command

[Mosquitto](https://mosquitto.org) is the small one, and it needs no configuration file to
listen on the loopback:

```
docker run --rm -it -p 1883:1883 eclipse-mosquitto:2 \
  mosquitto -c /mosquitto-no-auth.conf
```

That image ships two configurations; `/mosquitto-no-auth.conf` is the one that accepts anonymous
connections on `1883`, which is what you want on your own machine and nowhere else. If you would
rather not use Docker, `apt install mosquitto mosquitto-clients` gives you the same broker as a
service on `localhost:1883` and the two command-line tools below.

Leave it running in its own terminal. Everything after this is in another one.

## Step 2 — Publish something, and prove the broker works

`mosquitto_pub` is the publisher (`apt install mosquitto-clients`, or
`docker run --rm -it --network host eclipse-mosquitto:2 mosquitto_pub …`):

```
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": 31.2, "unit": "C"}'
```

Before pointing Saltcorn at it, watch it with `mosquitto_sub` in a third terminal, so that a
stream which shows nothing later is a question about Saltcorn rather than about the broker:

```
mosquitto_sub -h localhost -t 'house/+/temp' -v
```

Publish again and the subscriber prints the topic and the payload. That `house/+/temp` is a
**topic filter**, and it is the same string Saltcorn wants: `+` matches exactly one level,
`#` matches every level after it and can only be the last one.

## Step 3 — The stream

In the admin UI, the **Data Layer** section of the sidebar has a **Streams** entry, between
Triggers and Files — a stream is a source of events, so it lives beside the thing that listens
to them. Open it and press **New stream**.

Pick the `mqtt` provider. The rest of the form is *the provider's*: Saltcorn renders it from
what the provider declares, which is why it appears only once a provider is picked and why it
changes when you pick another one. Fill it in:

| Setting | Value |
| --- | --- |
| Name | `boiler` |
| Broker host | `localhost` |
| Port | `1883` |
| Connect over TLS | off |
| Client id | *(blank)* |
| Username / Password | *(blank)* |
| Topic filter | `house/+/temp` |
| Quality of service | `0` |
| Start a clean session | on |
| Payload | `json` |
| Declared keys | `temperature` (float, required), `unit` (text) |
| Minimum role to observe | *(blank — admin only, for now)* |
| Enabled | on |

Press **Save**. Saving *starts* it: the list shows `boiler` as `running` within a moment, with a
provider, a status, an element count and a "last element" column. There is no separate start
button, and disabling the row is what stops it.

Four things on that form are worth a sentence.

**The name is a reference, not a label.** It is what a trigger's channel names, what an
application's exposure names, and what a page subscribes to and its generated accessor is called
(`live.boiler`) — so it must be a legal identifier, and renaming the stream breaks those references visibly, exactly as renaming a
trigger does.

**The payload setting is what the element type is.** The same broker and the same filter are a
stream of objects with declared keys, a stream of text or a stream of bytes depending only on
this one picker — which is why the provider computes the element type from the configuration
rather than having one. With `json` you list the keys and their types, and that list is what the
Observe screen's columns, a trigger's `payload.value.temperature` and an application's generated
TypeScript are all built from. A `json` stream that declares no keys is refused when you save
it, because an element with no declared shape has nothing for any of those three to read.

**A blank client id is not a random one.** Saltcorn uses `feldspar-{the stream's name}`, and it
is stable on purpose: a client that invents a new id on every reconnect leaves the broker holding
a session per attempt, so a stream that flaps for a day becomes a broker nobody can administer.
Set one explicitly if your broker's access control keys off it.

**A clean session is the default**, and it means the broker does not hand over everything
published while the stream was down. That is deliberate: `received_at` on each element is when
*this server* saw it, so a resumed session delivers an hour of readings all stamped with the
moment the connection came back. What you do get from before you connected is a **retained**
message, because the broker sends those to every new subscriber — `mosquitto_pub -r` publishes
one, and it is a good way to see an element arrive the instant a stream starts.

## Step 4 — Watch it

Press **Observe** on the stream's row. The screen opens a WebSocket, says what the element type
is, replays the last hundred envelopes this server saw — labelled "since this server started",
because that is all there is: elements are not stored — and then tails live. Publish a few:

```
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": 31.2, "unit": "C"}'
mosquitto_pub -h localhost -t house/tank/temp   -m '{"temperature": 58.0, "unit": "C"}'
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": 30.9}'
```

All three arrive, as rows in a table whose columns are the keys you declared. The third declares
no `unit`, and because that key is not required the element carries `"unit": null` rather than
being refused — a declared shape is a floor on what an element has, not a ceiling, so an extra
key a publisher adds later is carried through too.

What the screen renders is one **envelope** per element, and the envelope is the wire contract
everything downstream reads:

```json
{ "stream": "boiler", "value": { "temperature": 31.2, "unit": "C" },
  "received_at": "2026-09-17T09:00:00Z",
  "source": { "topic": "house/boiler/temp", "qos": 0, "retain": false } }
```

`value` is the element. `source` is the provider's own metadata — MQTT's topic lives there
rather than beside `value`, because "which topic" is a fact about *this provider*, and a formula
that reads it has already accepted that it is talking to MQTT. `received_at` is when this server
saw it, never a claim about when it was produced.

What the screen draws follows the element type, which is why the payload picker mattered: a
`json` stream is a table of its declared keys, a `text` stream is a tail of lines, and a `binary`
stream is a hex head of each element rather than bytes pretending to be text.

**Pause** and **Clear** are yours, and client-side only: pausing stops the screen drawing, not
the stream. If your browser falls behind a fast stream the screen says so — a
`lagged: n dropped` notice rather than a silent gap, because nothing back-pressures a flow and
the honest answer is which elements you lost.

Now send something that is not what the stream declared:

```
mosquitto_pub -h localhost -t house/heartbeat/temp -m 'ok'
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": "warm"}'
```

Neither is delivered. Both are counted — the stream's `malformed` counter goes up, and that
counter is on the Streams list — and the server log says so **once a minute at most**, with the
number it held back:

```
feldspar: stream `boiler`: a payload on `house/heartbeat/temp` does not match the declared
element type and was not delivered: a json element's payload is not JSON: expected value at
line 1 column 1
```

This is the case that matters on a wildcard filter, and it is not even misbehaviour: `house/#`
matching four sensors and one heartbeat string is an ordinary thing to write. A publisher sending
the wrong shape at 50 Hz is one configuration mistake, not fifty log lines a second, and the
suppressed count in the next minute's line is what tells you which of the two you have.

## Step 5 — Store what arrives, with a trigger

The Observe screen's history is a ring in memory. What makes a flow **durable** is a trigger that
writes a row — a table you can query, back up and give away. That is the whole storage story for
streams, and it is deliberately the storage story you already know.

Make a table `readings` with `at` (`bigint`), `topic` (`string`) and `celsius` (`float`), then go
to **Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `store_boiler` |
| Event | `An element arrives on a stream` |
| Stream | `boiler` |
| **Only if** | `payload.value.temperature > 30` |
| Action | `insert_row` |
| Table (action setting) | `readings` |
| Field values | see below |

```json
{
  "at": "Date.now()",
  "topic": "payload.source.topic",
  "celsius": "payload.value.temperature"
}
```

Save it and publish two readings, one above thirty and one below:

```
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": 31.4, "unit": "C"}'
mosquitto_pub -h localhost -t house/boiler/temp -m '{"temperature": 18.0, "unit": "C"}'
```

**Tables → readings** has exactly one new row. Three things to name.

**The channel is a picker, not a box.** For a stream event the trigger form offers the streams
you have, and refuses a name that is not one of them — the same help a table event's table
picker gives, for the same reason.

**A stream event has no row.** The bindings a table trigger reads (`row`, `old`, the bare field
names) are not there, because there is no row: what there is, is `payload`, the envelope of step
4. So `only_if` is `payload.value.temperature > 30` and a field value is
`payload.source.topic`. A formula written against `row` fails here the way it already does on a
`startup` trigger.

**A trigger slower than its stream drops firings, and counts them.** Nothing may block a flow —
a broker does not wait for your trigger — so if elements arrive faster than the trigger runs, the
extra firings are dropped and the stream's `dropped_for_triggers` counter goes up on the Streams
list. That is the same judgement the periodic scheduler already makes about missed occurrences:
five queued copies of a job nobody read is worse than one late one, and an unbounded queue in
front of a trigger is a memory leak with a delay built into it. A stream that is dropping is a
thing you can see rather than a mystery you measure.

## Step 6 — Give it to an application

A stream is server-side configuration, so it is reachable from outside only because an
application said so — the rule its triggers already follow. Open your application, find the
**Streams** picker beside the Tables and Triggers ones, tick `boiler`, and save.

Two things follow from that tick.

**A page can subscribe to it** over the application's one **live socket**, `{mount}/live` —
`/api/live` for an app whose API is at `/api`. A page holds one such socket however many streams
it watches, and it authenticates exactly as the application's own API does, with the app's
session cookie. Each subscription is checked against the stream's **Minimum role to observe**,
which you left blank in step 3 — blank means admin, because a flow nobody has thought about the
access of is not public. If you want the app's members to watch it, set that to **Member (40)** on
the stream and save. A name the app does not expose, one that does not exist, and one above the
user's role all get the same answer, `unavailable`: the existence of a flow this application has
no business knowing about is not a fact worth handing out. The socket goes on checking while it
is open, so a user who signs out, or whose role is lowered, stops receiving.

**The generated client gets an accessor**, because that is where the declared element type earns
its keep. Rebuild the app and its client module (`src/feldspar/client.ts` in a React app) carries:

```ts
/** One element of the `boiler` stream, in its envelope. */
export type BoilerEnvelope = {
  stream: string;
  /** The topic it was published on, for a stream with topics. */
  topic?: string;
  value: { temperature: number; unit: string | null };
  /** When *this server* saw it — not a claim about when it was produced. */
  received_at: string;
  /** The provider's own metadata (MQTT's topic, QoS, retain), absent when it has none. */
  source?: unknown;
};

export interface LiveStreams {
  /** The `boiler` stream. */
  readonly boiler: LiveStream<BoilerEnvelope>;
}
```

and the client object has `live: LiveStreams`. The `value` type is the keys you declared, and a
key that is not required is `| null` rather than `?`-optional — deliberately, because a declared
key that is absent *is* null, so an element of a stream that declared two keys has two keys and
no consumer has to write `?? null` for a case the server already ruled out. A component reading
`envelope.value.unit` is checked by the same compiler that checks its table reads. In a React
app, the generated `useStream` hook ties a subscription to a component:

```tsx
import { live, useStream } from "./feldspar/live-react";

const [latest, setLatest] = useState<number | null>(null);
const { status } = useStream(live.boiler, (envelope) => setLatest(envelope.value.temperature));
```

Outside React, `api.live.boiler.subscribe({ element, ready, lagged, resync, error })` returns a
handle to `close()`. Note what it is **not**: a `Promise`. Subscribing is not a request, and the
caller wants the handle back now so it can close it when the screen goes away. `ready` arrives
first and says how many of the elements that follow are the replayed ring rather than new
arrivals; `lagged` is this client falling behind, never a silent gap; and `resync` follows a
reconnect, which the client makes by itself. [The live updates tutorial](tutorial-live.md) takes
this further.

## Step 7 — Stop the broker

In the broker's terminal, press `Ctrl-C`. The stream's status goes to `failed` with the reason,
and its attempt count starts climbing: Saltcorn retries with a doubling delay capped at a
minute, for ever. Start the broker again and the stream is `running` within a minute, without a
restart and without touching the row — and the subscription is re-sent, which is why the
reconnection is Saltcorn's job rather than the MQTT client's.

Editing the stream is the same story from the other side. Change its description and save: the
connection is left alone, because nothing about the flow changed. Change the topic filter and
save: it is stopped and started, because that is a different subscription. Neither needs a
restart, and neither disturbs any other stream.

## What to know before you point this at something real

- **One process, one subscription.** Two Saltcorn servers against one database both subscribe, so
  a trigger on the stream fires twice. That is a real limitation of this milestone, not a bug to
  find later. MQTT's own shared subscriptions are the escape hatch: a filter of
  `$share/feldspar/house/+/temp` asks the broker to give each element to exactly one member of
  the group, and Saltcorn accepts that filter as it stands.
- **Elements are not stored.** A stream is a flow, and nothing keeps its elements: the Observe
  screen's history is a small in-memory ring, labelled "since this server started". What makes a
  stream durable is step 5 — a trigger that writes a row.
- **TLS is `use_tls` plus the right port**, conventionally `8883`. Saltcorn verifies the broker
  against the machine's own root certificates, with the same rustls the HTTPS listener uses; it
  does not turn verification off, and there is no setting that does.
- **The password is a secret**, so it is redacted wherever the stream is read back and an edit
  that does not retype it keeps the stored one.
- **MQTT is not the only provider.** A module can supply one too — `plugins/rss` polls a feed —
  and it appears in the same picker with its own settings and its own element type. A module's
  provider is *polled* rather than pushed, because a module runs on a worker that has no channel
  back into the server; the interval is one of its settings. See
  [tutorial-modules.md](tutorial-modules.md).
