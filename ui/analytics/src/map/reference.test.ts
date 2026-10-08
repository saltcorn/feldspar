import { describe, expect, it } from "vitest";

import { referenceTiles } from "./maplibre";
import { draftProblem, referenceOf } from "./ReferenceLayers";

const t = (text: string, args: Record<string, string> = {}) =>
  text.replace(/\{(\w+)\}/g, (m, k: string) => args[k] ?? m);
const draft = { name: "OSM", kind: "tiles" as const, url: "https://tile.openstreetmap.org/{z}/{x}/{y}.png", layers: "", attribution: "© OSM" };

describe("reference layers", () => {
  it("are tiles, a WMS or an ArcGIS service, each a raster tile template", () => {
    expect(referenceTiles({ id: "a", ...referenceOf(draft) } as never)).toBe("https://tile.openstreetmap.org/{z}/{x}/{y}.png");
    expect(referenceTiles({ id: "a", name: "A", kind: "arcgis", url: "https://g.example/rest/services/X/MapServer/" })).toBe(
      "https://g.example/rest/services/X/MapServer/tile/{z}/{y}/{x}",
    );
    const wms = referenceTiles({ id: "a", name: "W", kind: "wms", url: "https://w.example/wms?map=x", layers: "parcels,roads" });
    expect(wms.startsWith("https://w.example/wms?map=x&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=parcels%2Croads")).toBe(true);
    expect(wms).toContain("CRS=EPSG%3A3857");
    expect(wms.endsWith("&BBOX={bbox-epsg-3857}")).toBe(true);
  });

  it("says what a draft lacks", () => {
    expect(draftProblem(draft, t)).toBeNull();
    expect(draftProblem({ ...draft, name: " " }, t)).toContain("name");
    expect(draftProblem({ ...draft, url: "tile.example/{z}/{x}/{y}" }, t)).toContain("http");
    expect(draftProblem({ ...draft, url: "https://tile.example/tiles.png" }, t)).toContain("{z}");
    expect(draftProblem({ ...draft, kind: "wms", url: "https://w.example/wms" }, t)).toContain("layers");
    expect(referenceOf({ ...draft, kind: "wms", layers: " a ", attribution: "" })).toEqual({
      name: "OSM",
      kind: "wms",
      url: draft.url,
      layers: "a",
    });
  });
});
