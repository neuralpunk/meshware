import { describe, expect, it } from "vitest";
import type { ResultRowView } from "./bindings/ResultRowView";
import {
  cellPreview,
  documentChunks,
  documentText,
  escapeRe,
  estimateBytes,
  fmtBytes,
  hasContent,
  objectRef,
  outputKeys,
  rowJson,
  rowLines,
  rowMatches,
  sliceSafe,
  suggestedName,
} from "./answers";

function row(over: Partial<ResultRowView> = {}): ResultRowView {
  return {
    node: "kitchen-pi",
    jobId: "job-0001",
    state: "succeeded",
    exitCode: 0,
    outputs: [["stdout", "PermitRootLogin no"]],
    expectPassed: null,
    expectDetail: "",
    ...over,
  };
}

const SHAPES: [string, ResultRowView][] = [
  ["one output", row()],
  ["no outputs", row({ outputs: [] })],
  ["several outputs", row({ outputs: [["stdout", "a"], ["stderr", ""], ["exit_code", "0"]] })],
  ["an expectation", row({ expectPassed: false, expectDetail: '"yes" contains "no" → false' })],
  ["no outputs and an expectation", row({ outputs: [], expectPassed: true })],
  ["a null exit code", row({ exitCode: null, state: "lost" })],
  ["newlines in a value", row({ outputs: [["stdout", "one\ntwo\nthree\n"]] })],
];

describe("rowLines", () => {
  // The JSON pane places row 90,000 by arithmetic rather than by rendering
  // the 89,999 before it. If these two ever disagree the scrollbar lies.
  it.each(SHAPES)("counts %s without rendering it", (_name, r) => {
    expect(rowLines(r)).toBe(rowJson(r).split("\n").length);
  });

  it("is unaffected by how long a value is", () => {
    const short = row({ outputs: [["stdout", "x"]] });
    const long = row({ outputs: [["stdout", "y\n".repeat(50_000)]] });
    expect(rowLines(long)).toBe(rowLines(short));
    expect(rowJson(long).split("\n")).toHaveLength(rowLines(long));
  });
});

describe("rowJson", () => {
  it("is the array element the CLI prints", () => {
    const r = row({ expectPassed: true, expectDetail: "ok" });
    expect(JSON.parse(`[${rowJson(r)}]`)).toEqual([
      {
        node: "kitchen-pi",
        job_id: "job-0001",
        state: "succeeded",
        exit_code: 0,
        outputs: { stdout: "PermitRootLogin no" },
        expect_passed: true,
        expect_detail: "ok",
      },
    ]);
  });

  it("omits the expectation when nothing was asserted", () => {
    expect(JSON.parse(`[${rowJson(row())}]`)[0]).not.toHaveProperty("expect_passed");
  });

  it("stays valid JSON when a value is shortened for the screen", () => {
    const r = row({ outputs: [["stdout", "z".repeat(5000)]] });
    const parsed = JSON.parse(`[${rowJson(r, "  ", 100)}]`)[0];
    expect(parsed.outputs.stdout).toMatch(/^z{100}…\[\+4,900 characters\]$/);
  });

  it("leaves a value alone when it is under the clip", () => {
    const parsed = JSON.parse(`[${rowJson(row(), "  ", 1000)}]`)[0];
    expect(parsed.outputs.stdout).toBe("PermitRootLogin no");
  });
});

describe("documentChunks", () => {
  it("concatenates to a document that parses", () => {
    const rows = [row(), row({ node: "attic-nas", exitCode: 1, expectPassed: false })];
    const doc = [...documentChunks(rows)].join("");
    expect(doc).toBe(documentText(rows));
    const parsed = JSON.parse(doc);
    expect(parsed).toHaveLength(2);
    expect(parsed[1].node).toBe("attic-nas");
    expect(parsed[1].exit_code).toBe(1);
  });

  it("writes an empty set as an empty array", () => {
    expect(JSON.parse(documentText([]))).toEqual([]);
  });

  it("yields a piece per row rather than one string", () => {
    const rows = [row(), row(), row()];
    expect([...documentChunks(rows)].length).toBeGreaterThan(rows.length);
  });
});

describe("outputKeys", () => {
  it("puts declared outputs before the four that arrive for free", () => {
    const rows = [
      row({ outputs: [["stdout", "x"], ["disk_free", "42"]] }),
      row({ outputs: [["exit_code", "0"], ["kernel", "6.1"]] }),
    ];
    expect(outputKeys(rows)).toEqual(["disk_free", "kernel", "stdout", "exit_code"]);
  });

  it("skips a key nothing published under", () => {
    const rows = [row({ outputs: [["stdout", "x"], ["stderr", "   "]] as [string, string][] })];
    expect(outputKeys(rows)).toEqual(["stdout"]);
  });
});

describe("hasContent", () => {
  it("is false for blank and whitespace", () => {
    expect(hasContent("")).toBe(false);
    expect(hasContent("  \n\t ")).toBe(false);
  });
  it("is true as soon as there is anything", () => {
    expect(hasContent("   x")).toBe(true);
  });
});

describe("cellPreview", () => {
  it("collapses a multi-line value onto one line", () => {
    expect(cellPreview("one\n  two\nthree")).toBe("one two three");
  });

  it("caps what reaches the DOM", () => {
    // The point of the cap is that a 40 MB stdout never becomes a 40 MB
    // text node behind `text-overflow: ellipsis`.
    const preview = cellPreview("x".repeat(5_000_000), 200);
    expect(preview).toHaveLength(201);
    expect(preview.endsWith("…")).toBe(true);
  });

  it("reads an object reference as one", () => {
    expect(cellPreview("object:mw1abc#1048576")).toBe("‹object · 1.0 MB›");
  });
});

describe("sliceSafe", () => {
  it("never splits a surrogate pair", () => {
    const s = `${"a".repeat(9)}😀tail`;
    expect(sliceSafe(s, 10)).toBe("a".repeat(9));
    expect(() => JSON.parse(JSON.stringify(sliceSafe(s, 10)))).not.toThrow();
  });
});

describe("rowMatches", () => {
  it("looks at the node, the keys and the values", () => {
    const r = row({ outputs: [["disk_free", "12G"]] });
    expect(rowMatches(r, /kitchen/i)).toBe(true);
    expect(rowMatches(r, /disk_free/i)).toBe(true);
    expect(rowMatches(r, /12g/i)).toBe(true);
    expect(rowMatches(r, /attic/i)).toBe(false);
  });

  it("treats the query as text, not as a pattern", () => {
    const r = row({ outputs: [["stdout", "a.b"]] });
    expect(rowMatches(r, new RegExp(escapeRe("a.b"), "i"))).toBe(true);
    expect(rowMatches(r, new RegExp(escapeRe("a?b"), "i"))).toBe(false);
  });
});

describe("estimateBytes / fmtBytes", () => {
  it("grows with the values and not with the row count alone", () => {
    const small = estimateBytes([row()]);
    const big = estimateBytes([row({ outputs: [["stdout", "x".repeat(10_000)]] })]);
    expect(big - small).toBeGreaterThan(9_000);
  });

  it("reads as a size", () => {
    expect(fmtBytes(512)).toBe("512 B");
    expect(fmtBytes(2048)).toBe("2.0 KB");
    expect(fmtBytes(20 * 1024 * 1024)).toBe("20 MB");
  });
});

describe("objectRef", () => {
  it("is null for anything that is not one", () => {
    expect(objectRef("PermitRootLogin no")).toBeNull();
  });
});

describe("suggestedName", () => {
  it("is a filename, whatever the group was called", () => {
    expect(suggestedName("fan-9c2/../etc")).toMatch(/^answers-fan-9c2-etc-\d{8}-\d{4}\.json$/);
  });
  it("works with nothing to name it after", () => {
    expect(suggestedName("")).toMatch(/^answers-\d{8}-\d{4}\.json$/);
  });
});
