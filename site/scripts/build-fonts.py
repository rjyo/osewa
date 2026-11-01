"""Self-hosted fonts for osewa.dev: subset, weight-trimmed WOFF2."""
import re, html, glob, os, sys
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer
from fontTools import subset

# Usage: python3 scripts/build-fonts.py <dir with the two variable TTFs>
# Needs fonttools and brotli (pip install fonttools brotli) and a built
# dist/ (bun run build): the subset keeps every character the pages use.
# Sources, both OFL: https://github.com/google/fonts/tree/main/ofl/intertight
# and .../ofl/jetbrainsmono ("InterTight[wght].ttf", "JetBrainsMono[wght].ttf").
# The fallback metrics it prints go in src/styles/global.css.
srcdir = sys.argv[1]
dist, out = "dist", "src/fonts"
text = ""
for f in glob.glob(dist + "/*.html"):
    s = open(f).read()
    s = re.sub(r"<(script|style)\b.*?</\1>", "", s, flags=re.S)
    s = re.sub(r"<[^>]+>", "", s)
    text += html.unescape(s)
used = {ord(c) for c in text if ord(c) >= 0x20}
# Room to edit copy without re-subsetting: all of Latin-1, common punctuation
# and arrows. Anything else the page uses is picked up from the build.
base = set(range(0x20, 0x7F)) | set(range(0xA0, 0x100)) | set(range(0x2010, 0x2028)) \
     | set(range(0x2030, 0x203B)) | {0x20AC, 0x2122, 0x2190, 0x2191, 0x2192, 0x2193, 0x2212, 0x00D7}
cps = used | base

def build(src, name, wmin, wmax):
    f = TTFont(src)
    f = instancer.instantiateVariableFont(f, {"wght": (wmin, wmax)})
    import io
    buf = io.BytesIO(); f.save(buf); buf.seek(0); f = TTFont(buf)
    have = set(f.getBestCmap())
    keep = sorted(cps & have)
    missing = sorted(c for c in used - have if c > 0x7F)
    opts = subset.Options()
    opts.flavor = "woff2"
    opts.layout_features = ["kern", "liga", "calt", "ccmp", "locl", "mark", "mkmk", "tnum", "case"]
    opts.name_IDs = [1, 2, 4, 6]
    opts.notdef_outline = True
    opts.hinting = False
    s = subset.Subsetter(opts)
    s.populate(unicodes=keep)
    s.subset(f)
    path = f"{out}/{name}.woff2"
    f.flavor = "woff2"
    f.save(path)
    print(name, os.path.getsize(path), "bytes,", len(keep), "glyphs; not in font:", "".join(map(chr, missing)))

def metrics(src, ref, sample="The quick brown fox jumps over the lazy dog 0123456789"):
    def width(font):
        cmap, hmtx, upm = font.getBestCmap(), font["hmtx"], font["head"].unitsPerEm
        return sum(hmtx[cmap[ord(c)]][0] for c in sample) / upm
    f = TTFont(src)
    if "fvar" in f:
        f = instancer.instantiateVariableFont(f, {"wght": 400})
    r = TTFont(ref, fontNumber=0)
    adj = width(f) / width(r)
    upm, hh = f["head"].unitsPerEm, f["hhea"]
    return dict(size=f"{adj*100:.2f}%", ascent=f"{hh.ascent/upm/adj*100:.2f}%",
                descent=f"{-hh.descent/upm/adj*100:.2f}%", gap=f"{hh.lineGap/upm/adj*100:.2f}%")

build(f"{srcdir}/InterTight[wght].ttf", "inter-tight", 400, 700)
build(f"{srcdir}/JetBrainsMono[wght].ttf", "jetbrains-mono", 400, 600)
print("sans/Arial", metrics(f"{srcdir}/InterTight[wght].ttf", "/System/Library/Fonts/Supplemental/Arial.ttf"))
print("mono/Menlo", metrics(f"{srcdir}/JetBrainsMono[wght].ttf", "/System/Library/Fonts/Menlo.ttc"))
print("mono/CourierNew", metrics(f"{srcdir}/JetBrainsMono[wght].ttf", "/System/Library/Fonts/Supplemental/Courier New.ttf"))
