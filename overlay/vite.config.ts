import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Inline empty PostCSS config: a stray D:\postcss.config.js outside the
// repo is picked up by PostCSS's upward search on this machine (same
// machine quirk recorded in docs/PHASE5_UI_REPORT.md).
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@shared': fileURLToPath(new URL('../shared', import.meta.url)) },
    // shared/ has no node_modules, so its React has to come from this app.
    dedupe: ['react'],
  },
  css: { postcss: {} },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: 'chrome110',
  },
});
