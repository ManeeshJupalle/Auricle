import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Dev mode: `npm run dev` serves the UI at :5173 and proxies API + WS to a
// running `auricle serve` daemon on :4820, so UI iteration never needs a
// Rust rebuild. Production embeds ui/dist into the binary via rust-embed.
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@shared': fileURLToPath(new URL('../shared', import.meta.url)) },
    // shared/ has no node_modules, so its React has to come from this app.
    dedupe: ['react'],
  },
  // Build output lives inside the server crate so `cargo package` can ship
  // the embedded UI (rust-embed cannot include files outside a crate).
  build: {
    outDir: '../crates/auricle-server/ui-dist',
    emptyOutDir: true,
  },
  // Inline (empty) PostCSS config: stops PostCSS from walking up past the
  // repo and loading an unrelated D:\postcss.config.js.
  css: { postcss: {} },
  server: {
    proxy: {
      // changeOrigin stays false, unlike the string shorthand: the daemon only
      // answers its own origin, so the browser's Host has to reach it intact.
      '/api': { target: 'http://127.0.0.1:4820', changeOrigin: false },
      '/ws': { target: 'ws://127.0.0.1:4820', ws: true },
    },
  },
  test: {
    environment: 'node',
  },
});
