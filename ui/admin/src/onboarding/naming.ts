// How the onboarding names what it creates: a free name beside the ones there
// are, and a subdomain from an application's name. Pure, and apart from
// `effects.ts` so a test can reach them without the API client.

/** `base`, or `base 2`, `base 3`, … — the first not in `taken`. */
export function unusedName(base: string, taken: readonly string[], sep = " "): string {
  if (!taken.includes(base)) return base;
  for (let n = 2; ; n++) {
    const candidate = `${base}${sep}${n}`;
    if (!taken.includes(candidate)) return candidate;
  }
}

/** A subdomain label from an application's name: lower case, letters, digits
 * and single hyphens. */
export function subdomainFor(name: string): string {
  const label = name
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/, "");
  return label || "app";
}
