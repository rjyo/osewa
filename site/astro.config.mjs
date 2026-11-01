import { defineConfig } from "astro/config";

// A static site: `astro build` writes dist/, which wrangler.jsonc serves
// as Workers static assets.
export default defineConfig({
  site: "https://osewa.dev",
  output: "static",
});
