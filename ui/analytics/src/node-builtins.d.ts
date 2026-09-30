// The two Node built-ins a **test** in this bundle reaches for, declared here
// because this bundle has no `@types/node`.
//
// `i18n.format.test.ts` reads `crates/sc-i18n/fixtures/format.json` off disk,
// which is the whole point of that test: the corpus is shared with a Rust test
// and a copy of it in this tree is a copy that drifts. vitest runs in Node and
// has these at runtime; `tsc` needs to be told they exist, and the alternative
// — pulling `@types/node` into an SPA that never runs in Node — would put a
// hundred Node globals in scope of every screen.
//
// Two functions, with the signatures these two call sites use. Nothing else
// belongs here: if a third thing needs Node, that is the moment to reconsider
// rather than to add a line.

declare module "node:fs" {
  export function readFileSync(path: string, encoding: "utf8"): string;
}

declare module "node:url" {
  export function fileURLToPath(url: URL | string): string;
}
