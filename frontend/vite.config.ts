import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// During `vite dev` the frontend runs separately from the Rust backend.
// The backend always listens on 8080 (the production target). The dev
// server proxies API + auth calls there so client code can use relative
// URLs in both dev and prod.
export default defineConfig({
  plugins: [vue()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://localhost:8080',
      '/auth': 'http://localhost:8080',
      '/healthz': 'http://localhost:8080',
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
