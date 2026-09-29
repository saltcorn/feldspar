/**
 * The store filesystem's own logic, against a stubbed client.
 *
 * What is worth testing here is exactly what has no VS Code in it: which path a
 * URI names, which bytes a base64 string is, and which of the four failures an
 * HTTP status means — the last being the one that decides whether the admin sees
 * "new file" or a red error box.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ApiClient } from "./client";
import {
  StoreFileError,
  StoreFiles,
  decodeBase64,
  encodeBase64,
  kindOfStatus,
  parentPath,
  requestedPath,
  toStorePath,
  toStorePathOrNull,
  toUriPath,
} from "./storeFiles";

/** Every call the stub was asked to make, in order — `endpoint path` strings. */
const calls: string[] = [];

/** A store held in a map, standing in for the server's file endpoints. */
function stubClient(files: Record<string, string>): ApiClient {
  const failure = (name: string, status: number, message: string) =>
    new Error(`${name} failed: ${status}: ${message}`);
  const directories = () => {
    const dirs = new Set<string>();
    for (const path of Object.keys(files)) {
      const parts = path.split("/");
      for (let i = 1; i < parts.length; i += 1) dirs.add(parts.slice(0, i).join("/"));
    }
    return dirs;
  };
  const client = {
    async browseFiles(_store: string, body: { dir: string }) {
      calls.push(`browse ${body.dir}`);
      const dir = body.dir.replace(/\/+$/, "");
      if (dir !== "" && !directories().has(dir)) {
        throw failure("browseFiles", 404, `"${dir}" does not exist`);
      }
      const prefix = dir === "" ? "" : `${dir}/`;
      const seen = new Map<string, { name: string; path: string; is_dir: boolean; size: number }>();
      for (const [path, contents] of Object.entries(files)) {
        if (!path.startsWith(prefix)) continue;
        const rest = path.slice(prefix.length);
        const cut = rest.indexOf("/");
        const name = cut === -1 ? rest : rest.slice(0, cut);
        seen.set(name, {
          name,
          path: `${prefix}${name}`,
          is_dir: cut !== -1,
          size: cut === -1 ? contents.length : 0,
        });
      }
      return [...seen.values()].sort((a, b) => a.name.localeCompare(b.name));
    },
    async readFile(_store: string, body: { path: string }) {
      calls.push(`read ${body.path}`);
      const contents = files[body.path];
      if (contents === undefined) {
        throw failure("readFile", 404, `"${body.path}" does not exist`);
      }
      return { path: body.path, size: contents.length, base64: btoa(contents), text: contents };
    },
    async writeFile(_store: string, body: { path: string; base64?: string | null }) {
      calls.push(`write ${body.path}`);
      files[body.path] = atob(body.base64 ?? "");
      return { name: body.path, path: body.path, is_dir: false, size: files[body.path].length };
    },
    async makeDirectory(_store: string, body: { path: string }) {
      calls.push(`mkdir ${body.path}`);
      files[`${body.path}/.keep`] = "";
      return { name: body.path, path: body.path, is_dir: true, size: 0 };
    },
    async deleteFile(_store: string, body: { path: string }) {
      calls.push(`delete ${body.path}`);
      let deleted = false;
      for (const path of Object.keys(files)) {
        if (path === body.path || path.startsWith(`${body.path}/`)) {
          delete files[path];
          deleted = true;
        }
      }
      return { deleted };
    },
    async renameFile(_store: string, body: { from: string; to: string }) {
      calls.push(`rename ${body.from}`);
      if (files[body.to] !== undefined) {
        throw failure("renameFile", 400, "the destination already exists");
      }
      const contents = files[body.from];
      if (contents === undefined) throw failure("renameFile", 404, "does not exist");
      delete files[body.from];
      files[body.to] = contents;
      return { name: body.to, path: body.to, is_dir: false, size: contents.length };
    },
  };
  return client as unknown as ApiClient;
}

beforeEach(() => {
  calls.length = 0;
  // Fake time, so a test can step past a listing's lifetime without waiting.
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("paths", () => {
  it("maps a workspace URI path to a store path and back", () => {
    expect(toStorePath("app", "/app")).toBe("");
    expect(toStorePath("app", "/app/")).toBe("");
    expect(toStorePath("app", "/app/src/App.tsx")).toBe("src/App.tsx");
    expect(toUriPath("app", "")).toBe("/app");
    expect(toUriPath("app", "src/App.tsx")).toBe("/app/src/App.tsx");
  });

  it("refuses a path outside the workspace folder", () => {
    expect(() => toStorePath("app", "/other/App.tsx")).toThrow(StoreFileError);
  });

  it("answers rather than throws where a caller may be handed a foreign URI", () => {
    // What a Source Control menu command gets is whatever the workbench put in
    // its argument list, so "not one of ours" is an answer there.
    expect(toStorePathOrNull("app", "/app/src/App.tsx")).toBe("src/App.tsx");
    expect(toStorePathOrNull("app", "/other/App.tsx")).toBeNull();
    expect(toStorePathOrNull("app", "/app")).toBeNull();
    expect(toStorePathOrNull("app", "/app/")).toBeNull();
  });

  it("opens the file a page URL names, and only one inside the store", () => {
    // "Open in IDE" on a model's program (Stan TODO §18).
    const at = (query: string) => requestedPath(`https://x.test/ide/?store=models${query}`);
    expect(at("&path=radon.stan")).toBe("radon.stan");
    expect(at("&path=%2Fprograms%2Fradon.stan")).toBe("programs/radon.stan");
    expect(at("")).toBeNull();
    expect(at("&path=")).toBeNull();
    expect(at("&path=..%2Fsecrets")).toBeNull();
    expect(at("&path=a%2F%2Fb")).toBeNull();
  });

  it("knows a path's parent", () => {
    expect(parentPath("src/App.tsx")).toBe("src");
    expect(parentPath("package.json")).toBe("");
  });
});

describe("base64", () => {
  it("round-trips bytes that are not text", () => {
    const bytes = new Uint8Array([0, 1, 2, 250, 255, 128, 10]);
    expect(decodeBase64(encodeBase64(bytes))).toEqual(bytes);
  });

  it("round-trips a file larger than one spread's worth of arguments", () => {
    const bytes = new Uint8Array(200_000).map((_, i) => i % 256);
    expect(decodeBase64(encodeBase64(bytes))).toEqual(bytes);
  });
});

describe("StoreFiles", () => {
  const store = () =>
    new StoreFiles(
      "app",
      stubClient({
        "package.json": "{}",
        "src/App.tsx": "export const App = () => null;\n",
        "src/main.tsx": "",
      }),
    );

  it("reads back what it wrote", async () => {
    const files = store();
    await files.write("src/App.tsx", new TextEncoder().encode("changed\n"));
    expect(new TextDecoder().decode(await files.read("src/App.tsx"))).toBe("changed\n");
  });

  it("lists a directory as files and directories", async () => {
    const entries = await store().list("");
    expect(entries).toEqual([
      { name: "package.json", kind: "file", size: 2 },
      { name: "src", kind: "directory", size: 0 },
    ]);
  });

  it("stats a path by looking in its parent, and reports a missing one as null", async () => {
    const files = store();
    expect(await files.stat("src/App.tsx")).toEqual({
      name: "App.tsx",
      kind: "file",
      size: 31,
    });
    expect(await files.stat("src")).toEqual({ name: "src", kind: "directory", size: 0 });
    expect(await files.stat("")).toEqual({ name: "app", kind: "directory", size: 0 });
    expect(await files.stat("src/Missing.tsx")).toBeNull();
    // The interesting case: not there *because its parent is not there either*.
    expect(await files.stat("nowhere/Missing.tsx")).toBeNull();
  });

  it("deletes a directory and everything in it", async () => {
    const files = store();
    await files.remove("src");
    expect(await files.list("")).toEqual([{ name: "package.json", kind: "file", size: 2 }]);
  });

  it("moves a file, and refuses to clobber unless asked", async () => {
    const files = store();
    await files.move("src/App.tsx", "src/Renamed.tsx", false);
    expect(await files.stat("src/App.tsx")).toBeNull();
    expect(await files.stat("src/Renamed.tsx")).not.toBeNull();

    await expect(files.move("src/Renamed.tsx", "src/main.tsx", false)).rejects.toMatchObject({
      kind: "exists",
    });
    await files.move("src/Renamed.tsx", "src/main.tsx", true);
    expect(new TextDecoder().decode(await files.read("src/main.tsx"))).toContain("export const App");
  });

  /**
   * The requirement this whole class is shaped around: an editor asks constantly
   * whether optional files exist, and a store cannot answer "no" except by failing
   * a request. A 404 in the operator's log should mean something asked for a thing
   * that is not there — so the IDE must not be the thing routinely asking.
   */
  it("answers a path under a missing directory without asking the server", async () => {
    const files = store();
    // What opening a store in VS Code actually does.
    await expect(files.read(".vscode/settings.json")).rejects.toMatchObject({ kind: "notFound" });
    await expect(files.read(".vscode/tasks.json")).rejects.toMatchObject({ kind: "notFound" });
    await expect(files.list(".vscode")).rejects.toMatchObject({ kind: "notFound" });
    expect(await files.stat(".vscode/launch.json")).toBeNull();

    // One listing — of the root, which exists — answered all of it.
    expect(calls).toEqual(["browse "]);
  });

  it("asks once for a directory however many questions are asked about it", async () => {
    const files = store();
    await Promise.all([
      files.stat("src/App.tsx"),
      files.stat("src/main.tsx"),
      files.stat("src/Missing.tsx"),
    ]);
    // The root's listing and `src`'s, each fetched once despite the concurrency.
    expect(calls).toEqual(["browse ", "browse src"]);
  });

  it("refetches a listing when it is listed, because refresh must mean refresh", async () => {
    const files = store();
    await files.list("");
    await files.list("");
    expect(calls).toEqual(["browse ", "browse "]);
  });

  it("forgets what it knew about a path it changed", async () => {
    const files = store();
    expect(await files.stat("src/New.tsx")).toBeNull();
    await files.write("src/New.tsx", new TextEncoder().encode("x"));
    // The new file is visible without an explicit refresh…
    expect(await files.stat("src/New.tsx")).not.toBeNull();
    // …and so is a deletion.
    await files.remove("src/New.tsx");
    expect(await files.stat("src/New.tsx")).toBeNull();
  });

  /**
   * The cost of remembering listings, and its three bounds. A file created
   * *outside* the IDE — a git pull, the file manager, a build — is the only thing
   * a remembered listing can be wrong about; a deletion cannot mislead, because
   * the listing still names the file and reading it asks the server.
   */
  describe("a file created outside the IDE", () => {
    it("is seen once the listing is too old to believe", async () => {
      const backing: Record<string, string> = { "src/App.tsx": "x" };
      const files = new StoreFiles("app", stubClient(backing), 5_000);
      expect(await files.stat("src/Outside.tsx")).toBeNull();

      backing["src/Outside.tsx"] = "made elsewhere";
      // Within the window, the remembered listing still answers.
      expect(await files.stat("src/Outside.tsx")).toBeNull();

      vi.setSystemTime(Date.now() + 6_000);
      expect(await files.stat("src/Outside.tsx")).not.toBeNull();
    });

    it("is seen at once when the page regains focus", async () => {
      const backing: Record<string, string> = { "src/App.tsx": "x" };
      const files = new StoreFiles("app", stubClient(backing));
      expect(await files.stat("src/Outside.tsx")).toBeNull();

      backing["src/Outside.tsx"] = "made elsewhere";
      files.forgetEverything();
      expect(await files.stat("src/Outside.tsx")).not.toBeNull();
    });

    it("is seen by anything that lists, which is how the explorer refreshes", async () => {
      const backing: Record<string, string> = { "src/App.tsx": "x" };
      const files = new StoreFiles("app", stubClient(backing));
      await files.list("src");

      backing["src/Outside.tsx"] = "made elsewhere";
      expect((await files.list("src")).map((e) => e.name)).toContain("Outside.tsx");
    });

    it("does not mislead in the other direction: a deletion still reaches the server", async () => {
      const backing: Record<string, string> = { "src/App.tsx": "x" };
      const files = new StoreFiles("app", stubClient(backing));
      await files.stat("src/App.tsx");

      delete backing["src/App.tsx"];
      calls.length = 0;
      // The listing still names it, so this asks — and the 404 is a real one,
      // which the server is right to log.
      await expect(files.read("src/App.tsx")).rejects.toMatchObject({ kind: "notFound" });
      expect(calls).toContain("read src/App.tsx");
    });
  });

  it("reports a missing file as notFound rather than a failure", async () => {
    await expect(store().read("src/Missing.tsx")).rejects.toMatchObject({
      kind: "notFound",
    });
  });
});

describe("kindOfStatus", () => {
  it("maps the statuses a file operation can produce", () => {
    expect(kindOfStatus(404)).toBe("notFound");
    expect(kindOfStatus(400)).toBe("exists");
    expect(kindOfStatus(401)).toBe("noPermission");
    expect(kindOfStatus(403)).toBe("noPermission");
    expect(kindOfStatus(500)).toBe("failed");
    expect(kindOfStatus(null)).toBe("failed");
  });
});
