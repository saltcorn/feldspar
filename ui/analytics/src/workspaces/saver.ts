// A workspace's state, saved as it changes (analytics TODO A1.15).
//
// Every workspace kind keeps its own state — which dataset is open, which
// operation is selected — and the frame saves it so the workspace reopens as
// it was left. Saving on every change would be a request per keystroke;
// saving on a button would lose what was not saved when a tab is closed. So a
// change schedules a save a moment later, a later change reschedules it, and
// leaving the workspace saves whatever is pending at once.
//
// A class with its clock handed in, so the rule is tested without a browser.

/** Where a saver stands, for the frame's "Saved" indicator. */
export type SaveStatus = "saved" | "pending" | "saving" | "failed";

/** The two timer functions a saver needs. */
export type Clock = {
  set: (fn: () => void, ms: number) => unknown;
  clear: (handle: unknown) => void;
};

const browserClock: Clock = {
  set: (fn, ms) => window.setTimeout(fn, ms),
  clear: (handle) => window.clearTimeout(handle as number),
};

export class StateSaver<S> {
  private waiting: { state: S } | null = null;
  private timer: unknown = null;
  private inFlight: Promise<void> | null = null;
  status: SaveStatus = "saved";

  constructor(
    private readonly save: (state: S) => Promise<void>,
    private readonly delayMs: number,
    private readonly onStatus: (status: SaveStatus, error?: unknown) => void = () => undefined,
    private readonly clock: Clock = browserClock,
  ) {}

  /** A new state: saved `delayMs` after the last one, unless another comes. */
  update(state: S): void {
    this.waiting = { state };
    this.setStatus("pending");
    if (this.timer !== null) this.clock.clear(this.timer);
    this.timer = this.clock.set(() => {
      this.timer = null;
      void this.flush();
    }, this.delayMs);
  }

  /** Save what is waiting now — what leaving the workspace does. */
  async flush(): Promise<void> {
    if (this.timer !== null) {
      this.clock.clear(this.timer);
      this.timer = null;
    }
    // One save at a time, in order: a slow save must not land after a newer one.
    if (this.inFlight) await this.inFlight;
    const next = this.waiting;
    if (!next) return;
    this.waiting = null;
    this.setStatus("saving");
    this.inFlight = this.save(next.state)
      .then(() => {
        this.setStatus(this.waiting ? "pending" : "saved");
      })
      .catch((error: unknown) => {
        // Kept, so the next change or flush tries again.
        if (!this.waiting) this.waiting = next;
        this.setStatus("failed", error);
      })
      .finally(() => {
        this.inFlight = null;
      });
    await this.inFlight;
  }

  private setStatus(status: SaveStatus, error?: unknown) {
    this.status = status;
    this.onStatus(status, error);
  }
}
