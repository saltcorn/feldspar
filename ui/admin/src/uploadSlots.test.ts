/**
 * The application form's upload slots (TODO F2.6): what an admin types — `1G`,
 * `video/mp4, video/webm`, a blank hook — becomes bytes, a list and `null`, and
 * a stored slot comes back as the same text.
 */

import { describe, expect, it } from "vitest";

import {
  blankUploadRow,
  formatBytes,
  parseBytes,
  uploadRowsFromApp,
  uploadRowsToRequest,
} from "./uploadSlots";

describe("parseBytes", () => {
  it("reads bytes and binary units, and empty as the server's limit", () => {
    expect(parseBytes("")).toBe(0);
    expect(parseBytes("1000")).toBe(1000);
    expect(parseBytes("5M")).toBe(5 * 1024 * 1024);
    expect(parseBytes("1g")).toBe(1024 ** 3);
    expect(parseBytes("1.5 GB")).toBe(1.5 * 1024 ** 3);
    expect(parseBytes("lots")).toBeNull();
  });

  it("is what formatBytes writes", () => {
    for (const n of [0, 1000, 1024, 5 * 1024 * 1024, 1024 ** 3]) {
      expect(parseBytes(formatBytes(n))).toBe(n);
    }
    expect(formatBytes(1024 ** 3)).toBe("1G");
  });
});

describe("upload rows", () => {
  it("round-trip a stored slot", () => {
    const slot = {
      name: "drop",
      store: "files",
      folder: "drops",
      min_role: 80,
      max_bytes: 1024 ** 3,
      mime_allow: ["video/mp4", "video/webm"],
      hook: "beforeDrop",
    };
    const rows = uploadRowsFromApp([slot]);
    expect(rows[0].max_bytes).toBe("1G");
    expect(rows[0].mime_allow).toBe("video/mp4, video/webm");
    expect(uploadRowsToRequest(rows)).toEqual([slot]);
  });

  it("drops a blank row and sends a blank hook as null", () => {
    const filled = { ...blankUploadRow(), name: "drop", store: "files", folder: "/a/" };
    expect(uploadRowsToRequest([blankUploadRow(), filled])).toEqual([
      {
        name: "drop",
        store: "files",
        folder: "a",
        min_role: 1,
        max_bytes: 0,
        mime_allow: [],
        hook: null,
      },
    ]);
  });

  it("names the slot whose size does not parse", () => {
    const bad = { ...blankUploadRow(), name: "drop", store: "files", max_bytes: "big" };
    expect(() => uploadRowsToRequest([bad])).toThrow(/drop/);
  });
});
