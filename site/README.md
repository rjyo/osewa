# osewa.dev

The home page: a static Astro site, deployed as Cloudflare Workers static
assets.

    bun install
    bun run dev       # http://localhost:4321
    bun run build     # astro check + astro build into dist/
    bun run preview   # build, then serve dist/ with wrangler dev
    bun run deploy    # build, then wrangler deploy

`wrangler.jsonc` serves `dist/` as the Worker `osewa-dev`, on the custom
domain `osewa.dev`, which needs the osewa.dev zone in the Cloudflare
account `wrangler login` uses.

The benchmark figures in `src/data/bench.js` and the tables in
`src/pages/index.astro` come from `../README.md` and `../bench/RESULTS.md`;
update them together.

Fonts are self-hosted in `src/fonts/`: Inter Tight and JetBrains Mono,
subset to Latin-1 plus the characters the pages use, weights trimmed to
the range in use. If new copy needs a character they lack, rebuild them
with `scripts/build-fonts.py` (instructions at its top).
