// The handful of Tabler icons the admin shell uses, as inline SVG components.
//
// Tabler's own templates inline every icon rather than loading a font or a
// sprite sheet, and that is what the strict CSP wants too: an icon is markup the
// React bundle already carries, so there is no second asset to serve and no
// external origin to allow. Only the icons that are actually used are copied
// here — adding one means copying its paths from https://tabler.io/icons.
//
// Every icon takes Tabler's own `icon` class plus a size class (`icon-1`/`icon-2`
// in the templates) and inherits colour from `currentColor`, so the same
// component works on a dark sidebar and a light page.

import type { ReactNode } from "react";

import logoUrl from "./vendor/saltcorn-logo.svg";

/** Props shared by every icon: an extra class, and a title for standalone use. */
type IconProps = {
  /** Appended to the base `icon` class — Tabler's `icon-1`, `icon-2`, `text-…`. */
  className?: string;
};

/** The common `<svg>` wrapper: Tabler's 24×24 stroked outline geometry. */
function Svg({ className, children }: IconProps & { children: ReactNode }) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="24"
      height="24"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className ? `icon ${className}` : "icon"}
      aria-hidden="true"
    >
      {children}
    </svg>
  );
}

export function IconTable(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3 5a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14z" />
      <path d="M3 10h18" />
      <path d="M10 3v18" />
    </Svg>
  );
}

export function IconApps(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 4m0 1a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1z" />
      <path d="M4 14m0 1a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1z" />
      <path d="M14 14m0 1a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1z" />
      <path d="M14 7l6 0" />
      <path d="M17 4l0 6" />
    </Svg>
  );
}

export function IconBolt(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M13 3l0 7l6 0l-8 11l0 -7l-6 0l8 -11" />
    </Svg>
  );
}

export function IconFolder(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 4h4l3 3h7a2 2 0 0 1 2 2v8a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-11a2 2 0 0 1 2 -2" />
    </Svg>
  );
}

export function IconFile(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M14 3v4a1 1 0 0 0 1 1h4" />
      <path d="M17 21h-10a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2h7l5 5v11a2 2 0 0 1 -2 2z" />
    </Svg>
  );
}

export function IconSearch(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M10 10m-7 0a7 7 0 1 0 14 0a7 7 0 1 0 -14 0" />
      <path d="M21 21l-6 -6" />
    </Svg>
  );
}

export function IconUsers(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M9 7m-4 0a4 4 0 1 0 8 0a4 4 0 1 0 -8 0" />
      <path d="M3 21v-2a4 4 0 0 1 4 -4h4a4 4 0 0 1 4 4v2" />
      <path d="M16 3.13a4 4 0 0 1 0 7.75" />
      <path d="M21 21v-2a4 4 0 0 0 -3 -3.85" />
    </Svg>
  );
}

export function IconSettings(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M10.325 4.317c.426 -1.756 2.924 -1.756 3.35 0a1.724 1.724 0 0 0 2.573 1.066c1.543 -.94 3.31 .826 2.37 2.37a1.724 1.724 0 0 0 1.065 2.572c1.756 .426 1.756 2.924 0 3.35a1.724 1.724 0 0 0 -1.066 2.573c.94 1.543 -.826 3.31 -2.37 2.37a1.724 1.724 0 0 0 -2.572 1.065c-.426 1.756 -2.924 1.756 -3.35 0a1.724 1.724 0 0 0 -2.573 -1.066c-1.543 .94 -3.31 -.826 -2.37 -2.37a1.724 1.724 0 0 0 -1.065 -2.572c-1.756 -.426 -1.756 -2.924 0 -3.35a1.724 1.724 0 0 0 1.066 -2.573c-.94 -1.543 .826 -3.31 2.37 -2.37c1 .608 2.296 .07 2.572 -1.065z" />
      <path d="M9 12a3 3 0 1 0 6 0a3 3 0 0 0 -6 0" />
    </Svg>
  );
}

export function IconShieldLock(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 3a12 12 0 0 0 8.5 3a12 12 0 0 1 -8.5 15a12 12 0 0 1 -8.5 -15a12 12 0 0 0 8.5 -3" />
      <path d="M12 11m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0" />
      <path d="M12 12l0 2.5" />
    </Svg>
  );
}

/** A robot's head — the agents section, of which LLM providers are the first
 * screen. */
export function IconRobot(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M6 8m0 2a2 2 0 0 1 2 -2h8a2 2 0 0 1 2 2v6a2 2 0 0 1 -2 2h-8a2 2 0 0 1 -2 -2z" />
      <path d="M12 2v4" />
      <path d="M9 12h.01" />
      <path d="M15 12h.01" />
      <path d="M9.5 16a3.5 3.5 0 0 0 5 0" />
      <path d="M3 12h3" />
      <path d="M18 12h3" />
    </Svg>
  );
}

export function IconSun(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 12m-4 0a4 4 0 1 0 8 0a4 4 0 1 0 -8 0" />
      <path d="M3 12h1m8 -9v1m8 8h1m-9 8v1m-6.4 -15.4l.7 .7m12.1 -.7l-.7 .7m0 11.4l.7 .7m-12.1 -.7l-.7 .7" />
    </Svg>
  );
}

export function IconMoon(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 3c.132 0 .263 0 .393 0a7.5 7.5 0 0 0 7.92 12.446a9 9 0 1 1 -8.313 -12.454z" />
    </Svg>
  );
}

export function IconLogout(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M14 8v-2a2 2 0 0 0 -2 -2h-7a2 2 0 0 0 -2 2v12a2 2 0 0 0 2 2h7a2 2 0 0 0 2 -2v-2" />
      <path d="M9 12h12l-3 -3" />
      <path d="M18 15l3 -3" />
    </Svg>
  );
}

export function IconPlus(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 5l0 14" />
      <path d="M5 12l14 0" />
    </Svg>
  );
}

export function IconChevronLeft(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M15 6l-6 6l6 6" />
    </Svg>
  );
}

export function IconChevronRight(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M9 6l6 6l-6 6" />
    </Svg>
  );
}

export function IconArrowLeft(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 12l14 0" />
      <path d="M5 12l6 6" />
      <path d="M5 12l6 -6" />
    </Svg>
  );
}

export function IconChevronDown(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M6 9l6 6l6 -6" />
    </Svg>
  );
}

/** Send, on the chat composer's button — the arrow every chat box sends with. */
export function IconArrowUp(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 5l0 14" />
      <path d="M18 11l-6 -6" />
      <path d="M6 11l6 -6" />
    </Svg>
  );
}

/** Stop, in the same place the send button was, while a turn runs. */
export function IconPlayerStop(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 5m0 2a2 2 0 0 1 2 -2h10a2 2 0 0 1 2 2v10a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2z" />
    </Svg>
  );
}

export function IconMessagePlus(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12.4 3a5.34 5.34 0 0 1 4.906 3.239a5.333 5.333 0 0 1 -1.195 10.6a4.26 4.26 0 0 1 -5.28 1.863l-2.831 2.29v-2.99h-.007a4.26 4.26 0 0 1 -2.65 -6.084a5.333 5.333 0 0 1 2.19 -8.865" />
      <path d="M15 11h-6" />
      <path d="M12 8v6" />
    </Svg>
  );
}

export function IconTrash(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 7l16 0" />
      <path d="M10 11l0 6" />
      <path d="M14 11l0 6" />
      <path d="M5 7l1 12a2 2 0 0 0 2 2h8a2 2 0 0 0 2 -2l1 -12" />
      <path d="M9 7v-3a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v3" />
    </Svg>
  );
}

/** Open a table's rows for editing — the "Edit" tile on the table-data card. */
export function IconPencil(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M7 7h-1a2 2 0 0 0 -2 2v9a2 2 0 0 0 2 2h9a2 2 0 0 0 2 -2v-1" />
      <path d="M20.385 6.585a2.1 2.1 0 0 0 -2.97 -2.97l-8.415 8.385v3h3l8.385 -8.415z" />
      <path d="M16 5l3 3" />
    </Svg>
  );
}

/** Take the table's rows away as a file. */
export function IconDownload(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2 -2v-2" />
      <path d="M7 11l5 5l5 -5" />
      <path d="M12 4l0 12" />
    </Svg>
  );
}

/** Put rows into the table from a file. */
export function IconUpload(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2 -2v-2" />
      <path d="M7 9l5 -5l5 5" />
      <path d="M12 4l0 12" />
    </Svg>
  );
}

/** The overflow menu's own handle: everything rarer than the tiles beside it. */
export function IconDots(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 12m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0" />
      <path d="M12 12m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0" />
      <path d="M19 12m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0" />
    </Svg>
  );
}

/** The Streams section: a broadcast, which is what a dataflow is — something
 * published from somewhere else that this installation happens to be listening
 * to. Beside Triggers in the sidebar, and the same family of idea. */
export function IconBroadcast(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M18.364 19.364a9 9 0 1 0 -12.728 0" />
      <path d="M15.536 16.536a5 5 0 1 0 -7.072 0" />
      <path d="M12 13m-1 0a1 1 0 1 0 2 0a1 1 0 1 0 -2 0" />
    </Svg>
  );
}

/** The Models section: a fitted curve over a histogram of the data under it. */
export function IconChartHistogram(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3 3v18h18" />
      <path d="M20 18v3" />
      <path d="M16 16v5" />
      <path d="M12 13v8" />
      <path d="M8 16v5" />
      <path d="M3 11c6 0 5 -5 9 -5s3 5 9 5" />
    </Svg>
  );
}

/** A tool call, in the transcript. */
export function IconTool(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M7 10h3v-3l-3.5 -3.5a6 6 0 0 1 8 8l6 6a2 2 0 0 1 -3 3l-6 -6a6 6 0 0 1 -8 -8l3.5 3.5" />
    </Svg>
  );
}

/** The agent's reasoning, above what it said. */
export function IconSparkles(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M16 18a2 2 0 0 1 2 2a2 2 0 0 1 2 -2a2 2 0 0 1 -2 -2a2 2 0 0 1 -2 2zm0 -12a2 2 0 0 1 2 2a2 2 0 0 1 2 -2a2 2 0 0 1 -2 -2a2 2 0 0 1 -2 2zm-7 12a6 6 0 0 1 6 -6a6 6 0 0 1 -6 -6a6 6 0 0 1 -6 6a6 6 0 0 1 6 6z" />
    </Svg>
  );
}

export function IconAlertTriangle(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 9v4" />
      <path d="M10.363 3.591l-8.106 13.534a1.914 1.914 0 0 0 1.636 2.871h16.214a1.914 1.914 0 0 0 1.636 -2.87l-8.106 -13.536a1.914 1.914 0 0 0 -3.274 0z" />
      <path d="M12 16h.01" />
    </Svg>
  );
}

/** Show/hide the chat's conversation rail. */
export function IconLayoutSidebar(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 4m0 2a2 2 0 0 1 2 -2h12a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2z" />
      <path d="M9 4l0 16" />
    </Svg>
  );
}

/** Pop the chat out of the page and into the corner: a frame with a smaller
 * frame in the bottom-right of it, which is the picture the overlay is. */
export function IconPictureInPicture(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M11 19h-6a2 2 0 0 1 -2 -2v-10a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v4" />
      <path d="M13 14a1 1 0 0 1 1 -1h6a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-6a1 1 0 0 1 -1 -1z" />
    </Svg>
  );
}

/** Minimize a popped-out chat to its title bar. */
export function IconMinus(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 12l14 0" />
    </Svg>
  );
}

/** Give a popped-out chat the screen. */
export function IconArrowsDiagonal(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M16 4l4 0l0 4" />
      <path d="M14 10l6 -6" />
      <path d="M8 20l-4 0l0 -4" />
      <path d="M10 14l-6 6" />
    </Svg>
  );
}

/** Give it back — the same button, in the same place, once it has the screen. */
export function IconArrowsDiagonalMinimize(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M18 10h-4v-4" />
      <path d="M20 4l-6 6" />
      <path d="M6 14h4v4" />
      <path d="M4 20l6 -6" />
    </Svg>
  );
}

/** Close. */
export function IconX(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M18 6l-12 12" />
      <path d="M6 6l12 12" />
    </Svg>
  );
}

export function IconFilter(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 4h16v2.172a2 2 0 0 1 -.586 1.414l-4.414 4.414v7l-6 2v-8.5l-4.48 -4.928a2 2 0 0 1 -.52 -1.345v-2.227z" />
    </Svg>
  );
}

export function IconLayoutColumns(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 4m0 2a2 2 0 0 1 2 -2h12a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2z" />
      <path d="M12 4l0 16" />
    </Svg>
  );
}

export function IconCode(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M7 8l-4 4l4 4" />
      <path d="M17 8l4 4l-4 4" />
      <path d="M14 4l-4 16" />
    </Svg>
  );
}

/** Tabler's `language`: the translations link, and the locale picker. */
export function IconLanguage(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M4 5h7" />
      <path d="M9 3v2c0 4.418 -2.239 8 -5 8" />
      <path d="M5 9c0 2.144 2.952 3.908 6.7 4" />
      <path d="M12 20l4 -9l4 9" />
      <path d="M19.1 18h-6.2" />
    </Svg>
  );
}

export function IconRefresh(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M20 11a8.1 8.1 0 0 0 -15.5 -2m-.5 -4v4h4" />
      <path d="M4 13a8.1 8.1 0 0 0 15.5 2m.5 4v-4h-4" />
    </Svg>
  );
}

export function IconExternalLink(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 6h-6a2 2 0 0 0 -2 2v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2 -2v-6" />
      <path d="M11 13l9 -9" />
      <path d="M15 4h5v5" />
    </Svg>
  );
}

export function IconHammer(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M11.414 10l-7.383 7.418a2.091 2.091 0 0 0 0 2.967a2.11 2.11 0 0 0 2.976 0l7.407 -7.385" />
      <path d="M18.121 15.293l2.586 -2.586a1 1 0 0 0 0 -1.414l-7.586 -7.586a1 1 0 0 0 -1.414 0l-2.586 2.586a1 1 0 0 0 0 1.414l7.586 7.586a1 1 0 0 0 1.414 0z" />
    </Svg>
  );
}

export function IconBooks(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 4m0 1a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v14a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1z" />
      <path d="M9 4m0 1a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v14a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1z" />
      <path d="M5 8h4" />
      <path d="M9 16h4" />
      <path d="M13.803 4.56l2.184 -.53c.562 -.135 1.133 .19 1.282 .732l3.695 13.418a1.02 1.02 0 0 1 -.634 1.219l-.133 .041l-2.184 .53c-.562 .135 -1.133 -.19 -1.282 -.732l-3.695 -13.418a1.02 1.02 0 0 1 .634 -1.219l.133 -.041z" />
      <path d="M14 9l4 -1" />
      <path d="M16 16l3.923 -.98" />
    </Svg>
  );
}

export function IconLayoutDashboard(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 4h4a1 1 0 0 1 1 1v6a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1v-6a1 1 0 0 1 1 -1" />
      <path d="M5 16h4a1 1 0 0 1 1 1v2a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1v-2a1 1 0 0 1 1 -1" />
      <path d="M15 12h4a1 1 0 0 1 1 1v6a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1v-6a1 1 0 0 1 1 -1" />
      <path d="M15 4h4a1 1 0 0 1 1 1v2a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1v-2a1 1 0 0 1 1 -1" />
    </Svg>
  );
}

/** The Saltcorn logo (`src/vendor/saltcorn-logo.svg`, the January 2023 mark).
 *
 * An `<img>` rather than inline SVG: it is brand art, not an icon — three fixed
 * colours that must not take `currentColor` — so there is nothing to gain from
 * inlining it, and a single imported file cannot drift from the one the rest of
 * the project ships. Vite inlines it into the bundle as a `data:` URI (it is
 * ~1 KB), which `img-src 'self' data:` permits.
 *
 * Tabler's `.navbar-brand-image` sizes it (2rem tall, width auto). Note that it
 * must **not** sit inside a `.navbar-brand-autodark`: that class exists to flip
 * a monochrome logo to white on a dark background, and it would flatten this
 * one to a white silhouette. */
export function SaltcornLogo({ className }: IconProps) {
  return (
    <img
      src={logoUrl}
      alt="Saltcorn"
      className={className ? `navbar-brand-image ${className}` : "navbar-brand-image"}
    />
  );
}

export function IconDeviceDesktop(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3 5m0 1a1 1 0 0 1 1 -1h16a1 1 0 0 1 1 1v10a1 1 0 0 1 -1 1h-16a1 1 0 0 1 -1 -1z" />
      <path d="M7 20l10 0" />
      <path d="M9 16l0 4" />
      <path d="M15 16l0 4" />
    </Svg>
  );
}

export function IconDeviceTablet(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 4m0 1a1 1 0 0 1 1 -1h12a1 1 0 0 1 1 1v14a1 1 0 0 1 -1 1h-12a1 1 0 0 1 -1 -1z" />
      <path d="M12 17l0 .01" />
    </Svg>
  );
}

export function IconDeviceMobile(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M6 5a2 2 0 0 1 2 -2h8a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-8a2 2 0 0 1 -2 -2z" />
      <path d="M11 4l2 0" />
      <path d="M12 17l0 .01" />
    </Svg>
  );
}

/** Pull — the arrow VS Code's status bar uses for commits to bring down. */
export function IconArrowDown(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 5l0 14" />
      <path d="M18 13l-6 6" />
      <path d="M6 13l6 6" />
    </Svg>
  );
}

/** Checklists: ticked lines. */
export function IconListCheck(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3.5 5.5l1.5 1.5l2.5 -2.5" />
      <path d="M3.5 11.5l1.5 1.5l2.5 -2.5" />
      <path d="M3.5 17.5l1.5 1.5l2.5 -2.5" />
      <path d="M11 6l9 0" />
      <path d="M11 12l9 0" />
      <path d="M11 18l9 0" />
    </Svg>
  );
}

/** Commit. */
export function IconCheck(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 12l5 5l10 -10" />
    </Svg>
  );
}

/** Discard a change — VS Code's curved "undo" arrow. */
export function IconArrowBackUp(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M9 14l-4 -4l4 -4" />
      <path d="M5 10h11a4 4 0 1 1 0 8h-1" />
    </Svg>
  );
}

/** The branch selector. */
export function IconGitBranch(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M7 18m-2 0a2 2 0 1 0 4 0a2 2 0 1 0 -4 0" />
      <path d="M7 6m-2 0a2 2 0 1 0 4 0a2 2 0 1 0 -4 0" />
      <path d="M17 6m-2 0a2 2 0 1 0 4 0a2 2 0 1 0 -4 0" />
      <path d="M7 8l0 8" />
      <path d="M9 18h6a2 2 0 0 0 2 -2v-5" />
      <path d="M14 14l3 -3l3 3" />
    </Svg>
  );
}

export function IconHelpCircle(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M12 12m-9 0a9 9 0 1 0 18 0a9 9 0 1 0 -18 0" />
      <path d="M12 16v.01" />
      <path d="M12 13a2 2 0 0 0 .914 -3.782a1.98 1.98 0 0 0 -2.414 .483" />
    </Svg>
  );
}
