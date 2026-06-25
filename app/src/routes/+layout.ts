// SPA mode for the Tauri webview: render entirely on the client (no SSR), and
// disable prerendering so the fallback shell drives every route.
export const ssr = false;
export const prerender = false;
