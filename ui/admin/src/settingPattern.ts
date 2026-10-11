import { SECRET_SENTINEL, type FieldSpec } from "./settings";

/** What is wrong with `value` under `field`'s pattern (v1's `regexp`), or null
 * when nothing is: the same rule the server applies on save, shown as the
 * admin types. An empty value, a secret's sentinel and a pattern this browser
 * cannot compile are not judged here; the server has the last word. */
export function patternProblem(field: FieldSpec, value: string): string | null {
  if (!field.pattern || value === "" || value === SECRET_SENTINEL) return null;
  let re: RegExp;
  try {
    re = new RegExp(`^(?:${field.pattern})$`);
  } catch {
    return null;
  }
  if (re.test(value)) return null;
  return field.pattern_error || `Does not match the pattern ${field.pattern}.`;
}
