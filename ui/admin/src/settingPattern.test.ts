import { describe, expect, it } from "vitest";
import { patternProblem } from "./settingPattern";
import { SECRET_SENTINEL, type FieldSpec } from "./settings";

const keyId: FieldSpec = {
  name: "asc_key_id",
  label: "Key ID",
  type: "text",
  required: false,
  options: [],
  multiline: false,
  pattern: "[A-Z0-9]{10}",
  pattern_error: "Ten capital letters and digits.",
};

describe("patternProblem", () => {
  it("accepts a value matching the whole pattern", () => {
    expect(patternProblem(keyId, "2X9R4HXF34")).toBeNull();
  });
  it("says the field's own sentence when the value does not match", () => {
    expect(patternProblem(keyId, "2X9R4HXF34 extra")).toBe("Ten capital letters and digits.");
    expect(patternProblem(keyId, "short")).toBe("Ten capital letters and digits.");
  });
  it("does not judge an empty value or a stored secret", () => {
    expect(patternProblem(keyId, "")).toBeNull();
    expect(patternProblem(keyId, SECRET_SENTINEL)).toBeNull();
  });
  it("leaves a field without a pattern, or with one it cannot compile, to the server", () => {
    expect(patternProblem({ ...keyId, pattern: null }, "anything")).toBeNull();
    expect(patternProblem({ ...keyId, pattern: "(" }, "anything")).toBeNull();
  });
});
