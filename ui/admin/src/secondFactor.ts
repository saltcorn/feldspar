// The pure half of two-factor authentication in the admin UI
// (`screens/SecondFactor.tsx`): what a typed code is sent as, and what a
// refusal is said as.

/** A code as typed, as it is sent: spaces taken out (an app shows `123 456`).
 * A recovery code keeps its dash, which the server ignores. */
export function normaliseCode(code: string): string {
  return code.replace(/\s+/g, "");
}

/** What to tell the person when a code was refused with `status`. */
export function codeError(
  status: number | null,
  t: (text: string) => string,
): string {
  switch (status) {
    case 401:
      return t("That code is not right, or this sign-in has expired. Try again.");
    case 429:
      return t("Too many wrong codes. Wait a few minutes and try again.");
    default:
      return t("Something went wrong. Please try again.");
  }
}
