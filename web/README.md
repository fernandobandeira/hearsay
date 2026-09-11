# web/ — the reader

The Vite + React + Tailwind PWA the server hands out at `/`. It lives here now;
the build is **not** vendored (it used to be, copied out of the python repo — see
`AGENTS.md`).

```bash
npm ci
npm run dev        # vite on :5173, proxying /api to NARRATOR_API (default :7870)
npm run build      # tsc -b && vite build → dist/
npm test           # vitest
```

Two images are built from this directory:

- **`ghcr.io/fernandobandeira/hearsay`** — the server. Its Dockerfile's `web`
  stage builds `dist/` and bakes it into the image at `/web`, so the server image
  is self-contained.
- **`ghcr.io/fernandobandeira/hearsay-web`** — `web/Dockerfile`: the same `dist/`
  and nothing else (`FROM scratch`, contents at `/dist`). The box's
  `hearsay-web-update.timer` pulls it and lays the files into `/home/ubuntu/web`,
  which the server mounts over its baked-in copy. That is what makes a UI change
  a 10-minute file swap instead of a Rust rebuild.

`placeholder/` is the page the server falls back to when there is no build at
all. `src/client/` is **generated** — the typed client `scripts/gen-client.sh`
builds from the server's OpenAPI document, which is where every request and
response shape in `src/lib/api.ts` comes from. Do not hand-edit it; CI
regenerates and diffs.
