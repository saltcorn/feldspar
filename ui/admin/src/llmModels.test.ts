/**
 * A provider's models as the admin screens present them: the agent form's
 * model pick-list, and the summary a model row shows.
 */

import { describe, expect, it } from "vitest";

import {
  capabilitySummary,
  formatPrice,
  formatTokens,
  groupByKind,
  guessKind,
  kindOf,
  kindsServedBy,
  modelOptions,
  namesToOffer,
  priceSummary,
  type ModelItem,
} from "./llmModels";

function model(name: string, isDefault = false, kind?: string): ModelItem {
  return {
    id: name,
    provider_id: "p1",
    name,
    description: "",
    kind,
    is_default: isDefault,
    config: {},
    capabilities: {},
    prices: {},
  };
}

describe("the agent form's model pick-list", () => {
  it("offers the provider default first, naming it, then each model with the default marked", () => {
    const options = modelOptions([model("claude-sonnet-5", true), model("claude-haiku-4-5")], "");
    expect(options).toEqual([
      { value: "", label: "Provider default (claude-sonnet-5)" },
      { value: "claude-haiku-4-5", label: "claude-haiku-4-5" },
      { value: "claude-sonnet-5", label: "claude-sonnet-5 (default)" },
    ]);
  });

  it("says when the provider has no default", () => {
    expect(modelOptions([model("gpt-5.1")], "")[0].label).toBe("Provider default (none set)");
    expect(modelOptions([], "")[0].label).toBe("Provider default (none set)");
  });

  it("keeps a model the agent names that has no row, marked missing", () => {
    const options = modelOptions([model("claude-sonnet-5", true)], "claude-opus-4-1");
    expect(options[options.length - 1]).toEqual({
      value: "claude-opus-4-1",
      label: "claude-opus-4-1 (missing)",
    });
    // Not when it exists.
    expect(modelOptions([model("claude-sonnet-5", true)], "claude-sonnet-5")).toHaveLength(2);
  });
});

describe("a model row's summary", () => {
  it("shows a blank price as unknown, never as free", () => {
    expect(formatPrice(null)).toBe("unknown");
    expect(formatPrice(undefined)).toBe("unknown");
    expect(formatPrice(0)).toBe("0/M");
    expect(formatPrice(3)).toBe("3/M");
  });

  it("abbreviates token counts", () => {
    expect(formatTokens(200_000)).toBe("200k");
    expect(formatTokens(1_000_000)).toBe("1M");
    expect(formatTokens(32_000)).toBe("32k");
    expect(formatTokens(512)).toBe("512");
  });

  it("lists what the resolved capabilities add", () => {
    expect(
      capabilitySummary({
        parallel_tool_calls: true,
        parallel_tool_calls_default: false,
        native_apply_patch: true,
        reasoning_replay: true,
        prompt_caching: "automatic",
        edit_format: "apply_patch",
        context_window: 400_000,
        working_budget: 100_000,
        vision: true,
      }),
    ).toEqual([
      "400k window",
      "100k budget",
      "apply_patch",
      "automatic caching",
      "vision",
      "reasoning replay",
      "native apply_patch",
    ]);
  });

  it("offers only fetched names that still have no row", () => {
    expect(namesToOffer(["a", "b", "c"], [model("b")])).toEqual(["a", "c"]);
  });
});

describe("models by kind (TODO-agent-skills AS3.1)", () => {
  it("lists each kind under its own heading, chat first, and a row with no kind is chat", () => {
    const groups = groupByKind([
      model("tts-1", true, "speech"),
      model("gpt-5.1", true),
      model("text-embedding-3-small", true, "embedding"),
    ]);
    expect(groups.map((g) => [g.kind, g.models.map((m) => m.name)])).toEqual([
      ["chat", ["gpt-5.1"]],
      ["embedding", ["text-embedding-3-small"]],
      ["speech", ["tts-1"]],
    ]);
    expect(kindOf(model("x", false, "video"))).toBe("chat");
    expect(kindsServedBy("anthropic")).toEqual(["chat"]);
    expect(kindsServedBy("openai_chat")).toContain("image");
  });

  it("guesses a fetched name's kind", () => {
    expect(guessKind("text-embedding-3-small")).toBe("embedding");
    expect(guessKind("gpt-4o-mini-tts")).toBe("speech");
    expect(guessKind("gpt-image-1")).toBe("image");
    expect(guessKind("gpt-5.1")).toBe("chat");
  });

  it("offers an agent chat models only", () => {
    const options = modelOptions(
      [model("gpt-5.1", true), model("text-embedding-3-small", true, "embedding")],
      "",
    );
    expect(options.map((o) => o.value)).toEqual(["", "gpt-5.1"]);
  });

  it("shows each kind's own prices", () => {
    const blank = {
      input: null,
      cached_input: null,
      cache_write: null,
      output: null,
    };
    expect(priceSummary("embedding", { ...blank, input: 0.02 })).toBe("0.02/M input");
    expect(priceSummary("image", { ...blank, per_image: 0.04 })).toBe("0.04 per image");
    expect(priceSummary("speech", { ...blank, per_million_characters: 15 })).toBe("15/M characters");
    expect(priceSummary("chat", blank)).toBe("unknown / unknown");
  });
});
