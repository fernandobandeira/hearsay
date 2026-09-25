import {defineConfig} from 'vitest/config';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import {VitePWA} from 'vite-plugin-pwa';

// In production the Rust server hands this out at / (from /web, which the box
// mounts over the image's copy - see the reader deploy in AGENTS.md). In
// development vite serves it and proxies the API to a locally running server.
const API = process.env.NARRATOR_API || 'http://localhost:7870';

export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    VitePWA({
      registerType: 'autoUpdate',
      injectRegister: 'auto',
      includeAssets: ['icon-192.png', 'icon-512.png', 'apple-touch-icon.png'],
      manifest: {
        name: 'HearSay',
        short_name: 'HearSay',
        description: 'Listen to EPUBs, rendered a few chunks ahead of the playhead.',
        start_url: '/',
        scope: '/',
        display: 'standalone',
        background_color: '#0a0a0a',
        theme_color: '#0a0a0a',
        icons: [
          {src: '/icon-192.png', sizes: '192x192', type: 'image/png', purpose: 'any'},
          {src: '/icon-512.png', sizes: '512x512', type: 'image/png', purpose: 'any'},
          {src: '/icon-192.png', sizes: '192x192', type: 'image/png', purpose: 'maskable'},
          {src: '/icon-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable'},
        ],
      },
      workbox: {
        // The shell is precached by Workbox from the build manifest.
        //
        // Every runtime rule below skips a request carrying `x-narrator-store`.
        // That header marks a *deliberate* save - the download action writing an
        // entry with `Cache.put` from the page - and a strategy that also
        // handled it would be a second writer on one streaming body: on WebKit
        // that surfaces either as `FetchEvent.respondWith received an error` in
        // the page or, worse, as an entry that reads back fine all session and
        // is gone after the next launch. The rules exist to *serve* those
        // entries back; writing them is lib/offline.ts's job alone.
        globPatterns: ['**/*.{js,css,html,png,svg,woff2}'],
        navigateFallback: '/index.html',
        navigateFallbackDenylist: [/^\/api\//, /^\/healthz/],
        maximumFileSizeToCacheInBytes: 4 * 1024 * 1024,
        runtimeCaching: [
          {
            // The book's words: small, taken on first open, and the whole reason
            // the reader works with no network.
            urlPattern: ({url, request}) => request.headers.get('x-narrator-store') !== '1'
              && (url.pathname === '/api/book.json' || url.pathname.startsWith('/api/text/')),
            handler: 'StaleWhileRevalidate',
            options: {
              cacheName: 'narrator-text',
              // The reader is interchangeable between this server and the python
              // one, and that one serves the text bundle gzipped - so a stored
              // shard carries `Vary: Accept-Encoding`. A Vary-respecting match
              // can then miss it entirely, which offline means a book that is on
              // the device and will not open. Same reason as the audio rule.
              matchOptions: {ignoreVary: true},
              expiration: {maxEntries: 400},
            },
          },
          {
            // One chapter's words. This is what first paint waits on - never the
            // 17 MB bundle - so it needs an answer with no network too: the copy
            // the last visit left behind, while the network is asked for a fresh
            // one. Keyed by ?book=, so it can never answer for another book.
            //
            // Its own cache, not the text one. Workbox's expiration records are
            // keyed by cache *name*, so sharing one meant sharing one 400-entry
            // budget: on a 1433-chapter novel every chapter visited added an
            // entry until the shards - downloaded first, therefore oldest - were
            // evicted out from under the offline reader.
            urlPattern: ({url, request}) => request.headers.get('x-narrator-store') !== '1'
              && /^\/api\/chapter\/\d+$/.test(url.pathname),
            handler: 'NetworkFirst',
            options: {
              cacheName: 'narrator-chapter',
              matchOptions: {ignoreVary: true},
              networkTimeoutSeconds: 6,
              expiration: {maxEntries: 400},
              cacheableResponse: {statuses: [200]},
            },
          },
          {
            // Chapter audio and its chunk->time manifest. CacheFirst, and the
            // entries are put there deliberately by the download action; this
            // rule is what serves them back (Range included - Workbox's
            // rangeRequests plugin slices the cached body).
            //
            // **Read-only.** It used to write as well: every manifest a
            // *streamed* chapter asked for went into this cache on the way
            // past, and CacheFirst then answered from that copy for good - so a
            // chapter re-rendered or re-packed later kept the old chunk->time
            // map, and the page followed the wrong words. Only lib/offline.ts
            // writes here now (`cacheWillUpdate` refusing everything), and a
            // manifest is only served while its audio is here too, so one left
            // behind by a download that never finished cannot answer for a
            // chapter that is streaming.
            urlPattern: ({url, request}) => request.headers.get('x-narrator-store') !== '1'
              && /^\/api\/chapters\/\d+\.(m4a|json)$/.test(url.pathname),
            handler: 'CacheFirst',
            options: {
              cacheName: 'narrator-audio',
              matchOptions: {ignoreVary: true},
              rangeRequests: true,
              plugins: [{
                cacheWillUpdate: async () => null,
                cachedResponseWillBeUsed: async ({cacheName, request, cachedResponse}) => {
                  if (!cachedResponse || !/\.json$/.test(new URL(request.url).pathname))
                    return cachedResponse;
                  const audio = request.url.replace(/\.json(\?|$)/, '.m4a$1');
                  // Serialised into the service worker, where `caches` exists;
                  // this config is typechecked without the DOM lib.
                  const store = (globalThis as unknown as {caches: {open: (n: string) =>
                    Promise<{match: (u: string, o: object) => Promise<unknown>}>}}).caches;
                  const held = await (await store.open(cacheName))
                    .match(audio, {ignoreVary: true});
                  return held ? cachedResponse : null;
                },
              }],
            },
          },
        ],
      },
      devOptions: {enabled: false},
    }),
  ],
  resolve: {alias: {'@': new URL('./src', import.meta.url).pathname}},
  server: {
    port: 5173,
    proxy: {
      '/api': {target: API, changeOrigin: true},
      '/healthz': {target: API, changeOrigin: true},
    },
  },
  build: {outDir: 'dist', emptyOutDir: true, sourcemap: false},
  test: {environment: 'node', include: ['src/**/*.test.ts']},
});
