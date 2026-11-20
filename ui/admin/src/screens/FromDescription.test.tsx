// "Application from a description" (TODO-app-constructor AC5.1): which LLM
// provider the dialog starts on.

import { describe, expect, it } from "vitest";

import type { ListLlmProvidersResponse } from "../client";
import { initialProvider } from "./FromDescription";

const provider = (name: string, is_default: boolean) =>
  ({ id: name, name, is_default }) as ListLlmProvidersResponse[number];

describe("initialProvider", () => {
  it("is the installation's default, else the first, else none", () => {
    expect(initialProvider([provider("spare", false), provider("main", true)])).toBe("main");
    expect(initialProvider([provider("spare", false), provider("other", false)])).toBe("spare");
    expect(initialProvider([])).toBe("");
  });
});
