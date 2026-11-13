import { describe, expect, it } from "vitest";
import { buildContract, definitionsPath, isDefinitionsPath, parseContract } from "./contract";

// What `meshnode add` writes for a script carrying a #meshware: header.
const FROM_A_SCRIPT = JSON.stringify(
  {
    type: "object",
    description: "Re-encode one media file",
    properties: {
      path: { type: "string", description: "The file to encode" },
      crf: { type: "integer", description: "Lower is bigger", default: 28 },
    },
    required: ["path"],
    additionalProperties: false,
    "x-meshware": { item: "path", cores: 2, reads: ["$path"], writes: ["$outdir"] },
  },
  null,
  2,
);

describe("contract round-trip", () => {
  it("reads the rows a script declared", () => {
    const { rows, error } = parseContract(FROM_A_SCRIPT);
    expect(error).toBeNull();
    expect(rows?.map((r) => r.name)).toEqual(["path", "crf"]);
    expect(rows?.find((r) => r.name === "path")?.required).toBe(true);
    expect(rows?.find((r) => r.name === "crf")?.kind).toBe("integer");
  });

  it("keeps what a script said about itself when an argument is edited", () => {
    // The bug this pins: the row editor models arguments and nothing else,
    // so editing one used to re-emit a schema with `x-meshware` — the item
    // input, the per-item cores, the declared reads and writes — silently
    // deleted. The Run screen then proposed the wrong plan with no sign
    // anything had been lost.
    const { rows, rootExtra } = parseContract(FROM_A_SCRIPT);
    expect(rootExtra["x-meshware"]).toEqual({
      item: "path",
      cores: 2,
      reads: ["$path"],
      writes: ["$outdir"],
    });

    rows![1].description = "edited";
    const rebuilt = JSON.parse(buildContract(rows!, rootExtra)!);
    expect(rebuilt["x-meshware"]).toEqual({
      item: "path",
      cores: 2,
      reads: ["$path"],
      writes: ["$outdir"],
    });
    expect(rebuilt.description).toBe("Re-encode one media file");
    expect(rebuilt.properties.crf.description).toBe("edited");
    expect(rebuilt.required).toEqual(["path"]);
  });

  it("still produces a plain schema for a script that declared nothing extra", () => {
    const rebuilt = JSON.parse(
      buildContract([
        {
          name: "n",
          kind: "text",
          required: false,
          default: "",
          description: "",
          options: "",
          extra: {},
        },
      ])!,
    );
    expect(rebuilt).toEqual({
      type: "object",
      properties: { n: { type: "string" } },
      additionalProperties: false,
    });
  });

  it("survives a schema the row editor cannot model", () => {
    const { rows, rootExtra } = parseContract('{"oneOf":[{"type":"object"}]}');
    expect(rows).toBeNull();
    expect(rootExtra).toEqual({});
  });
});

describe("definitions files", () => {
  it("names the companion after the whole script name", () => {
    // `run.sh` and `run.py` in one directory must not share one file.
    expect(definitionsPath("burn-in.sh")).toBe("burn-in.sh.definitions");
    expect(definitionsPath("a/b/run.py")).toBe("a/b/run.py.definitions");
  });

  it("recognises one, so the file picker can leave it out", () => {
    // A definitions file is never an entrypoint: offering it as something
    // you could run is offering a choice with a wrong answer in it.
    expect(isDefinitionsPath("scripts/burn-in.sh.definitions")).toBe(true);
    expect(isDefinitionsPath("scripts/burn-in.sh")).toBe(false);
    expect(isDefinitionsPath("definitions")).toBe(false);
  });

  it("carries the bounds a definitions file adds through the row editor", () => {
    // `min`, `max` and `pattern` compile to standard JSON Schema keywords.
    // The row editor has no field for them, so they have to survive as
    // unmodelled `extra` — editing a description must not delete a bound.
    const schema = JSON.stringify({
      type: "object",
      properties: {
        crf: { type: "integer", default: 28, minimum: 0, maximum: 51, description: "Quality" },
        tag: { type: "string", pattern: "^[a-z]+$" },
      },
      additionalProperties: false,
      "x-meshware": { item: "path", cores: 2 },
    });
    const { rows, rootExtra } = parseContract(schema);
    expect(rows).not.toBeNull();
    expect(rows![0].extra).toEqual({ minimum: 0, maximum: 51 });
    expect(rows![1].extra).toEqual({ pattern: "^[a-z]+$" });
    expect(rootExtra).toEqual({ "x-meshware": { item: "path", cores: 2 } });

    const back = JSON.parse(buildContract(rows!, rootExtra)!);
    expect(back.properties.crf.minimum).toBe(0);
    expect(back.properties.crf.maximum).toBe(51);
    expect(back.properties.tag.pattern).toBe("^[a-z]+$");
    expect(back["x-meshware"]).toEqual({ item: "path", cores: 2 });
  });
});
