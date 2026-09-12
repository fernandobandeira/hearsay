/**
 * Codegen config for the typed narrator client.
 *
 * Input is the checked-in contract (`openapi.json`, dumped by
 * `narrator --openapi`); output is the reader's own `web/src/client/` — a
 * fetch-based SDK plus the schema types, committed, and the only description of
 * this API the reader has. Paths are relative to this file (scripts/client/).
 */
export default {
  input: '../../openapi.json',
  output: {
    path: '../../web/src/client',
    postProcess: [],
  },
  plugins: [
    // Fetch-based runtime client (bundled inside @hey-api/openapi-ts since v0.73).
    '@hey-api/client-fetch',
    // Types for every schema + per-operation request/response types.
    {
      name: '@hey-api/typescript',
      enums: 'javascript',
    },
    // One exported function per operation (the default flat strategy — no
    // classes, so call sites read `books({...})`, not `Sdk.books({...})`).
    '@hey-api/sdk',
  ],
};
