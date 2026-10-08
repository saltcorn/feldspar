// A lasso drawn on a map (analytics TODO A5.10): the pointer's path on the
// screen made a polygon in degrees, which `selectFeatures` selects the features
// it touches by (`Geo.intersects`, on the server).
//
// The path is thinned first — a pointer reports a position every few pixels,
// and a polygon of hundreds of vertices only makes the condition longer — and
// closed. Fewer than three distinct points is not a lasso but a click.

/** A position on the map's canvas, in CSS pixels. */
export type ScreenPoint = [number, number];

/** How far apart, in pixels, two kept points of a lasso are at least. */
export const LASSO_STEP_PX = 4;
/** The most vertices a lasso keeps. */
export const LASSO_MAX_POINTS = 200;

/** The path with points closer than `step` to the last kept one dropped, and
 * at most `max` points (evenly picked). */
export function thin(points: ScreenPoint[], step = LASSO_STEP_PX, max = LASSO_MAX_POINTS): ScreenPoint[] {
  const out: ScreenPoint[] = [];
  for (const p of points) {
    const last = out[out.length - 1];
    if (!last || Math.hypot(p[0] - last[0], p[1] - last[1]) >= step) out.push(p);
  }
  if (out.length <= max) return out;
  return Array.from({ length: max }, (_, i) => out[Math.round((i * (out.length - 1)) / (max - 1))]);
}

/** The lasso as a closed GeoJSON polygon, each point turned into degrees by
 * `unproject`; `null` for a path with too few points to enclose anything. */
export function lassoPolygon(
  points: ScreenPoint[],
  unproject: (p: ScreenPoint) => [number, number],
): GeoJSON.Polygon | null {
  const kept = thin(points);
  if (kept.length < 3) return null;
  const ring = kept.map((p) => {
    const [lng, lat] = unproject(p);
    return [round(lng), round(lat)];
  });
  ring.push([...ring[0]]);
  return { type: "Polygon", coordinates: [ring] };
}

/** Degrees to seven places: about a centimetre. */
function round(v: number): number {
  return Math.round(v * 1e7) / 1e7;
}
