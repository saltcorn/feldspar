// The screen as the admin mounts it (TODO-app-constructor AC13.5): `mount`
// with a host whose `fetch` and `json` stand in for the module's routes. What
// it loads, what its buttons ask, how it moves between its paths, and where
// its links point.

import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Construction } from "./construction";
import type { Host } from "./host";
import { mount } from "./main";

(
  globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
).IS_REACT_ACT_ENVIRONMENT = true;

function construction(extra: Partial<Construction> = {}): Construction {
  return {
    id: 1,
    application: "a1",
    status: "stopped",
    specification: "A shop.",
    planning: false,
    plan_only: false,
    designing: false,
    asking: false,
    questions: [],
    mockups: [],
    feedback: [],
    errors: [],
    phases: [
      {
        id: 4,
        title: "Data model",
        description: "The tables.",
        requirements: [{ text: "books", priority: "must" }],
        status: "failed",
        tasks: [
          {
            id: 40,
            kind: "data_model",
            title: "Tables",
            brief: "Make the tables.",
            depends_on: [],
            status: "failed",
            runs: ["run-1"],
            summary: "the build is red",
            created: [],
          },
        ],
      },
    ],
    ...extra,
  };
}

/** A host answering `GET …/construction` with `loaded` (a status, or the
 * construction), recording every request. */
function fakeHost(loaded: Construction | number, path = "") {
  const requests: { path: string; method: string; body: unknown }[] = [];
  const record = (p: string, init?: RequestInit) =>
    requests.push({
      path: p,
      method: init?.method ?? "GET",
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
    });
  const host: Host = {
    app: {
      id: "a1",
      name: "Book shop",
      subdomain: "shop",
      description: "",
      framework: { name: "react" },
    },
    path,
    href: (p) => `#/applications/a1/m/app-constructor/construct${p}`,
    navigate: vi.fn(),
    routes: "/_feldspar/routes/app-constructor",
    fetch: vi.fn(async (p: string, init?: RequestInit) => {
      record(p, init);
      return typeof loaded === "number"
        ? new Response(JSON.stringify({ error: "none" }), { status: loaded })
        : new Response(JSON.stringify(loaded), { status: 200 });
    }),
    json: vi.fn(async (p: string, init?: RequestInit) => {
      record(p, init);
      if (p.endsWith("/stop"))
        return construction({ status: "stopped", error: "stopped by you" });
      if (p.endsWith("/retry")) throw new Error("The construction is running");
      return typeof loaded === "number" ? construction() : loaded;
    }),
  };
  return { host, requests };
}

let element: HTMLDivElement;
let mounted: ReturnType<typeof mount> | null = null;

beforeEach(() => {
  element = document.createElement("div");
  document.body.appendChild(element);
});

afterEach(() => {
  act(() => mounted?.unmount());
  mounted = null;
  element.remove();
});

/** Lets the loads and the renders they cause finish. */
async function settle() {
  await act(async () => {
    for (let i = 0; i < 5; i++) await Promise.resolve();
  });
}

async function show(host: Host) {
  await act(async () => {
    mounted = mount(element, host);
  });
  await settle();
}

function button(text: string): HTMLButtonElement {
  const found = [...element.querySelectorAll("button")].find(
    (b) => b.textContent?.trim() === text
  );
  if (!found)
    throw new Error(
      `no button "${text}" in ${element.innerHTML.slice(0, 2000)}`
    );
  return found;
}

describe("the mounted screen", () => {
  it("loads the application's construction from the module's route", async () => {
    const { host, requests } = fakeHost(construction());
    await show(host);
    expect(requests[0]).toEqual({
      path: "/applications/a1/construction",
      method: "GET",
      body: undefined,
    });
    expect(element.textContent).toContain("Book shop");
    expect(element.textContent).toContain("Plan & build");
    // A build under way opens on its phases.
    expect(element.querySelector('input[value="Data model"]')).not.toBeNull();
    // Its styles are added to the page once.
    expect(
      document.querySelectorAll("#feldspar-app-constructor-styles")
    ).toHaveLength(1);
  });

  it("opens on an empty specification when the application has no construction", async () => {
    const { host } = fakeHost(404);
    await show(host);
    const spec = element.querySelector("textarea") as HTMLTextAreaElement;
    expect(spec.value).toBe("");
    expect(element.textContent).toContain("A good specification says");
  });

  it("says why the load failed, in the route's words", async () => {
    const { host } = fakeHost(403);
    await show(host);
    expect(element.querySelector(".alert-danger")?.textContent).toContain(
      "none"
    );
  });

  it("posts a button's request and shows the construction it answers", async () => {
    const { host, requests } = fakeHost(construction({ status: "running" }));
    await show(host);
    await act(async () => button("Stop").click());
    await settle();
    expect(requests[requests.length - 1]).toEqual({
      path: "/applications/a1/construction/stop",
      method: "POST",
      body: {},
    });
    expect(element.textContent).toContain("Stopped");
  });

  it("shows a route's refusal", async () => {
    const { host } = fakeHost(construction(), "/phases/4");
    await show(host);
    await act(async () => button("Retry").click());
    await settle();
    expect(element.querySelector(".alert-danger")?.textContent).toContain(
      "The construction is running"
    );
  });

  it("shows one phase's page for its path, with links made by the host", async () => {
    const { host } = fakeHost(construction(), "/phases/4");
    await show(host);
    expect(element.textContent).toContain("Phase 1 of 1");
    expect(element.textContent).toContain("Tables");
    const back = [...element.querySelectorAll("a")].find((a) =>
      a.textContent?.includes("All phases")
    );
    expect(back?.getAttribute("href")).toBe(
      "#/applications/a1/m/app-constructor/construct/phases"
    );
    // A run is the admin's own page.
    const run = [...element.querySelectorAll("a")].find(
      (a) => a.textContent === "Run 1"
    );
    expect(run?.getAttribute("href")).toBe("#/runs/run-1");
  });

  it("moves to another of its paths on update", async () => {
    const { host } = fakeHost(construction());
    await show(host);
    expect(element.textContent).not.toContain("Phase 1 of 1");
    await act(async () => mounted?.update("/phases/4"));
    await settle();
    expect(element.textContent).toContain("Phase 1 of 1");
    await act(async () => mounted?.update("/phases/99"));
    await settle();
    expect(element.textContent).toContain(
      "This phase is not part of the plan any more."
    );
  });

  it("says so for a path it does not have", async () => {
    const { host } = fakeHost(construction(), "/nowhere");
    await show(host);
    expect(element.textContent).toContain(
      "There is no such page of Plan & build."
    );
  });
});
