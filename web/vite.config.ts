import {defineConfig} from 'vitest/config';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import {VitePWA} from 'vite-plugin-pwa';

// The reader is served by the FastAPI container in production; in development
// vite serves it and proxies the API to a locally running narrator.
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
        globPatterns: ['**/*.{js,css,html,png,svg,woff2}'],
        navigateFallback: '/index.html',
        navigateFallbackDenylist: [/^\/api\//, /^\/healthz/],
        maximumFileSizeToCacheInBytes: 4 * 1024 * 1024,
        runtimeCaching: [
          {
            // The book's words: small, taken on first open, and the whole reason
            // the reader works with no network.
            urlPattern: ({url}) => url.pathname === '/api/book.json'
              || url.pathname.startsWith('/api/text/'),
            handler: 'StaleWhileRevalidate',
            options: {cacheName: 'narrator-text', expiration: {maxEntries: 400}},
          },
          {
            // One chapter's words. This is what first paint waits on - never the
            // 17 MB bundle - so it needs an answer with no network too: the copy
            // the last visit left behind, while the network is asked for a fresh
            // one. Keyed by ?book=, so it can never answer for another book.
            urlPattern: ({url}) => /^\/api\/chapter\/\d+$/.test(url.pathname),
            handler: 'NetworkFirst',
            options: {
              cacheName: 'narrator-text',
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
            urlPattern: ({url}) => /^\/api\/chapters\/\d+\.(m4a|json)$/.test(url.pathname),
            handler: 'CacheFirst',
            options: {
              cacheName: 'narrator-audio',
              matchOptions: {ignoreVary: true},
              rangeRequests: true,
              cacheableResponse: {statuses: [200]},
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
