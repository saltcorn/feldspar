import { describe, expect, it } from "vitest";

import type { AuthStatusResponse, LoginResponse } from "./client";
import {
  INVALID_CREDENTIALS,
  SESSION_NOT_KEPT,
  SIGN_IN_FAILED,
  SIGN_IN_TIMED_OUT,
  signIn,
} from "./signIn";

const user = { id: "u1", email: "ada@example.com", role: 1, email_verified: true };

function status(signedIn: boolean): AuthStatusResponse {
  return {
    any_user_exists: true,
    first_user_token_required: false,
    current_user: signedIn ? user : null,
    locales: { default: "en", enabled: ["en"], current: "en" },
    onboarding_pending: false,
  };
}

const errorStatus = (err: unknown) => {
  const match = err instanceof Error ? err.message.match(/failed: (\d+)/) : null;
  return match ? Number(match[1]) : null;
};

const never = <T>() => new Promise<T>(() => {});

describe("signIn", () => {
  it("is signed in when the status read afterwards sees the user", async () => {
    const outcome = await signIn({
      login: async () => user as LoginResponse,
      refresh: async () => status(true),
      errorStatus,
    });
    expect(outcome).toEqual({ kind: "signed_in" });
  });

  it("says so when the password was accepted but the session was not kept", async () => {
    // The screen stays mounted in this case; before, it stayed "Signing in…".
    const outcome = await signIn({
      login: async () => user as LoginResponse,
      refresh: async () => status(false),
      errorStatus,
    });
    expect(outcome).toEqual({ kind: "error", message: SESSION_NOT_KEPT });
  });

  it("gives up on a login that never answers", async () => {
    const outcome = await signIn(
      { login: never, refresh: async () => status(true), errorStatus },
      20,
    );
    expect(outcome).toEqual({ kind: "error", message: SIGN_IN_TIMED_OUT });
  });

  it("gives up on a status read that never answers", async () => {
    const outcome = await signIn(
      { login: async () => user as LoginResponse, refresh: never, errorStatus },
      20,
    );
    expect(outcome).toEqual({ kind: "error", message: SIGN_IN_TIMED_OUT });
  });

  it("tells a wrong password from any other failure", async () => {
    const wrong = await signIn({
      login: async () => {
        throw new Error("login failed: 401: invalid credentials");
      },
      refresh: async () => status(false),
      errorStatus,
    });
    expect(wrong).toEqual({ kind: "error", message: INVALID_CREDENTIALS });
    const broken = await signIn({
      login: async () => {
        throw new Error("login failed: 500: could not start session");
      },
      refresh: async () => status(false),
      errorStatus,
    });
    expect(broken).toEqual({ kind: "error", message: SIGN_IN_FAILED });
  });

  it("hands a two-factor challenge back without reading the status", async () => {
    let refreshed = false;
    const outcome = await signIn({
      login: async () =>
        ({ ...user, second_factor: { token: "t", setup: false } }) as LoginResponse,
      refresh: async () => {
        refreshed = true;
        return status(false);
      },
      errorStatus,
    });
    expect(outcome).toEqual({
      kind: "second_factor",
      challenge: { token: "t", setup: false },
    });
    expect(refreshed).toBe(false);
  });

  it("leaves a status read that failed to the app's own error screen", async () => {
    const outcome = await signIn({
      login: async () => user as LoginResponse,
      refresh: async () => null,
      errorStatus,
    });
    expect(outcome).toEqual({ kind: "signed_in" });
  });
});
