import { describe, expect, it } from "vitest";

import { LASSO_MAX_POINTS, lassoPolygon, thin, type ScreenPoint } from "./lasso";

describe("a lasso", () => {
  it("thins the pointer's path and closes it as a polygon in degrees", () => {
    const path: ScreenPoint[] = [
      [0, 0],
      [1, 0],
      [10, 0],
      [10, 10],
      [10, 11],
      [0, 10],
    ];
    expect(thin(path)).toEqual([
      [0, 0],
      [10, 0],
      [10, 10],
      [0, 10],
    ]);
    const polygon = lassoPolygon(path, ([x, y]) => [x / 100, 51 - y / 100]);
    expect(polygon).toEqual({
      type: "Polygon",
      coordinates: [
        [
          [0, 51],
          [0.1, 51],
          [0.1, 50.9],
          [0, 50.9],
          [0, 51],
        ],
      ],
    });
  });

  it("is a click, not a lasso, with fewer than three points, and keeps at most so many", () => {
    expect(lassoPolygon([[0, 0], [1, 1]], ([x, y]) => [x, y])).toBeNull();
    const long: ScreenPoint[] = Array.from({ length: 1000 }, (_, i) => [i * 5, (i % 2) * 5]);
    const kept = thin(long);
    expect(kept).toHaveLength(LASSO_MAX_POINTS);
    expect(kept[0]).toEqual(long[0]);
    expect(kept[kept.length - 1]).toEqual(long[long.length - 1]);
  });
});
