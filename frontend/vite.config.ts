import { defineConfig, type Plugin } from 'vite';
import vue from '@vitejs/plugin-vue';
import { marked } from 'marked';
import { readFileSync } from 'node:fs';

// Compiles `*.md` imports to a default-exported HTML string at build time
// so static content (e.g. the License info panel on SetDetail) ships as
// pre-rendered HTML — no markdown parser in the client bundle, no
// runtime parse cost. The TS module declaration lives in env.d.ts.
function markdownPlugin(): Plugin {
  return {
    name: 'rawdb-markdown',
    transform(_code, id) {
      if (!id.endsWith('.md')) return null;
      const src = readFileSync(id, 'utf8');
      const html = marked.parse(src, { async: false }) as string;
      return { code: `export default ${JSON.stringify(html)};`, map: null };
    },
  };
}

// During `vite dev` the frontend runs separately from the Rust backend.
// The backend always listens on 8080 (the production target). The dev
// server proxies API + auth calls there so client code can use relative
// URLs in both dev and prod.
export default defineConfig({
  plugins: [vue(), markdownPlugin()],
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
