# snap

Screenshots of live websites. Minimal interface, powerful engine.

```
snap example.com                        # 1440×900 @2x → example.com.png
snap --full --dark tailwindcss.com      # full page, dark color scheme
snap --size iphone stripe.com           # 390×844 @3x with a mobile UA
snap -o shots/ a.com b.com c.com        # batch, captured concurrently
cat urls.txt | snap - -o shots/         # batch from stdin (# comments ok)
snap -o hero.jpg --wait-for 'h1' app.dev
```

## Install

```
curl -fsSL https://raw.githubusercontent.com/brijr/snap/main/install.sh | sh
```

Or with a Rust toolchain:

```
cargo install --git https://github.com/brijr/snap
```

The only runtime dependency is an installed Chrome-family browser (Chrome, Chromium, Edge, or Brave).

## What it does for you

- Renders with your real installed Chrome, driven over the DevTools Protocol
- Waits for fonts, paint frames, and — on `--full` — scroll-triggers lazy-loaded content before capturing
- Retina `@2x` output by default; full pages taller than Chrome's ~16k px render limit fall back to `@1x` automatically (the report tells you which you got)
- Image format from the `-o` extension or `--format`: `png` (default), `jpg`, `webp` (JPEG/WebP encode at quality 90)
- One browser process, concurrent tabs; a failed URL prints `✗` and never kills the batch (exit code 1 if anything failed)
- Batch filenames derive from the URL (`example.com-pricing.png`); collisions get `-2`, `-3` suffixes

## Flags

```
-o, --out <PATH>       output file (single URL) or directory (batch)
-s, --size <SIZE>      WxH, or desktop (1440x900@2x) | iphone (390x844@3x) | ipad (1024x1366@2x)
    --full             capture the full page height
    --dark             emulate prefers-color-scheme: dark
    --format <FMT>     png | jpg | webp (a recognized --out extension wins)
    --wait <MS>        extra settle delay after smart waiting
    --wait-for <CSS>   wait until a selector exists before capturing
    --scale <N>        device scale factor (overrides the preset's)
    --jobs <N>         concurrent captures (default: min(4, URLs))
    --timeout <SECS>   per-page budget (default: 30)
    --chrome <PATH>    browser binary (auto-detected; also via $CHROME)
```

## License

[MIT](LICENSE)
