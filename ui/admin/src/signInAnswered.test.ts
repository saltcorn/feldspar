// The generated client's answer to a sign-in (`helper.ts`, design §7.2): a
// two-factor challenge in a browser goes to the server's page for the code,
// unless the application said it answers the challenge itself. This is what
// lets an application scaffolded before two-factor authentication existed —
// whose `Login.tsx` knows nothing of it — sign such an account in after a
// rebuild.
//
// The helper keeps the opt-out in module state, so each case loads a fresh copy.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

type Helper = typeof import("./helper");

async function freshHelper(): Promise<Helper> {
  vi.resetModules();
  return import("./helper");
}

describe("signInAnswered", () => {
  let assigned: string[];

  beforeEach(() => {
    assigned = [];
    vi.stubGlobal("window", {
      location: {
        pathname: "/orders",
        search: "?page=2",
        hash: "",
        assign: (url: string) => assigned.push(url),
      },
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("passes a sign-in that started a session straight through", async () => {
    const { signInAnswered } = await freshHelper();
    const user = { id: "u", email: "a@example.com", role: 80, email_verified: true };
    await expect(signInAnswered("", user)).resolves.toBe(user);
    expect(assigned).toEqual([]);
  });

  it("sends a browser with a challenge to the server's page, coming back here", async () => {
    const { signInAnswered } = await freshHelper();
    const answer = { id: "u", second_factor: { token: "t", setup: false } };
    let settled = false;
    void signInAnswered("", answer).then(() => (settled = true));
    await Promise.resolve();
    expect(assigned).toEqual([
      "/_feldspar/second-factor/sign-in?return_to=%2Forders%3Fpage%3D2",
    ]);
    expect(settled).toBe(false);
  });

  it("leaves the challenge to an application that answers it itself", async () => {
    const { signInAnswered, handleSecondFactorInApp } = await freshHelper();
    handleSecondFactorInApp();
    const answer = { id: "u", second_factor: { token: "t", setup: true } };
    await expect(signInAnswered("", answer)).resolves.toBe(answer);
    expect(assigned).toEqual([]);
  });
});

describe("signInAnswered outside a browser", () => {
  it("returns the challenge to a native app's code", async () => {
    const { signInAnswered } = await freshHelper();
    const answer = { id: "u", second_factor: { token: "t", setup: false } };
    await expect(signInAnswered("", answer)).resolves.toBe(answer);
  });
});
