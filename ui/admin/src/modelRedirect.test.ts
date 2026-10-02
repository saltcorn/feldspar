import { describe, expect, it } from "vitest";

import { modelRedirect } from "./modelRedirect";

describe("the retired model screens", () => {
  it("send every old link to the Analytics UI's model editor", () => {
    expect(modelRedirect("/models")).toBe("/analytics/#/");
    expect(modelRedirect("/models/new")).toBe("/analytics/#/models/new");
    expect(modelRedirect("/models/new?dataset=d1")).toBe("/analytics/#/models/new?dataset=d1");
    expect(modelRedirect("/models/0b6f%20x")).toBe("/analytics/#/models/0b6f%20x");
    expect(modelRedirect("/model-instances/i9")).toBe("/analytics/#/model-instances/i9");
  });

  it("leaves every other route alone", () => {
    expect(modelRedirect("/tables")).toBeNull();
    expect(modelRedirect("/agents/models")).toBeNull();
    expect(modelRedirect("/models/a/b")).toBeNull();
  });
});
