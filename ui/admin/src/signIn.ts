// The password sign-in's steps, apart from the screen that shows them, so that
// every way it can end is an outcome the screen renders, never a button left
// disabled with nothing said.
//
// A sign-in is two requests: `login`, which starts the session, and the status
// read that follows it, which is how the SPA learns it is now signed in. Either
// can let the screen down without failing:
//
// - `login` can simply not answer (a server waiting on a database connection
//   that never comes free). Nothing about `fetch` ends that, so a timer does.
// - `login` can succeed and the status read still say nobody is signed in (the
//   session cookie was not kept). The sign-in screen then stays mounted, and
//   would stay "Signing in…" forever if it believed the first answer.

import type { AuthStatusResponse, LoginResponse } from "./client";

/** How long a sign-in waits for the server before saying it did not answer. */
export const SIGN_IN_TIMEOUT_MS = 30_000;

/** What became of a sign-in. */
export type SignInOutcome =
  | { kind: "signed_in" }
  | { kind: "second_factor"; challenge: { token: string; setup: boolean } }
  | { kind: "error"; message: string };

export const INVALID_CREDENTIALS = "Invalid email or password.";
export const SIGN_IN_FAILED = "Login failed. Please try again.";
export const SIGN_IN_TIMED_OUT =
  "The server did not answer the sign-in. Please try again.";
export const SESSION_NOT_KEPT =
  "The password was accepted, but the browser was not signed in. Please try again.";

class TimedOut extends Error {}

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new TimedOut()), ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err: unknown) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}

/**
 * Sign in: `login` starts the session, `refresh` reads the auth status the app
 * renders from (and returns it, or `null` if it could not be read — the app
 * shows its own error then). `errorStatus` reads the HTTP status out of a
 * failed call.
 */
export async function signIn(
  steps: {
    login: () => Promise<LoginResponse>;
    refresh: () => Promise<AuthStatusResponse | null>;
    errorStatus: (err: unknown) => number | null;
  },
  timeoutMs: number = SIGN_IN_TIMEOUT_MS,
): Promise<SignInOutcome> {
  let answer: LoginResponse;
  try {
    answer = await withTimeout(steps.login(), timeoutMs);
  } catch (err) {
    if (err instanceof TimedOut) return { kind: "error", message: SIGN_IN_TIMED_OUT };
    return {
      kind: "error",
      message: steps.errorStatus(err) === 401 ? INVALID_CREDENTIALS : SIGN_IN_FAILED,
    };
  }
  if (answer.second_factor) {
    return { kind: "second_factor", challenge: answer.second_factor };
  }
  let status: AuthStatusResponse | null;
  try {
    status = await withTimeout(steps.refresh(), timeoutMs);
  } catch {
    return { kind: "error", message: SIGN_IN_TIMED_OUT };
  }
  // `null` is a status read that failed, which the app already shows in place
  // of this screen; only a status that *answered* with nobody signed in is this
  // screen's to report.
  if (status && !status.current_user) {
    return { kind: "error", message: SESSION_NOT_KEPT };
  }
  return { kind: "signed_in" };
}
