// The base map a map is drawn over (analytics TODO A5.6): the style URLs of
// Settings → Maps, asked of the server once, and each style read once.
//
// The style is fetched here rather than handed to MapLibre as a URL for two
// reasons: a label layer needs the fonts the style's glyphs have (its own
// labels' `text-font`), and a base map that cannot be reached — no internet,
// a host the policy does not name — should leave the data on a plain
// background with a sentence, not an empty grey box.

import type { StyleSpecification } from "@maplibre/maplibre-gl-style-spec";

import { api } from "../api";
import { chartPalette } from "../plot/palette";

/** The style URLs, light and dark; `null` for no base map. */
export type BaseMapSettings = { style: string | null; style_dark: string | null };

let settings: Promise<BaseMapSettings> | null = null;

/** Settings → Maps, asked once per page. */
export function baseMapSettings(): Promise<BaseMapSettings> {
  settings ??= api
    .mapSettings()
    .then((s) => ({ style: s.style ?? null, style_dark: s.style_dark ?? null }))
    .catch(() => {
      settings = null;
      return { style: null, style_dark: null };
    });
  return settings;
}

/** The style URL a page in `theme` draws. */
export function styleUrl(s: BaseMapSettings, theme: "light" | "dark"): string | null {
  return theme === "dark" ? (s.style_dark ?? s.style) : s.style;
}

const styles = new Map<string, Promise<StyleSpecification | null>>();

/** The style at `url`, or `null` when it cannot be read. */
export function loadStyle(url: string): Promise<StyleSpecification | null> {
  let style = styles.get(url);
  if (!style) {
    style = fetch(url, { credentials: "omit" })
      .then((res) => (res.ok ? (res.json() as Promise<StyleSpecification>) : null))
      .then((json) => (json && typeof json === "object" && json.version === 8 ? json : null))
      .catch(() => null);
    style.then((s) => {
      // A failure is tried again next time: the network may be back.
      if (!s) styles.delete(url);
    });
    styles.set(url, style);
  }
  return style;
}

/** A style with nothing but the page's background: the map with no base map. */
export function blankStyle(theme: "light" | "dark"): StyleSpecification {
  return {
    version: 8,
    sources: {},
    layers: [{ id: "background", type: "background", paint: { "background-color": chartPalette(theme).surface } }],
  };
}
