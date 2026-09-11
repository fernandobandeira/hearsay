#!/usr/bin/env node
/**
 * Drift check: the React reader's hand-written contract vs. the generated client.
 *
 * The reader in ~/git/narrator/web wrote the server's API down by hand, in
 * `src/lib/types.ts` and `src/lib/api.ts`. This Rust server is supposed to be a
 * drop-in replacement for the Python one those types describe. So the question
 * this script answers is narrow and mechanical:
 *
 *   Is a value produced by the generated client assignable to the type the
 *   reader already expects?
 *
 * That direction is the one that matters. If the generated `Status` is
 * assignable to the reader's `Status`, the reader compiles against the new
 * client untouched. Anything that breaks that assignment is a MISMATCH.
 *
 * Three verdicts, and the difference between them is the whole point:
 *
 *   MISMATCH  — breaks the assignment. A required field is gone, a type
 *               changed, a field the reader requires became optional, or the
 *               nullability moved. gen-client.sh exits non-zero on these.
 *   NOTE      — safe but worth knowing: an *optional* hand-written field the
 *               server no longer sends, or a field the server now always sends
 *               that the reader treated as optional. Not a failure.
 *   ADDITIVE  — fields only the generated type has. The server grew; the
 *               reader ignores them. Not a failure.
 *
 * Nothing under ~/git/narrator is ever written to. It is read as the contract.
 *
 * Usage: node scripts/drift-check.mjs
 * Exit:  0 when there are no mismatches and no missing endpoints, 1 otherwise.
 */

import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';
import fs from 'node:fs';
import path from 'node:path';

const SCRIPTS_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(SCRIPTS_DIR, '..');
const CLIENT_DIR = path.join(REPO, 'client');
const GEN_TYPES = path.join(CLIENT_DIR, 'types.gen.ts');
const SPEC = path.join(REPO, 'openapi.json');
const REPORT = path.join(CLIENT_DIR, 'DRIFT.md');

/** The reader. Read-only, always — it belongs to the other repo. */
const READER = process.env.NARRATOR_WEB ??
  path.resolve(process.env.HOME ?? '~', 'git/narrator/web/src/lib');
const HAND_TYPES = path.join(READER, 'types.ts');
const HAND_API = path.join(READER, 'api.ts');

/** typescript is pinned in scripts/client, which is not on the default path. */
const require = createRequire(path.join(SCRIPTS_DIR, 'client', 'package.json'));
const ts = require('typescript');

for (const f of [HAND_TYPES, HAND_API, GEN_TYPES, SPEC]) {
  if (!fs.existsSync(f)) {
    console.error(`drift-check: missing input ${f}`);
    process.exit(2);
  }
}

// ---------------------------------------------------------------------------
// Which hand-written interface answers to which generated type.
// The names diverged in two places; everything else lines up.
// ---------------------------------------------------------------------------
const PAIRS = [
  ['BookFile', 'BookFile'],
  ['ChapMeta', 'ChapMeta'],
  ['SavedPosition', 'StampedPosition'],
  ['LoadResult', 'LoadResult'],
  ['ChapRow', 'ChapterRow'],
  ['ChaptersResult', 'ChaptersResult'],
  ['Status', 'Status'],
  ['BookIndex', 'BookIndex'],
  ['TextShard', 'TextShard'],
  ['ChapterText', 'ChapterText'],
  ['NoteResult', 'NoteResult'],
];

// ---------------------------------------------------------------------------
// Type comparison, through the compiler rather than through string matching:
// nested $refs, inline object literals and arrays all resolve structurally.
// ---------------------------------------------------------------------------
const program = ts.createProgram([HAND_TYPES, GEN_TYPES], {
  strict: true,                       // strictNullChecks: null is part of the type
  noEmit: true,
  target: ts.ScriptTarget.ESNext,
  module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  skipLibCheck: true,
});
const checker = program.getTypeChecker();

const handFile = program.getSourceFile(HAND_TYPES);
const genFile = program.getSourceFile(GEN_TYPES);

/** Top-level `interface X` / `type X = …` declarations in a file, by name. */
function declarations(sourceFile) {
  const out = new Map();
  sourceFile.forEachChild((node) => {
    if (ts.isInterfaceDeclaration(node) || ts.isTypeAliasDeclaration(node)) {
      out.set(node.name.text, node);
    }
  });
  return out;
}

const handDecls = declarations(handFile);
const genDecls = declarations(genFile);

const isNullish = (t) => !!(t.flags & (ts.TypeFlags.Null | ts.TypeFlags.Undefined));
const parts = (t) => (t.isUnion() ? t.types : [t]);
const has = (t, flag) => parts(t).some((p) => p.flags & flag);
const show = (t) => checker.typeToString(t, undefined,
  ts.TypeFormatFlags.NoTruncation | ts.TypeFormatFlags.UseFullyQualifiedType * 0);

/** `name?: string | null`, the way it reads in a declaration. */
const sig = (info) =>
  `${info.name}${info.optional ? '?' : ''}: ` +
  show(info.type).replace(/ \| undefined$/, '');

/** A property's declared type, and whether `?` was on it. */
function propInfo(sym) {
  const decl = sym.valueDeclaration ?? sym.declarations?.[0];
  const type = decl
    ? checker.getTypeOfSymbolAtLocation(sym, decl)
    : checker.getTypeOfSymbol(sym);
  return {
    name: sym.getName(),
    type,
    optional: !!(sym.flags & ts.SymbolFlags.Optional),
    hasNull: has(type, ts.TypeFlags.Null),
  };
}

function propsOf(decl) {
  const type = checker.getTypeAtLocation(decl);
  const out = new Map();
  for (const sym of checker.getPropertiesOfType(type)) out.set(sym.getName(), propInfo(sym));
  return out;
}

/**
 * Does every non-nullish constituent of the generated type match something in
 * the hand-written one? Union membership is enough — a `number` constituent is
 * never assignable to `null`, so a false positive here is not possible.
 */
function baseTypesAgree(gen, hand) {
  const genParts = parts(gen).filter((p) => !isNullish(p));
  if (genParts.length === 0) return true;
  return genParts.every((p) => checker.isTypeAssignableTo(p, hand));
}

const findings = [];   // {iface, field, kind, verdict, expected, got, note}
const record = (f) => findings.push(f);

for (const [handName, genName] of PAIRS) {
  const handDecl = handDecls.get(handName);
  const genDecl = genDecls.get(genName);

  if (!handDecl) {
    record({iface: handName, field: '*', kind: 'missing-interface', verdict: 'MISMATCH',
      expected: `interface ${handName} in ${path.relative(REPO, HAND_TYPES)}`, got: 'not found'});
    continue;
  }
  if (!genDecl) {
    record({iface: handName, field: '*', kind: 'missing-schema', verdict: 'MISMATCH',
      expected: `generated type ${genName}`, got: 'not generated — the server has no such schema'});
    continue;
  }

  const handProps = propsOf(handDecl);
  const genProps = propsOf(genDecl);

  for (const [name, h] of handProps) {
    const g = genProps.get(name);

    if (!g) {
      record({
        iface: handName, field: name,
        kind: h.optional ? 'missing-optional' : 'missing-required',
        verdict: h.optional ? 'NOTE' : 'MISMATCH',
        expected: sig(h),
        got: 'absent from the generated type',
        note: h.optional
          ? 'the reader already treats it as possibly absent, so nothing breaks'
          : 'the reader reads this field unconditionally',
      });
      continue;
    }

    // Optionality. Only one direction breaks the assignment.
    if (g.optional && !h.optional) {
      record({iface: handName, field: name, kind: 'optionality', verdict: 'MISMATCH',
        expected: `${sig(h)}  (required)`,
        got: `${sig(g)}  (optional)`,
        note: 'generated may omit a field the reader requires'});
    } else if (!g.optional && h.optional) {
      record({iface: handName, field: name, kind: 'now-always-sent', verdict: 'NOTE',
        expected: sig(h),
        got: `${sig(g)}  (always present)`,
        note: 'server is stricter than the reader assumed — safe'});
    }

    // Nullability. Only one direction breaks `generated -> hand-written`
    // assignability: a generated `| null` the reader does not admit. The
    // reverse - the reader guarding against a null the server never sends -
    // still assigns fine, so it is a note, not a failure. (The reader's
    // ChapMeta is nullable because ChapRow extends it and *that* use can be
    // null; the server is simply more precise about which is which.)
    if (g.hasNull !== h.hasNull) {
      record({iface: handName, field: name, kind: 'nullability',
        verdict: g.hasNull ? 'MISMATCH' : 'NOTE',
        expected: sig(h), got: sig(g),
        note: g.hasNull
          ? 'generated adds `| null` the reader does not handle'
          : 'generated never sends null where the reader allows it — assigns fine, the reader\'s guard is now dead code'});
    }

    // The underlying type, ignoring null/undefined (reported above). When the
    // two spell the same it is a nested type that drifted, not this field.
    if (!baseTypesAgree(g.type, h.type)) {
      const same = show(h.type) === show(g.type);
      record({iface: handName, field: name, kind: same ? 'nested' : 'type', verdict: 'MISMATCH',
        expected: sig(h), got: sig(g),
        note: same
          ? 'same spelling, different shape — the nested type drifted; fix it in its own section and this clears'
          : 'underlying type differs'});
    }
  }

  for (const [name, g] of genProps) {
    if (handProps.has(name)) continue;
    record({iface: handName, field: name, kind: 'extra', verdict: 'ADDITIVE',
      expected: 'not in the hand-written type',
      got: sig(g)});
  }
}

// ---------------------------------------------------------------------------
// Endpoint surface: every URL api.ts builds must exist in openapi.json.
// The URLs come out of the AST (string literals and template expressions), so
// a new call site in the reader shows up here without anyone updating a list.
// ---------------------------------------------------------------------------
const spec = JSON.parse(fs.readFileSync(SPEC, 'utf8'));

/** `/api/chapters/{ci}.m4a` and `/api/chapters/{id}.m4a` are the same route. */
const shape = (p) => p.replace(/\{[^}]*\}/g, '{}');

const specRoutes = new Set();
for (const [p, item] of Object.entries(spec.paths ?? {})) {
  for (const method of Object.keys(item)) {
    if (['get', 'post', 'put', 'patch', 'delete', 'head', 'options'].includes(method)) {
      specRoutes.add(`${method.toUpperCase()} ${shape(p)}`);
    }
  }
}

const apiSource = ts.createSourceFile(
  HAND_API, fs.readFileSync(HAND_API, 'utf8'), ts.ScriptTarget.ESNext, true);

/** Rebuild a template literal with `${expr}` left in place as a placeholder. */
function templateText(node) {
  if (ts.isStringLiteralLike(node)) return node.text;
  if (!ts.isTemplateExpression(node)) return null;
  let out = node.head.text;
  for (const span of node.templateSpans) {
    out += `\${${span.expression.getText(apiSource)}}`;
    out += span.literal.text;
  }
  return out;
}

/** The call this literal is an argument of, if it names the HTTP verb. */
function methodFor(node) {
  for (let n = node.parent; n; n = n.parent) {
    if (ts.isCallExpression(n)) {
      let callee = n.expression;
      if (ts.isPropertyAccessExpression(callee)) callee = callee.name;
      const name = ts.isIdentifier(callee) ? callee.text : '';
      if (name === 'post' || name === 'tell') return 'POST';
      if (name === 'get' || name === 'fetch') return 'GET';
    }
  }
  return 'GET';   // url builders (chapterAudioUrl & friends) are all fetched
}

const callSites = new Map();   // "METHOD /route" -> raw text as written
(function walk(node) {
  const text = templateText(node);
  if (text && text.includes('/api/')) {
    // Drop the trailing `?book=` query builder, then turn `${ci}` into `{ci}`.
    const route = text
      .replace(/\$\{qs\([^}]*\)\}$/, '')
      .replace(/\?.*$/, '')
      .replace(/\$\{([^}]*)\}/g, (_, e) => `{${e.trim()}}`);
    if (route.startsWith('/api/')) {
      callSites.set(`${methodFor(node)} ${shape(route)}`, `${methodFor(node)} ${route}`);
    }
  }
  node.forEachChild(walk);
})(apiSource);

const missingEndpoints = [];
const okEndpoints = [];
for (const [key, raw] of [...callSites].sort()) {
  (specRoutes.has(key) ? okEndpoints : missingEndpoints).push(raw);
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------
const mismatches = findings.filter((f) => f.verdict === 'MISMATCH');
const notes = findings.filter((f) => f.verdict === 'NOTE');
const additive = findings.filter((f) => f.verdict === 'ADDITIVE');
const summary =
  `DRIFT: ${mismatches.length} mismatches, ${additive.length} additive, ` +
  `${missingEndpoints.length} missing endpoints`;

const L = [];
const say = (s = '') => L.push(s);

say('# Drift: reader contract vs generated client');
say();
say(`Generated ${new Date().toISOString().replace('T', ' ').slice(0, 16)} by \`scripts/drift-check.mjs\`.`);
say();
say(`- hand-written contract: \`${HAND_TYPES}\` (read-only)`);
say(`- generated client: \`${path.relative(REPO, GEN_TYPES)}\``);
say(`- spec: \`${path.relative(REPO, SPEC)}\``);
say();
say('A **MISMATCH** breaks `generated → hand-written` assignability: the reader');
say('cannot compile against the generated client until it is resolved, on one side');
say('or the other. **NOTE** and **ADDITIVE** are safe and do not fail the build.');
say();

const byIface = new Map();
for (const f of findings) {
  if (!byIface.has(f.iface)) byIface.set(f.iface, []);
  byIface.get(f.iface).push(f);
}

say('## Types');
for (const [handName, genName] of PAIRS) {
  const rows = byIface.get(handName) ?? [];
  const bad = rows.filter((r) => r.verdict === 'MISMATCH').length;
  say();
  say(`### ${handName} → ${genName}${handName === genName ? '' : '  *(renamed)*'}`);
  say();
  if (rows.length === 0) { say('Identical. No drift.'); continue; }
  say(bad === 0 ? '_No mismatches._' : `_${bad} mismatch${bad === 1 ? '' : 'es'}._`);
  say();
  say('| verdict | field | kind | reader expects | generated has |');
  say('|---|---|---|---|---|');
  const rank = {MISMATCH: 0, NOTE: 1, ADDITIVE: 2};
  const sorted = [...rows].sort((a, b) => rank[a.verdict] - rank[b.verdict] ||
      a.field.localeCompare(b.field));
  for (const r of sorted) {
    const cell = (s) => String(s).replace(/\|/g, '\\|').replace(/\n/g, ' ');
    say(`| ${r.verdict} | \`${r.field}\` | ${r.kind} | \`${cell(r.expected)}\` | \`${cell(r.got)}\` |`);
  }
  const withNotes = sorted.filter((r) => r.note);
  if (withNotes.length) {
    say();
    for (const r of withNotes) say(`- \`${r.field}\` — ${r.note}`);
  }
}

say();
say('## Endpoints');
say();
say(`Every URL \`api.ts\` builds, checked against \`openapi.json\` (${specRoutes.size} routes).`);
say();
for (const e of okEndpoints) say(`- ok — \`${e}\``);
for (const e of missingEndpoints) say(`- **MISSING** — \`${e}\` is not in the spec`);

const byKind = new Map();
for (const m of mismatches) byKind.set(m.kind, (byKind.get(m.kind) ?? 0) + 1);
const optionKind = (byKind.get('optionality') ?? 0) +
  mismatches.filter((m) => m.kind === 'nullability' && m.got.includes('null')).length;

say();
say('## Summary');
say();
say(`- ${mismatches.length} mismatches (breaking)`);
for (const [k, n] of [...byKind].sort((a, b) => b[1] - a[1])) say(`  - ${n} × ${k}`);
say(`- ${notes.length} notes (safe: optional field dropped, or now always sent)`);
say(`- ${additive.length} additive fields (generated only)`);
say(`- ${missingEndpoints.length} missing endpoints`);
if (optionKind > 0) {
  say();
  say(`### One root cause behind ${optionKind} of them`);
  say();
  say('`optionality` and "generated adds `| null`" are the same Rust fact seen twice.');
  say('utoipa renders a struct field of type `Option<T>` as *both* `"type": ["T","null"]`');
  say('and absent from `required`, so hey-api emits `field?: T | null`. serde still');
  say('writes the key on every response (there is no `skip_serializing_if`), so at');
  say('runtime the field is present-and-possibly-null — exactly what the reader');
  say('already expects. The drift is in the description of the API, not its behaviour.');
  say();
  say('Two ways to clear it, both in narrator-rs — never in the reader:');
  say();
  say('1. Keep `Option<T>` and mark the field required in the schema, so the spec says');
  say('   present-and-nullable: `#[schema(required)]` on the field (utoipa ≥ 4).');
  say('2. Drop the `Option` where the value genuinely always exists.');
}
say();
say('```');
say(summary);
say('```');

const md = L.join('\n') + '\n';
fs.mkdirSync(CLIENT_DIR, {recursive: true});
fs.writeFileSync(REPORT, md);

// stdout gets the same thing, minus the markdown table scaffolding.
const pad = (s, n) => String(s).padEnd(n);
console.log('');
console.log('  drift-check — reader contract vs generated client');
console.log(`  hand-written : ${HAND_TYPES}`);
console.log(`  generated    : ${GEN_TYPES}`);
console.log('');
for (const [handName, genName] of PAIRS) {
  const rows = byIface.get(handName) ?? [];
  const bad = rows.filter((r) => r.verdict === 'MISMATCH').length;
  const label = handName === genName ? handName : `${handName} → ${genName}`;
  if (rows.length === 0) { console.log(`  ✓ ${label}`); continue; }
  console.log(`  ${bad ? '✗' : '·'} ${label}`);
  const rank = {MISMATCH: 0, NOTE: 1, ADDITIVE: 2};
  for (const r of [...rows].sort((a, b) => rank[a.verdict] - rank[b.verdict] ||
      a.field.localeCompare(b.field))) {
    console.log(`      ${pad(r.verdict, 9)} ${pad(r.field, 20)} ${pad(r.kind, 16)}`);
    console.log(`          reader   : ${r.expected}`);
    console.log(`          generated: ${r.got}`);
    if (r.note) console.log(`          → ${r.note}`);
  }
  console.log('');
}
console.log('  endpoints');
for (const e of okEndpoints) console.log(`      ok       ${e}`);
for (const e of missingEndpoints) console.log(`      MISSING  ${e}`);
console.log('');
if (mismatches.length) {
  console.log('  mismatches by kind');
  for (const [k, n] of [...byKind].sort((a, b) => b[1] - a[1])) {
    console.log(`      ${String(n).padStart(3)} × ${k}`);
  }
  console.log('');
}
console.log(`  report written to ${REPORT}`);
console.log('');
console.log(summary);

process.exit(mismatches.length > 0 || missingEndpoints.length > 0 ? 1 : 0);
