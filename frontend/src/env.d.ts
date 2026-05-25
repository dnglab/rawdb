/// <reference types="vite/client" />

declare module '*.vue' {
  import type { DefineComponent } from 'vue';
  const component: DefineComponent<object, object, unknown>;
  export default component;
}

// `*.md` imports are pre-rendered to an HTML string at build time by the
// `markdownPlugin` in vite.config.ts.
declare module '*.md' {
  const html: string;
  export default html;
}
