import { defineConfig } from "astro/config";

// A static site: `astro build` writes dist/, which wrangler.jsonc serves
// as Workers static assets.
export default defineConfig({
  site: "https://osewa.dev",
  output: "static",
  // The page's CSS is a few kB: inline it rather than spend a
  // render-blocking request on it.
  build: { inlineStylesheets: "always" },
});
