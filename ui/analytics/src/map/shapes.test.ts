import { describe, expect, it } from "vitest";

import { SHAPES, SHAPE_PIXELS, SHAPE_RADIUS, shapeAt, shapeImage, shapeImageName, signedDistance } from "./shapes";

/** The alpha of the image's pixel nearest a point in units of the radius. */
function alphaAt(image: ReturnType<typeof shapeImage>, u: number, v: number): number {
  const x = Math.floor(SHAPE_PIXELS / 2 + u * SHAPE_RADIUS);
  const y = Math.floor(SHAPE_PIXELS / 2 + v * SHAPE_RADIUS);
  return image.data[(y * SHAPE_PIXELS + x) * 4 + 3];
}

describe("shapes", () => {
  it("cycles through the shapes by a value's place", () => {
    expect(shapeAt(0)).toBe("circle");
    expect(shapeAt(1)).toBe("square");
    expect(shapeAt(SHAPES.length)).toBe("circle");
    expect(shapeImageName("star")).toBe("fd-shape-star");
  });

  it("measures the distance to each shape's edge, negative inside", () => {
    expect(signedDistance("circle", [0, 0])).toBeCloseTo(-1);
    expect(signedDistance("circle", [2, 0])).toBeCloseTo(1);
    expect(signedDistance("square", [0, 0])).toBeCloseTo(-0.886);
    expect(signedDistance("diamond", [1, 1])).toBeCloseTo(Math.SQRT1_2);
    // Between a star's points is outside it; its centre is inside.
    expect(signedDistance("star", [0, 0])).toBeLessThan(0);
    expect(signedDistance("star", [0, 0.9])).toBeGreaterThan(0);
    // A cross's corner is outside it, its arm inside.
    expect(signedDistance("cross", [0.8, 0.8])).toBeGreaterThan(0);
    expect(signedDistance("cross", [0.8, 0])).toBeLessThan(0);
  });

  it("draws an SDF image: opaque inside, the edge at 0.75, clear outside", () => {
    for (const shape of SHAPES) {
      const image = shapeImage(shape);
      expect(image.width).toBe(SHAPE_PIXELS);
      expect(image.data.length).toBe(SHAPE_PIXELS * SHAPE_PIXELS * 4);
      expect(alphaAt(image, 0.05, 0.05), shape).toBeGreaterThan(191);
      expect(alphaAt(image, -1.5, -1.5), shape).toBeLessThan(191);
      // Colour comes from `icon-color`: only the alpha is drawn.
      expect(image.data[0]).toBe(0);
    }
    // A square's corner is inside it and outside the circle.
    expect(alphaAt(shapeImage("square"), 0.8, 0.8)).toBeGreaterThan(191);
    expect(alphaAt(shapeImage("circle"), 0.8, 0.8)).toBeLessThan(191);
  });
});
