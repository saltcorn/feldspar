// The colour scheme: the admin UI's, followed rather than chosen again.
//
// Both bundles are served from one origin, so they share `localStorage`, and
// the admin UI keeps its choice under `saltcorn-admin-theme`. Reading and
// writing the same key is what makes the toggle here and the toggle there one
// setting: an admin who works in the dark in one works in the dark in both.

import { useCallback, useEffect, useState } from "react";

/** The two colour schemes Tabler ships. */
export type Theme = "light" | "dark";

/** The admin UI's key (`ui/admin/src/layout.tsx`). */
export const THEME_KEY = "saltcorn-admin-theme";

/** A stored preference, or what the OS reports when there is none. */
export function pickTheme(stored: string | null, prefersDark: boolean): Theme {
  if (stored === "light" || stored === "dark") return stored;
  return prefersDark ? "dark" : "light";
}

function initialTheme(): Theme {
  return pickTheme(
    window.localStorage.getItem(THEME_KEY),
    Boolean(window.matchMedia?.("(prefers-color-scheme: dark)").matches),
  );
}

/** The colour scheme, applied to the document and remembered with the admin UI's. */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(initialTheme);

  useEffect(() => {
    document.documentElement.setAttribute("data-bs-theme", theme);
    window.localStorage.setItem(THEME_KEY, theme);
  }, [theme]);

  // A change in the admin UI's tab, while this one is open.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === THEME_KEY && (e.newValue === "light" || e.newValue === "dark")) {
        setTheme(e.newValue);
      }
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  const toggle = useCallback(() => {
    setTheme((current) => (current === "dark" ? "light" : "dark"));
  }, []);

  return [theme, toggle];
}

/** The colour scheme in force, read off the document (which `useTheme` keeps
 * current), for the screens that draw — plots are drawn in the scheme's own
 * colours, not the stylesheet's. */
export function useDocumentTheme(): Theme {
  const read = (): Theme =>
    document.documentElement.getAttribute("data-bs-theme") === "dark" ? "dark" : "light";
  const [theme, setTheme] = useState<Theme>(read);
  useEffect(() => {
    const observer = new MutationObserver(() => setTheme(read()));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-bs-theme"] });
    return () => observer.disconnect();
  }, []);
  return theme;
}
