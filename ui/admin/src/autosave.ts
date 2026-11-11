// Saving a form as it is edited (TODO UX2.2).
//
// The application form has no Save button: each change is saved on its own,
// after a pause for typing and at once for a switch or a choice. What makes that
// safe is that saves never overlap and are never lost:
//
// - **One at a time.** A change made while a save is in flight waits for it,
//   and then everything changed meanwhile goes in one save, so the server never
//   sees two versions of the record race.
// - **Nothing twice.** A body equal to the last one saved is not sent, so
//   typing a letter and deleting it costs nothing.
// - **A refused save keeps the edits.** It is not retried in a loop: the next
//   change tries again, with whatever is then in the form, and until then the
//   state says why it was refused.
// - **Flushed on the way out.** Leaving the screen saves what is waiting.

/** Where the form's edits stand. */
export type SaveState =
  | { kind: "idle" }
  | { kind: "pending" }
  | { kind: "saving" }
  | { kind: "saved" }
  | { kind: "error"; message: string };

/** The timer functions, so a test can drive time. */
export type Timers = {
  setTimeout: (fn: () => void, ms: number) => unknown;
  clearTimeout: (handle: unknown) => void;
};

const realTimers: Timers = {
  setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** The pause after typing before a save. */
export const TYPING_DELAY_MS = 700;

export class Autosaver<B> {
  private lastSaved: string | null = null;
  private pending: { body: B; key: string } | null = null;
  private timer: unknown = null;
  private inFlight: Promise<void> | null = null;
  private inFlightKey: string | null = null;

  constructor(
    private readonly save: (body: B) => Promise<void>,
    private readonly onState: (state: SaveState) => void,
    private readonly errorText: (err: unknown) => string = String,
    private readonly timers: Timers = realTimers,
  ) {}

  /** The body as stored: what the form was loaded with. Not saved. */
  baseline(body: B): void {
    this.lastSaved = JSON.stringify(body);
    this.pending = null;
    this.clearTimer();
  }

  /** The form now holds `body`; save it after `delayMs` (0: as soon as any
   * save in flight is done). */
  change(body: B, delayMs: number): void {
    if (this.lastSaved === null) return; // not loaded yet
    const key = JSON.stringify(body);
    if (key === (this.pending?.key ?? this.inFlightKey ?? this.lastSaved)) return;
    this.clearTimer();
    if (key === this.lastSaved && !this.inFlight) {
      // Changed back to what is stored: nothing to save.
      this.pending = null;
      this.onState({ kind: "saved" });
      return;
    }
    this.pending = { body, key };
    this.onState({ kind: "pending" });
    this.timer = this.timers.setTimeout(() => {
      this.timer = null;
      void this.run();
    }, delayMs);
  }

  /** Save whatever is waiting now, and resolve once every save is done. */
  async flush(): Promise<void> {
    this.clearTimer();
    await this.run();
    while (this.inFlight) await this.inFlight;
  }

  private clearTimer(): void {
    if (this.timer !== null) {
      this.timers.clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private run(): Promise<void> {
    // Already saving: the waiting body follows when that is done.
    if (this.inFlight) return this.inFlight;
    const next = this.pending;
    if (!next) return Promise.resolve();
    this.pending = null;
    if (next.key === this.lastSaved) return Promise.resolve();
    this.onState({ kind: "saving" });
    this.inFlightKey = next.key;
    this.inFlight = this.save(next.body).then(
      () => {
        this.lastSaved = next.key;
        if (!this.pending) this.onState({ kind: "saved" });
      },
      (err: unknown) => {
        // What is in the form stays; the next change tries again with it.
        if (!this.pending) this.onState({ kind: "error", message: this.errorText(err) });
      },
    );
    return this.inFlight.finally(() => {
      this.inFlight = null;
      this.inFlightKey = null;
      if (this.pending && this.timer === null) void this.run();
    });
  }
}
