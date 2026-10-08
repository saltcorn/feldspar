// Split view (analytics TODO A4.1): two things side by side, each a workspace,
// the Dataset editor or the model editor, with a divider between them that
// moves.
//
// The address records both (`router.ts`'s `Layout`), so each side's screens
// must move **their own** side: a link or a button on the right opens on the
// right. Screens ask `usePane()` for that instead of `navigate`/`routeHash`:
// `href` is the whole address with this side changed, `go` goes there. Off
// the split, the pane is the whole screen and they mean what `navigate` and
// `routeHash` mean.
//
// Each side is mounted in the same place whether the screen is split or not,
// so splitting, or closing the other side, does not rebuild what is open.

import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent,
  type ReactNode,
} from "react";
import Button from "react-bootstrap/Button";

import { announce, concerns, listen, type Change } from "./changes";
import { useT } from "./i18n";
import { layoutHash, parseLayout, withSide, type Layout, type Route, type Side } from "./router";

/** What a screen can do to the side it is on. */
export type Pane = {
  side: Side;
  /** Whether the screen is split. */
  split: boolean;
  /** The address with this side showing `route`. */
  href: (route: Route) => string;
  /** Show `route` on this side. */
  go: (route: Route) => void;
  /** Show `route` on this side without a new history entry. */
  replace: (route: Route) => void;
  /** Close this side; the other becomes the whole screen. */
  close: () => void;
  /** Show `route` on the other side, splitting the screen if it is not. */
  beside: (route: Route) => void;
};

/** The pane for `side`, reading the layout from the address when it acts so
 * that it never acts on a stale one. */
export function paneOf(side: Side, split: boolean): Pane {
  const current = () => parseLayout(window.location.hash);
  const href = (route: Route) => layoutHash(withSide(current(), side, route));
  return {
    side,
    split,
    href,
    go: (route) => {
      window.location.hash = href(route);
    },
    replace: (route) => {
      window.history.replaceState(null, "", href(route));
    },
    close: () => {
      window.location.hash = layoutHash(withSide(current(), side, null));
    },
    beside: (route) => {
      window.location.hash = layoutHash(withSide(current(), side === "main" ? "side" : "main", route));
    },
  };
}

const PaneContext = createContext<Pane>(paneOf("main", false));

/** The side this screen is on. */
export function usePane(): Pane {
  return useContext(PaneContext);
}

export function PaneProvider({ side, split, children }: { side: Side; split: boolean; children: ReactNode }) {
  const pane = useMemo(() => paneOf(side, split), [side, split]);
  return <PaneContext.Provider value={pane}>{children}</PaneContext.Provider>;
}

/** Announce a change made on this side. */
export function useAnnounce(): (kind: Change["kind"], id: string) => void {
  const { side } = usePane();
  return useMemo(() => (kind: Change["kind"], id: string) => announce({ kind, id, from: side }), [side]);
}

/** Act on the changes of these kinds made on the other side. The handler may
 * change from render to render; the latest is called. */
export function useChanges(kinds: Change["kind"][], handler: (change: Change) => void): void {
  const { side } = usePane();
  const latest = useRef(handler);
  latest.current = handler;
  const key = kinds.join(",");
  useEffect(
    () =>
      listen((change) => {
        if (concerns(change, side, key.split(",") as Change["kind"][])) latest.current(change);
      }),
    [side, key],
  );
}

// --- the divider -------------------------------------------------------------

/** Where the divider starts: the left side's share of the width. */
export const DEFAULT_RATIO = 0.5;
/** The narrowest either side may be made, as a share. */
export const MIN_RATIO = 0.2;
/** How far an arrow key moves the divider. */
const KEY_STEP = 0.05;
/** Where the divider is remembered — this viewer's convenience, not state. */
const RATIO_KEY = "feldspar.analytics.split";

/** A share kept between the narrowest each side may be. */
export function clampRatio(ratio: number): number {
  if (!Number.isFinite(ratio)) return DEFAULT_RATIO;
  return Math.min(1 - MIN_RATIO, Math.max(MIN_RATIO, ratio));
}

function storedRatio(): number {
  try {
    const raw = window.localStorage.getItem(RATIO_KEY);
    return raw === null ? DEFAULT_RATIO : clampRatio(Number(raw));
  } catch {
    return DEFAULT_RATIO;
  }
}

function storeRatio(ratio: number): void {
  try {
    window.localStorage.setItem(RATIO_KEY, String(ratio));
  } catch {
    // A private window: the divider starts in the middle next time.
  }
}

/**
 * The screen: the main side, and when split, the divider and the other side.
 * `render` draws a route; each side gets its own pane.
 */
export function SplitView({ layout, render }: { layout: Layout; render: (route: Route) => ReactNode }) {
  const { t } = useT();
  const [ratio, setRatio] = useState(storedRatio);
  const box = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);
  const split = layout.side !== null;

  const move = (next: number) => {
    const r = clampRatio(next);
    setRatio(r);
    storeRatio(r);
  };
  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    dragging.current = true;
    e.currentTarget.setPointerCapture(e.pointerId);
    e.preventDefault();
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const rect = box.current?.getBoundingClientRect();
    if (!dragging.current || !rect || rect.width === 0) return;
    move((e.clientX - rect.left) / rect.width);
  };
  const onPointerUp = (e: PointerEvent<HTMLDivElement>) => {
    dragging.current = false;
    e.currentTarget.releasePointerCapture(e.pointerId);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "ArrowLeft") move(ratio - KEY_STEP);
    else if (e.key === "ArrowRight") move(ratio + KEY_STEP);
    else if (e.key === "Home") move(MIN_RATIO);
    else if (e.key === "End") move(1 - MIN_RATIO);
    else return;
    e.preventDefault();
  };

  return (
    <div className={split ? "an-split split" : "an-split"} ref={box}>
      <div className="an-pane" style={split ? { flexBasis: `${ratio * 100}%` } : undefined}>
        <PaneProvider side="main" split={split}>
          {split && <PaneBar />}
          <div className="an-pane-body">{render(layout.main)}</div>
        </PaneProvider>
      </div>
      {layout.side && (
        <>
          <div
            className="an-divider"
            role="separator"
            aria-orientation="vertical"
            aria-label={t("Move the divider")}
            aria-valuemin={MIN_RATIO * 100}
            aria-valuemax={(1 - MIN_RATIO) * 100}
            aria-valuenow={Math.round(ratio * 100)}
            tabIndex={0}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onKeyDown={onKeyDown}
            onDoubleClick={() => move(DEFAULT_RATIO)}
          />
          <div className="an-pane" style={{ flexBasis: `${(1 - ratio) * 100}%` }}>
            <PaneProvider side="side" split>
              <PaneBar />
              <div className="an-pane-body">{render(layout.side)}</div>
            </PaneProvider>
          </div>
        </>
      )}
    </div>
  );
}

/** A side's own bar: its front page, and closing it. */
function PaneBar() {
  const { t } = useT();
  const pane = usePane();
  return (
    <div className="an-pane-bar">
      <a href={pane.href({ name: "home" })} className="small text-secondary">
        {t("Front page")}
      </a>
      <Button
        size="sm"
        variant="link"
        className="ms-auto p-0 text-secondary"
        aria-label={pane.side === "main" ? t("Close the left side") : t("Close the right side")}
        title={t("Close this side")}
        onClick={pane.close}
      >
        ×
      </Button>
    </div>
  );
}
