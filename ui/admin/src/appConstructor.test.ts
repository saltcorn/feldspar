// Plan & build as the bundled App Constructor module: whether it is installed,
// what installs it, where its screen is, and the request that creates a
// construction.

import { afterEach, describe, expect, it, vi } from "vitest";

import { constructPath, constructorState, createConstruction } from "./appConstructor";
import type { ListModulesResponse } from "./client";

const listing = (
  modules: Array<{ name: string; loaded: boolean }>,
  bundled: Array<{ id: string; name: string }>,
) => ({ modules, bundled }) as unknown as ListModulesResponse;

describe("constructorState", () => {
  it("is installed once the module is installed and loaded", () => {
    expect(
      constructorState(listing([{ name: "@feldspar/app-constructor", loaded: true }], [])),
    ).toEqual({ installed: true });
  });

  it("names the bundled catalog's entry to install when it is missing or did not load", () => {
    const bundled = [{ id: "app-constructor", name: "@feldspar/app-constructor" }];
    expect(constructorState(listing([], bundled))).toEqual({
      installed: false,
      bundled: "app-constructor",
    });
    expect(
      constructorState(listing([{ name: "@feldspar/app-constructor", loaded: false }], bundled)),
    ).toEqual({ installed: false, bundled: "app-constructor" });
    expect(constructorState(listing([], []))).toEqual({ installed: false, bundled: null });
  });
});

describe("Plan & build", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("is the module's screen on the application", () => {
    expect(constructPath("a 1")).toBe("/applications/a%201/m/app-constructor/construct");
  });

  it("creates a construction through the module's route, with the CSRF header", async () => {
    const sent: Array<[string, RequestInit]> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit) => {
        sent.push([url, init]);
        return new Response(JSON.stringify({ id: 1 }), { status: 201 });
      }),
    );
    vi.stubGlobal("document", { cookie: "__Host-sc_csrf=tok" });
    await createConstruction("a1", "main");
    const [url, init] = sent[0];
    expect(url).toBe("/_feldspar/routes/app-constructor/applications/a1/construction");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ provider: "main" });
    expect(new Headers(init.headers).get("x-csrf-token")).toBe("tok");
  });
});
