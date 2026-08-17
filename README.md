# iris

A camera for coding agents. One fast command or MCP tool call produces one trustworthy image.

```
iris example.com                        # 1440×900 @2x → example.com.png
iris --full --dark tailwindcss.com      # full page, dark color scheme
iris --size iphone stripe.com           # 390×844 @3x with a mobile UA
iris --selector '#hero' --padding 24 app.dev # first matching element, tightly framed
iris -o shots/ a.com b.com c.com        # batch, captured concurrently
cat urls.txt | iris - -o shots/         # batch from stdin (# comments ok)
iris -o hero.jpg --wait-for 'h1' app.dev
iris --selector 'h1' --json app.dev      # machine-readable JSON Lines
```

`iris --full bridger.to` →

![Full-page capture of bridger.to, taken by iris](.github/demo.png)

## Install

```
curl -fsSL https://raw.githubusercontent.com/brijr/iris/main/install.sh | sh
```

Or with a Rust toolchain:

```
cargo install iris-screenshot
```

The crates.io package is named `iris-screenshot`; the installed command is `iris`.

The only runtime dependency is an installed Chrome-family browser (Chrome, Chromium, Edge, or Brave).
Building from source requires Rust 1.88 or newer.

## Give your coding agent eyes

Iris includes a local stdio MCP server in the same binary. Add it to Codex:

```
codex mcp add iris -- iris mcp
```

Or use the equivalent configuration in another MCP client:

```json
{
  "mcpServers": {
    "iris": {
      "command": "iris",
      "args": ["mcp"]
    }
  }
}
```

The server exposes one tool, `capture`, which returns the image inline with structured metadata. It writes nothing by default; pass `output` when the agent also needs a file.

```json
{
  "url": "localhost:3000",
  "selector": "#pricing-card",
  "padding": 24,
  "size": "desktop",
  "dark": false,
  "format": "png",
  "timeout_seconds": 30,
  "output": "/tmp/pricing-card.png"
}
```

Bare localhost, `.localhost`, and loopback addresses use HTTP automatically. Other bare hosts use HTTPS. Run `iris mcp --help` to select a Chrome binary for the server.

## What it does for you

- Renders with your real installed Chrome, driven over the DevTools Protocol
- Waits for fonts, image loads, entrance animations, and — on `--full` — scroll-triggers lazy-loaded content before capturing
- Captures the first matching element with `--selector`, automatically scrolling it into view and settling newly visible content before framing it
- Serves the same capture engine to coding agents with `iris mcp`, returning pixels inline instead of making the agent locate a file
- Retina `@2x` output by default; full pages taller than Chrome's ~16k px render limit fall back to `@1x` automatically (the report tells you which you got)
- Image format from the `-o` extension or `--format`: `png` (default), `jpg`, `webp` (JPEG/WebP encode at quality 90)
- One browser process, concurrent tabs; a failed URL prints `✗` and never kills the batch (exit code 1 if anything failed)
- Batch filenames derive from the URL (`example.com-pricing.png`); collisions get `-2`, `-3` suffixes
- `--json` writes one JSON object per completed capture to stdout, in concurrent completion order; capture failures are JSON too and still produce exit code 1

Element capture is intentionally CSS-selector based: Iris captures the first match in document order. `--selector` conflicts with `--full`; `--padding` requires it. Cross-origin iframe contents and capturing every match are not supported.

```json
{"status":"ok","url":"https://example.com/","output":"/absolute/example.com.png","mode":"element","selector":"h1","padding":24,"css_width":180,"css_height":72,"scale":2.0,"format":"png","bytes":14231}
```

## Flags

```
-o, --out <PATH>       output file (single URL) or directory (batch)
-s, --size <SIZE>      WxH, or desktop (1440x900@2x) | iphone (390x844@3x) | ipad (1024x1366@2x)
    --full             capture the full page height
    --selector <CSS>   capture the first element matching a CSS selector
    --padding <PX>     nonnegative CSS-pixel padding around a selected element
    --dark             emulate prefers-color-scheme: dark
    --format <FMT>     png | jpg | webp (a recognized --out extension wins)
    --wait <MS>        extra settle delay after smart waiting
    --wait-for <CSS>   wait until a selector exists before capturing
    --scale <N>        device scale factor (overrides the preset's)
    --jobs <N>         concurrent captures (default: min(4, URLs))
    --timeout <SECS>   per-page budget (default: 30)
    --chrome <PATH>    browser binary (auto-detected; also via $CHROME)
    --json             emit one JSON object per completed capture
```

`iris mcp [--chrome <PATH>]` serves the single-image `capture` tool over stdio. Batch capture, browser interaction scripting, diffs, and review workflows remain outside Iris.

## License

[MIT](LICENSE)
