# web/ — the reader's slot

A placeholder page, not the reader. The real one is the Vite + React + Tailwind
app in `~/git/narrator/web`, which still calls the python server's hand-written
`src/lib/api.ts`. Porting it onto the generated client
(`scripts/gen-client.sh` → `client/`) is the integration step; when that
happens this directory holds the app and the Dockerfile's `web` stage builds it
with `npm ci && npm run build` instead of copying the placeholder through.

`NARRATOR_WEB` points the server at a built copy somewhere else.
