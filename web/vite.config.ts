import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// The console is served by the gateway's admin listener, which mounts this
// single-page app at the site root (and, for convenience, under /ui/). Every
// API call is therefore same-origin and relative: /admin/...
//
// `base: './'` keeps the asset URLs in index.html relative, so one build works
// whether the directory is served from `/`, from `/ui/`, or from a file
// server; nothing is fetched from a CDN at runtime, by design: the deployment
// target has no reliable internet.
//
// The dev server proxies /admin to a locally running gateway so `npm run dev`
// can talk to a real backend without CORS.
const ADMIN_TARGET = 'http://127.0.0.1:8081'

export default defineConfig({
  base: './',
  plugins: [vue()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // The console is a handful of views; one bundle avoids a waterfall of
    // requests on a loopback listener with no HTTP/2.
    chunkSizeWarningLimit: 900,
  },
  server: {
    port: 5173,
    proxy: { '/admin': { target: ADMIN_TARGET, changeOrigin: true } },
  },
  preview: {
    port: 5199,
    proxy: { '/admin': { target: ADMIN_TARGET, changeOrigin: true } },
  },
})
