import { defineConfig } from "astro/config";
import sitemap from "@astrojs/sitemap";

// A static site: `astro build` writes dist/, which wrangler.jsonc serves
// as Workers static assets.
export default defineConfig({
  site: "https://osewa.dev",
  output: "static",
  // The page's CSS is a few kB: inline it rather than spend a
  // render-blocking request on it.
  // "file" writes paper.html, which Workers assets serve at /paper, the
  // canonical URL; the default paper/index.html would 307 to /paper/.
  build: { inlineStylesheets: "always", format: "file" },
  // Pages swap client-side (Base.astro's ClientRouter); fetching on hover
  // makes the swap land at once.
  prefetch: { prefetchAll: true, defaultStrategy: "hover" },
  integrations: [sitemap()],
});
