// Arguments <-> JSON Schema.
//
// A service's "contract" is a JSON Schema the engine validates run inputs
// against (mesh-core::services::validate_inputs). Humans do not want to
// write JSON Schema to say "this script takes a --version", so the Director
// edits arguments as rows and compiles them here. Anything the row editor
// cannot express (patterns, ranges, custom keywords) is preserved verbatim
// in `extra` and re-emitted, so a schema written by hand round-trips
// through the friendly editor without losing a keyword.

export type ArgKind = "text" | "number" | "integer" | "yesno" | "choice";

export type ArgSpec = {
  name: string;
  kind: ArgKind;
  required: boolean;
  /** Always held as a string; coerced to the declared type on compile. */
  default: string;
  description: string;
  /** Comma-separated allowed values — `choice` only. */
  options: string;
  /** Schema keywords the row editor does not model (pattern, minimum, …). */
  extra: Record<string, unknown>;
};

export const ARG_KINDS: { id: ArgKind; label: string }[] = [
  { id: "text", label: "Text" },
  { id: "number", label: "Number" },
  { id: "integer", label: "Whole number" },
  { id: "yesno", label: "Yes / no" },
  { id: "choice", label: "One of…" },
];

export function emptyArg(): ArgSpec {
  return { name: "", kind: "text", required: false, default: "", description: "", options: "", extra: {} };
}

/**
 * What a script's companion declaration file is called: `burn-in.sh` →
 * `burn-in.sh.definitions`. Kept in step with `mesh_core::script_spec`.
 */
export const DEFINITIONS_SUFFIX = ".definitions";

/** `burn-in.sh` → `burn-in.sh.definitions`. */
export function definitionsPath(scriptPath: string): string {
  return `${scriptPath}${DEFINITIONS_SUFFIX}`;
}

/**
 * True if this path *is* a definitions file. A definitions file is never an
 * entrypoint, so it never belongs in the "which file runs" picker.
 */
export function isDefinitionsPath(path: string): boolean {
  return path.endsWith(DEFINITIONS_SUFFIX);
}

/** Keywords the row editor owns; everything else survives in `extra`. */
const MODELLED = new Set(["type", "description", "default", "enum"]);
/** Root keywords the row editor owns; everything else survives in `rootExtra`. */
const MODELLED_ROOT = new Set(["type", "properties", "required", "additionalProperties"]);
/** Top-level keywords that mean "this schema is beyond the row editor". */
const UNSUPPORTED_ROOT = ["oneOf", "anyOf", "allOf", "not", "$ref", "patternProperties", "dependentSchemas"];

/**
 * Compile argument rows into a JSON Schema string.
 * Returns null when there are no named arguments — a script with no
 * declared arguments gets no contract at all, so ad-hoc inputs still work.
 */
export function buildContract(args: ArgSpec[], rootExtra: Record<string, unknown> = {}): string | null {
  const named = args.filter((a) => a.name.trim());
  if (named.length === 0) return null;

  const properties: Record<string, unknown> = {};
  const required: string[] = [];
  for (const a of named) {
    const name = a.name.trim();
    const options = a.kind === "choice" ? splitOptions(a.options) : [];
    // Key order mirrors how these schemas are written by hand, so a schema
    // that came in through parseContract() goes back out byte-identical.
    properties[name] = {
      type: jsonTypeOf(a.kind),
      ...a.extra,
      ...(a.description.trim() ? { description: a.description.trim() } : {}),
      ...(options.length ? { enum: options } : {}),
      ...(a.default.trim() ? { default: coerce(a.default.trim(), a.kind) } : {}),
    };
    if (a.required) required.push(name);
  }

  // Root keywords this editor does not model — `description`, `title`, and
  // the `x-meshware` block carrying what a script said about itself — are
  // re-emitted verbatim. Dropping them was a real bug: editing one argument
  // in the row editor silently deleted the script's declared item input,
  // its per-item core count and its declared reads and writes, and the next
  // run then proposed the wrong plan with no sign that anything had been
  // lost. Unknown *property* keywords already survived in `extra`; this is
  // the same promise, one level up.
  const schema: Record<string, unknown> = { type: "object", ...rootExtra, properties };
  if (required.length) schema.required = required;
  schema.additionalProperties = false;
  return JSON.stringify(schema, null, 2);
}

/**
 * Read a JSON Schema back into argument rows.
 * `rows` is null when the schema uses constructs the row editor cannot
 * round-trip — callers fall back to the raw JSON editor in that case.
 */
export function parseContract(contract: string | null): {
  rows: ArgSpec[] | null;
  rootExtra: Record<string, unknown>;
  error: string | null;
} {
  const text = (contract ?? "").trim();
  if (!text) return { rows: [], rootExtra: {}, error: null };

  let schema: unknown;
  try {
    schema = JSON.parse(text);
  } catch (e) {
    return { rows: null, rootExtra: {}, error: e instanceof Error ? e.message : String(e) };
  }
  if (!isRecord(schema)) return { rows: null, rootExtra: {}, error: "not a JSON object" };
  if (UNSUPPORTED_ROOT.some((k) => k in schema)) return { rows: null, rootExtra: {}, error: null };
  if (schema.type !== undefined && schema.type !== "object") return { rows: null, rootExtra: {}, error: null };

  const props = schema.properties;
  if (props !== undefined && !isRecord(props)) return { rows: null, rootExtra: {}, error: null };
  const rootExtra: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(schema)) if (!MODELLED_ROOT.has(k)) rootExtra[k] = v;
  const required = Array.isArray(schema.required) ? schema.required.map(String) : [];

  const rows: ArgSpec[] = [];
  for (const [name, raw] of Object.entries(props ?? {})) {
    if (!isRecord(raw)) return { rows: null, rootExtra: {}, error: null };
    const type = typeof raw.type === "string" ? raw.type : "string";
    const options = Array.isArray(raw.enum) ? raw.enum.map(String) : null;
    const extra: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(raw)) if (!MODELLED.has(k)) extra[k] = v;

    rows.push({
      name,
      kind: kindOf(type, options !== null),
      required: required.includes(name),
      default: raw.default === undefined ? "" : String(raw.default),
      description: typeof raw.description === "string" ? raw.description : "",
      options: options ? options.join(", ") : "",
      extra,
    });
  }
  return { rows, rootExtra, error: null };
}

// ---------------------------------------------------------------------------
// Run-time view of a contract: the fields the Run form renders
// ---------------------------------------------------------------------------

export type ContractField = {
  name: string;
  /** string | number | integer | boolean */
  type: string;
  description: string;
  required: boolean;
  options: string[] | null;
  default: string;
  min: number | null;
  max: number | null;
  pattern: string | null;
};

/** The inputs a service declares, ready to render. Empty when uncontracted. */
export function contractFields(contract: string | null): ContractField[] {
  if (!contract?.trim()) return [];
  let schema: unknown;
  try {
    schema = JSON.parse(contract);
  } catch {
    return [];
  }
  if (!isRecord(schema) || !isRecord(schema.properties)) return [];
  const required = Array.isArray(schema.required) ? schema.required.map(String) : [];

  return Object.entries(schema.properties).map(([name, raw]) => {
    const p = isRecord(raw) ? raw : {};
    return {
      name,
      type: typeof p.type === "string" ? p.type : "string",
      description: typeof p.description === "string" ? p.description : "",
      required: required.includes(name),
      options: Array.isArray(p.enum) ? p.enum.map(String) : null,
      default: p.default === undefined ? "" : String(p.default),
      min: typeof p.minimum === "number" ? p.minimum : null,
      max: typeof p.maximum === "number" ? p.maximum : null,
      pattern: typeof p.pattern === "string" ? p.pattern : null,
    };
  });
}

/** Is this JSON Schema text usable? Returns a human sentence, ok or not. */
export function contractStatus(contract: string): { ok: boolean; text: string } | null {
  const t = contract.trim();
  if (!t) return null;
  try {
    const parsed: unknown = JSON.parse(t);
    const n = isRecord(parsed) && isRecord(parsed.properties) ? Object.keys(parsed.properties).length : 0;
    return { ok: true, text: `valid JSON · ${n} argument${n === 1 ? "" : "s"}` };
  } catch (e) {
    return { ok: false, text: e instanceof Error ? e.message : String(e) };
  }
}

// ---------------------------------------------------------------------------
// Shared shell rendering: what the run will actually look like
// ---------------------------------------------------------------------------

/**
 * How each runtime presents itself. One tint per runtime, used by every
 * surface that shows one, so "that's a python one" is a colour you recognise
 * before you read the word.
 */
export type RuntimeMeta = { id: string; label: string; short: string; bg: string; fg: string };

export const RUNTIMES: RuntimeMeta[] = [
  // Tokens, not literals: a badge painted in fixed light tints was the
  // brightest thing on the page once the app had a dark theme.
  { id: "bash", label: "Bash", short: "sh", bg: "var(--rt-sh-bg)", fg: "var(--rt-sh-fg)" },
  { id: "python", label: "Python", short: "py", bg: "var(--rt-py-bg)", fg: "var(--rt-py-fg)" },
  { id: "ansible", label: "Ansible playbook", short: "ans", bg: "var(--rt-ans-bg)", fg: "var(--rt-ans-fg)" },
  { id: "exec", label: "Run the file directly", short: "bin", bg: "var(--rt-bin-bg)", fg: "var(--rt-bin-fg)" },
  // N4. The entrypoint is an **image reference**, not a file in the bundle
  // — the label says so, because this is the one runtime where the field
  // above it means something different.
  { id: "container", label: "Container image", short: "oci", bg: "var(--rt-oci-bg)", fg: "var(--rt-oci-fg)" },
];

export function runtimeMeta(id: string): RuntimeMeta {
  return RUNTIMES.find((r) => r.id === id) ?? RUNTIMES[3];
}

/** How a runtime invokes an entrypoint — the left half of the preview line. */
export function runtimeProgram(runtime: string, entrypoint: string): string {
  switch (runtime) {
    case "python":
      return `python ${entrypoint}`;
    case "ansible":
      return `ansible-playbook ${entrypoint}`;
    case "exec":
      return `./${entrypoint}`;
    // N4. The preview is "the command line this will run", and for a
    // container the honest answer is the runtime plus the image — the
    // exact flags come from the node's plugin, which this side cannot
    // know and must not invent.
    case "container":
      return `container ${entrypoint}`;
    default:
      return `bash ${entrypoint}`;
  }
}

/** Inputs reach the script as `--key value` argv (mesh-core::runtime::kv_args). */
export function argvOf(inputs: [string, string][]): string {
  return inputs.map(([k, v]) => `--${k} ${quote(v)}`).join(" ");
}

function quote(v: string): string {
  return v === "" || /[\s"'$`\\]/.test(v) ? JSON.stringify(v) : v;
}

// ---------------------------------------------------------------------------

function jsonTypeOf(kind: ArgKind): string {
  switch (kind) {
    case "number":
      return "number";
    case "integer":
      return "integer";
    case "yesno":
      return "boolean";
    default:
      return "string";
  }
}

function kindOf(type: string, hasEnum: boolean): ArgKind {
  if (hasEnum) return "choice";
  switch (type) {
    case "number":
      return "number";
    case "integer":
      return "integer";
    case "boolean":
      return "yesno";
    default:
      return "text";
  }
}

function coerce(value: string, kind: ArgKind): unknown {
  if (kind === "number" || kind === "integer") {
    const n = Number(value);
    return Number.isFinite(n) ? n : value;
  }
  if (kind === "yesno") {
    if (value === "true") return true;
    if (value === "false") return false;
  }
  return value;
}

export function splitOptions(options: string): string[] {
  return options
    .split(",")
    .map((o) => o.trim())
    .filter(Boolean);
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
