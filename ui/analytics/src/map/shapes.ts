// The symbols a map draws a point's Shape with (analytics TODO A5.7), as
// signed-distance-field images MapLibre colours itself.
//
// MapLibre draws a point either as a circle (one colour, any radius) or as an
// icon from the style's sprite. A shape per category needs icons, and the
// basemap's sprite has none for this, so the map adds its own: one image per
// shape, made here, pixel by pixel. They are SDF images — each pixel's alpha
// is its distance from the shape's edge, the edge at 0.75 — so `icon-color`
// colours them by the Color channel and `icon-size` sizes them by Size, as a
// plot's symbols are, without one image per colour and size.
//
// The shapes are a plot's (`SYMBOLS` in `plot/echarts.ts`) where MapLibre can
// draw them alike: circle, square, triangle, diamond, then a cross and a star.

/** The shapes, in the order a category's values take them. */
export const SHAPES = ["circle", "square", "triangle", "diamond", "cross", "star"] as const;

/** One of the shapes. */
export type ShapeName = (typeof SHAPES)[number];

/** The side of a shape's image, in device pixels. */
export const SHAPE_PIXELS = 32;
/** The device pixels per CSS pixel the image is drawn at. */
export const SHAPE_RATIO = 2;
/** The radius of a shape within its image, in device pixels: at `icon-size`
 * 1 a shape is `2 * SHAPE_RADIUS / SHAPE_RATIO` CSS pixels across. */
export const SHAPE_RADIUS = 10;
/** The CSS pixels across a shape is at `icon-size` 1. */
export const SHAPE_CSS = (2 * SHAPE_RADIUS) / SHAPE_RATIO;

/** How far from the edge, in device pixels, the distance field reaches
 * either way (tiny-sdf's `radius`), and where the edge sits (its `cutoff`). */
const SDF_RADIUS = 6;
const SDF_CUTOFF = 0.25;

/** The image name a shape is added to the map under. */
export function shapeImageName(shape: ShapeName): string {
  return `fd-shape-${shape}`;
}

/** The shape a category's value at `index` of its domain takes. */
export function shapeAt(index: number): ShapeName {
  return SHAPES[((index % SHAPES.length) + SHAPES.length) % SHAPES.length];
}

type Pt = [number, number];

/** A shape's outline in units of its radius, y down; none for the circle,
 * whose distance is exact. */
export function shapeOutline(shape: ShapeName): Pt[] | null {
  switch (shape) {
    case "circle":
      return null;
    case "square":
      // The area of the circle, near enough: a square of side √π.
      return [
        [-0.886, -0.886],
        [0.886, -0.886],
        [0.886, 0.886],
        [-0.886, 0.886],
      ];
    case "triangle":
      return [
        [0, -1],
        [0.95, 0.75],
        [-0.95, 0.75],
      ];
    case "diamond":
      return [
        [0, -1],
        [1, 0],
        [0, 1],
        [-1, 0],
      ];
    case "cross": {
      const a = 0.32;
      return [
        [-a, -1],
        [a, -1],
        [a, -a],
        [1, -a],
        [1, a],
        [a, a],
        [a, 1],
        [-a, 1],
        [-a, a],
        [-1, a],
        [-1, -a],
        [-a, -a],
      ];
    }
    case "star": {
      const out: Pt[] = [];
      for (let i = 0; i < 10; i++) {
        const r = i % 2 === 0 ? 1 : 0.45;
        const angle = -Math.PI / 2 + (i * Math.PI) / 5;
        out.push([r * Math.cos(angle), r * Math.sin(angle)]);
      }
      return out;
    }
  }
}

/** The distance from `p` to the segment `a`–`b`. */
function segmentDistance(p: Pt, a: Pt, b: Pt): number {
  const [dx, dy] = [b[0] - a[0], b[1] - a[1]];
  const len = dx * dx + dy * dy;
  const t = len === 0 ? 0 : Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len));
  return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
}

/** Whether `p` is inside the polygon `ring` (even–odd). */
function inside(p: Pt, ring: Pt[]): boolean {
  let hit = false;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const [xi, yi] = ring[i];
    const [xj, yj] = ring[j];
    if (yi > p[1] !== yj > p[1] && p[0] < ((xj - xi) * (p[1] - yi)) / (yj - yi) + xi) hit = !hit;
  }
  return hit;
}

/** The signed distance from `p` to a shape's edge, in units of its radius:
 * negative inside. */
export function signedDistance(shape: ShapeName, p: Pt): number {
  const ring = shapeOutline(shape);
  if (!ring) return Math.hypot(p[0], p[1]) - 1;
  let d = Infinity;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) d = Math.min(d, segmentDistance(p, ring[j], ring[i]));
  return inside(p, ring) ? -d : d;
}

/** A shape's SDF image: RGBA, `SHAPE_PIXELS` square, the distance in alpha
 * (255 deep inside, about 191 on the edge, 0 far outside). */
export function shapeImage(shape: ShapeName): { width: number; height: number; data: Uint8Array } {
  const side = SHAPE_PIXELS;
  const data = new Uint8Array(side * side * 4);
  const centre = side / 2;
  for (let y = 0; y < side; y++) {
    for (let x = 0; x < side; x++) {
      const p: Pt = [(x + 0.5 - centre) / SHAPE_RADIUS, (y + 0.5 - centre) / SHAPE_RADIUS];
      const pixels = signedDistance(shape, p) * SHAPE_RADIUS;
      const value = 255 * (1 - SDF_CUTOFF) - (255 * pixels) / SDF_RADIUS;
      data[(y * side + x) * 4 + 3] = Math.max(0, Math.min(255, Math.round(value)));
    }
  }
  return { width: side, height: side, data };
}
