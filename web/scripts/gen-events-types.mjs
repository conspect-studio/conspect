#!/usr/bin/env node
/**
 * gen-events-types.mjs — generate TypeScript types from the AsyncAPI 3.0
 * document at docs/api/asyncapi.json (SUR-6 / ADR-RT006).
 *
 * This is the `generate:events` script. It reads the canonical AsyncAPI
 * document (produced by `cargo xtask gen-asyncapi`) and emits
 * `web/src/realtime/generated-types.ts` containing TypeScript interfaces for
 * every payload schema in `components/schemas`.
 *
 * The output is ADDITIVE: the hand-modelled `envelope.ts` runtime
 * (connection lifecycle, resume, conflation) is NOT replaced — it stays
 * hand-authored because the runtime semantics (ADR-RT006 Consequences) are
 * beyond what a schema-only generator produces. The generated types module
 * gives consumers precise, CI-verified types for every payload shape.
 *
 * Design: no external code-generation dependency (Modelina, etc.) to keep the
 * script simple, auditable, and zero-install. It maps JSON Schema primitives
 * to TypeScript types deterministically so re-running yields an identical file.
 */

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, "../..");
const ASYNCAPI_PATH = resolve(REPO_ROOT, "docs/api/asyncapi.json");
const OUT_PATH = resolve(__dirname, "../src/realtime/generated-types.ts");

// --- Load the AsyncAPI document ---

let doc;
try {
  const raw = readFileSync(ASYNCAPI_PATH, "utf8");
  doc = JSON.parse(raw);
} catch (err) {
  process.stderr.write(`gen-events-types: failed to read ${ASYNCAPI_PATH}: ${String(err)}\n`);
  process.exit(1);
}

const schemas = doc?.components?.schemas;
if (typeof schemas !== "object" || schemas === null) {
  process.stderr.write("gen-events-types: components/schemas not found in AsyncAPI document\n");
  process.exit(1);
}

// --- Type-mapping logic ---

/**
 * Map a JSON Schema `type` + optional `format` to a TypeScript type string.
 * This is deliberately conservative: only the formats present in our schema
 * are handled. Unknown combinations fall back to `unknown`.
 */
function primitiveType(schema) {
  if (schema["$ref"] !== undefined) {
    // $ref: "#/components/schemas/Foo" → reference to generated interface "Foo"
    const ref = String(schema["$ref"]);
    const name = ref.split("/").at(-1);
    return name !== undefined ? name : "unknown";
  }
  const t = schema.type;
  if (t === "string") {
    if (Array.isArray(schema.enum)) {
      return schema.enum.map((v) => JSON.stringify(v)).join(" | ");
    }
    if (schema.const !== undefined) {
      return JSON.stringify(schema.const);
    }
    return "string";
  }
  if (t === "boolean") {
    return "boolean";
  }
  if (t === "integer" || t === "number") {
    return "number";
  }
  if (t === "array") {
    if (schema.items !== undefined) {
      return `readonly ${primitiveType(schema.items)}[]`;
    }
    return "readonly unknown[]";
  }
  if (t === "object") {
    if (schema.additionalProperties === true) {
      return "Record<string, unknown>";
    }
    // Inline object — emit as mapped type literal.
    return buildObjectLiteral(schema, "  ");
  }
  return "unknown";
}

/** Emit an inline TypeScript object-literal type for a nested object schema. */
function buildObjectLiteral(schema, indent) {
  const props = schema.properties;
  const required = Array.isArray(schema.required) ? new Set(schema.required) : new Set();
  if (typeof props !== "object" || props === null) {
    return "Record<string, unknown>";
  }
  const lines = ["{"];
  for (const [key, propSchema] of Object.entries(props)) {
    const opt = required.has(key) ? "" : "?";
    const comment = propSchema.description !== undefined
      ? `${indent}  /** ${String(propSchema.description)} */\n`
      : "";
    lines.push(`${comment}${indent}  readonly ${key}${opt}: ${primitiveType(propSchema)};`);
  }
  lines.push(`${indent}}`);
  return lines.join("\n");
}

/**
 * Emit a TypeScript `interface` or `type` alias for one JSON Schema entry from
 * `components/schemas`. Returns the TypeScript source lines.
 */
function emitSchema(name, schema) {
  const lines = [];
  if (schema.description !== undefined) {
    lines.push(`/** ${schema.description} */`);
  }
  // Top-level enum → type alias (string union).
  if (schema.type === "string" && Array.isArray(schema.enum)) {
    const union = schema.enum.map((v) => JSON.stringify(v)).join(" | ");
    lines.push(`export type ${name} = ${union};`);
    return lines;
  }
  // oneOf → discriminated union (e.g. TallyTarget). Check before `type:object`
  // because some schemas combine both (`type: "object"` + `oneOf`).
  if (Array.isArray(schema.oneOf)) {
    const members = schema.oneOf.map((branch) => {
      if (branch["$ref"] !== undefined) {
        return primitiveType(branch);
      }
      return buildObjectLiteral(branch, "");
    });
    const unionLines = [];
    for (let i = 0; i < members.length; i++) {
      const isLast = i === members.length - 1;
      unionLines.push(`  | ${members[i]}${isLast ? ";" : ""}`);
    }
    lines.push(`export type ${name} =`);
    lines.push(...unionLines);
    return lines;
  }
  // Top-level object → interface.
  if (schema.type === "object") {
    const props = schema.properties;
    const required = Array.isArray(schema.required) ? new Set(schema.required) : new Set();
    lines.push(`export interface ${name} {`);
    if (typeof props === "object" && props !== null) {
      for (const [key, propSchema] of Object.entries(props)) {
        const opt = required.has(key) ? "" : "?";
        if (propSchema.description !== undefined) {
          lines.push(`  /** ${String(propSchema.description)} */`);
        }
        lines.push(`  readonly ${key}${opt}: ${primitiveType(propSchema)};`);
      }
    }
    lines.push("}");
    return lines;
  }
  // Fallback: opaque unknown.
  lines.push(`export type ${name} = unknown;`);
  return lines;
}

// --- Generate ---

// Emit schemas in insertion order (deterministic: asyncapi.rs build_schemas() is stable).
const schemaNames = Object.keys(schemas);
const parts = [];

parts.push([
  "// GENERATED FILE — do not edit by hand.",
  "// Source: docs/api/asyncapi.json (produced by `cargo xtask gen-asyncapi`).",
  "// Regenerate: npm run generate:events",
  "// Consumers: hand-authored runtime in envelope.ts and connection.ts is NOT",
  "// replaced — see ADR-RT006. Import from this module for precise payload types.",
  "",
].join("\n"));

for (const name of schemaNames) {
  const schema = schemas[name];
  const emitted = emitSchema(name, schema);
  parts.push(emitted.join("\n"));
  parts.push("");
}

const output = parts.join("\n");

// Write (create parent dirs if needed).
try {
  mkdirSync(dirname(OUT_PATH), { recursive: true });
  writeFileSync(OUT_PATH, output, "utf8");
} catch (err) {
  process.stderr.write(`gen-events-types: failed to write ${OUT_PATH}: ${String(err)}\n`);
  process.exit(1);
}

process.stdout.write(`wrote ${OUT_PATH}\n`);
