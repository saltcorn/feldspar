import { describe, expect, it } from "vitest";

import { StateSaver, type Clock } from "./saver";

/** A clock the test advances by hand. */
function manualClock() {
  let now = 0;
  let next = 1;
  const timers = new Map<number, { at: number; fn: () => void }>();
  const clock: Clock = {
    set: (fn, ms) => {
      const id = next++;
      timers.set(id, { at: now + ms, fn });
      return id;
    },
    clear: (handle) => {
      timers.delete(handle as number);
    },
  };
  const advance = (ms: number) => {
    now += ms;
    for (const [id, timer] of [...timers]) {
      if (timer.at <= now) {
        timers.delete(id);
        timer.fn();
      }
    }
  };
  return { clock, advance };
}

describe("saving a workspace's state as it changes", () => {
  it("saves once, the last state, after the changes stop", async () => {
    const { clock, advance } = manualClock();
    const saved: number[] = [];
    const saver = new StateSaver<number>(async (s) => void saved.push(s), 500, undefined, clock);
    saver.update(1);
    advance(300);
    saver.update(2);
    advance(300);
    expect(saved).toEqual([]);
    expect(saver.status).toBe("pending");
    advance(300);
    await Promise.resolve();
    await Promise.resolve();
    expect(saved).toEqual([2]);
    expect(saver.status).toBe("saved");
  });

  it("saves what is pending at once when the workspace is left", async () => {
    const { clock } = manualClock();
    const saved: string[] = [];
    const saver = new StateSaver<string>(async (s) => void saved.push(s), 500, undefined, clock);
    saver.update("a");
    await saver.flush();
    expect(saved).toEqual(["a"]);
    // Nothing waiting: nothing sent.
    await saver.flush();
    expect(saved).toEqual(["a"]);
  });

  it("keeps a state whose save failed, and sends it with the next flush", async () => {
    const { clock } = manualClock();
    const statuses: string[] = [];
    let fail = true;
    const saved: string[] = [];
    const saver = new StateSaver<string>(
      async (s) => {
        if (fail) throw new Error("offline");
        saved.push(s);
      },
      500,
      (status) => statuses.push(status),
      clock,
    );
    saver.update("a");
    await saver.flush();
    expect(saver.status).toBe("failed");
    fail = false;
    await saver.flush();
    expect(saved).toEqual(["a"]);
    expect(statuses).toEqual(["pending", "saving", "failed", "saving", "saved"]);
  });
});
