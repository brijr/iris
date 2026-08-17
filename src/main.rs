mod capture;
mod mcp;

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use capture::{CaptureMode, Format, Opts, Session, normalize_url, parse_viewport, success_report};
use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use url::Url;

/// Screenshot live websites. Minimal interface, powerful engine:
/// smart waiting, lazy-load handling, retina output, concurrent capture.
#[derive(Parser)]
#[command(
    name = "iris",
    version,
    arg_required_else_help = true,
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    after_help = "\
Examples:
  iris example.com                      1440\u{d7}900 @2x \u{2192} example.com.png
  iris --full --dark tailwindcss.com    full page, dark color scheme
  iris --selector '#hero' --padding 24 app.dev
  iris mcp                              serve the capture tool over stdio
  cat urls.txt | iris - -o shots/       concurrent batch from stdin"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    capture: CaptureArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Serve Iris as a local Model Context Protocol camera
    Mcp(mcp::McpArgs),
}

#[derive(Args)]
struct CaptureArgs {
    /// URLs to capture; `-` reads newline-separated URLs from stdin
    #[arg(required = true, value_name = "URL")]
    urls: Vec<String>,

    /// Output file (single URL) or directory (batch) [default: ./<host>-<path>.png]
    #[arg(short, long, value_name = "PATH")]
    out: Option<PathBuf>,

    /// Viewport: WxH, or a preset: desktop (1440x900@2x), iphone (390x844@3x), ipad (1024x1366@2x)
    #[arg(short, long, default_value = "desktop", value_name = "SIZE")]
    size: String,

    /// Capture the full page height
    #[arg(long)]
    full: bool,

    /// Capture the first element matching this CSS selector
    #[arg(long, value_name = "CSS", conflicts_with = "full")]
    selector: Option<String>,

    /// Uniform CSS-pixel padding around a selected element
    #[arg(long, value_name = "PX", requires = "selector")]
    padding: Option<u32>,

    /// Emulate prefers-color-scheme: dark
    #[arg(long)]
    dark: bool,

    /// Image format (a recognized --out file extension wins) [default: png]
    #[arg(long, value_parser = ["png", "jpg", "jpeg", "webp"], value_name = "FMT")]
    format: Option<String>,

    /// Extra settle delay in ms after smart waiting
    #[arg(long, default_value_t = 0, value_name = "MS")]
    wait: u64,

    /// Wait until this CSS selector exists before capturing
    #[arg(long, value_name = "CSS")]
    wait_for: Option<String>,

    /// Device scale factor (overrides the preset's)
    #[arg(long, value_name = "N")]
    scale: Option<f64>,

    /// Concurrent captures [default: min(4, number of URLs)]
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,

    /// Per-page timeout in seconds
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    timeout: u64,

    /// Chrome/Chromium binary [auto-detected]
    #[arg(long, env = "CHROME", value_name = "PATH")]
    chrome: Option<PathBuf>,

    /// Emit one JSON object per completed capture
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Mcp(args)) = cli.command {
        return mcp::run(args).await;
    }
    run_capture(cli.capture).await
}

async fn run_capture(cli: CaptureArgs) -> Result<()> {
    let urls = collect_urls(&cli.urls)?;
    let viewport = parse_viewport(&cli.size, cli.scale)?;
    let mode = capture_mode(&cli);
    let flag_format = cli.format.as_deref().and_then(Format::from_ext);
    let (targets, format) = resolve_outputs(&urls, cli.out.as_deref(), flag_format).await?;
    let jobs = cli.jobs.unwrap_or_else(|| urls.len().min(4)).max(1);

    let opts = Arc::new(Opts {
        viewport,
        mode: mode.clone(),
        dark: cli.dark,
        wait_ms: cli.wait,
        wait_for: cli.wait_for,
        timeout: Duration::from_secs(cli.timeout.max(1)),
        format,
    });

    let session = Arc::new(Session::launch(cli.chrome, viewport).await?);

    let mut failed = 0usize;
    let mut stream = futures::stream::iter(targets)
        .map({
            let session = Arc::clone(&session);
            let opts = Arc::clone(&opts);
            move |(url, path): (Url, PathBuf)| {
                let session = Arc::clone(&session);
                let opts = Arc::clone(&opts);
                async move {
                    let result = match session.capture(url.as_str(), &opts).await {
                        Ok(image) => image.write_to(&path).await.map(|()| image.shot),
                        Err(error) => Err(error),
                    };
                    (url, path, result)
                }
            }
        })
        .buffer_unordered(jobs);

    while let Some((url, path, result)) = stream.next().await {
        match result {
            Ok(shot) => {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string(&success_report(
                            url.as_str(),
                            Some(&path),
                            &mode,
                            format,
                            &shot,
                        ))?
                    );
                } else {
                    println!(
                        "\u{2713} {} \u{2014} {}\u{d7}{} @{}x, {}",
                        path.display(),
                        shot.width,
                        shot.height,
                        shot.scale,
                        human_size(shot.bytes),
                    );
                }
            }
            Err(err) => {
                failed += 1;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string(&capture::error_report(
                            url.as_str(),
                            Some(&path),
                            &mode,
                            format!("{err:#}"),
                        ))?
                    );
                } else {
                    eprintln!("\u{2717} {url} \u{2014} {err:#}");
                }
            }
        }
    }

    drop(stream);
    if let Ok(session) = Arc::try_unwrap(session) {
        session.close().await;
    }

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn capture_mode(cli: &CaptureArgs) -> CaptureMode {
    if let Some(selector) = &cli.selector {
        CaptureMode::Element {
            selector: selector.clone(),
            padding: cli.padding.unwrap_or(0),
        }
    } else if cli.full {
        CaptureMode::FullPage
    } else {
        CaptureMode::Viewport
    }
}

fn collect_urls(args: &[String]) -> Result<Vec<Url>> {
    let mut raw = Vec::new();
    for arg in args {
        if arg == "-" {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("failed to read URLs from stdin")?;
            raw.extend(
                input
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(String::from),
            );
        } else {
            raw.push(arg.clone());
        }
    }
    if raw.is_empty() {
        bail!("no URLs given");
    }
    raw.iter().map(|value| normalize_url(value)).collect()
}

/// Pair each URL with its output path and settle the image format. Single URL +
/// `--out file.ext` writes that exact file (its extension beats --format); anything
/// else treats --out (default `.`) as a directory of derived names in the chosen format.
async fn resolve_outputs(
    urls: &[Url],
    out: Option<&std::path::Path>,
    flag_format: Option<Format>,
) -> Result<(Vec<(Url, PathBuf)>, Format)> {
    if let (1, Some(path)) = (urls.len(), out)
        && let Some(ext) = path.extension()
        && let Some(format) = Format::from_ext(&ext.to_string_lossy())
    {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(dir).await?;
        }
        return Ok((vec![(urls[0].clone(), path.to_path_buf())], format));
    }

    let format = flag_format.unwrap_or(Format::Png);
    let dir = out.unwrap_or_else(|| std::path::Path::new("."));
    tokio::fs::create_dir_all(dir).await?;
    let mut seen: HashMap<String, u32> = HashMap::new();
    let targets = urls
        .iter()
        .map(|url| {
            let mut name = derived_name(url);
            let n = seen.entry(name.clone()).or_insert(0);
            *n += 1;
            if *n > 1 {
                name = format!("{name}-{n}");
            }
            (url.clone(), dir.join(format!("{name}.{}", format.ext())))
        })
        .collect();
    Ok((targets, format))
}

/// `https://example.com/pricing/` -> `example.com-pricing`
fn derived_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or("page");
    let path = url.path().trim_matches('/').replace('/', "-");
    let name = if path.is_empty() {
        host.to_string()
    } else {
        format!("{host}-{path}")
    };
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;
    use crate::capture::Shot;

    #[test]
    fn selector_conflicts_with_full_page() {
        let error = Cli::try_parse_from(["iris", "example.com", "--selector", "h1", "--full"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn padding_requires_a_selector_and_rejects_negative_values() {
        let missing_selector = Cli::try_parse_from(["iris", "example.com", "--padding", "12"])
            .err()
            .unwrap();
        assert_eq!(missing_selector.kind(), ErrorKind::MissingRequiredArgument);

        let negative =
            Cli::try_parse_from(["iris", "example.com", "--selector", "h1", "--padding=-1"])
                .err()
                .unwrap();
        assert_eq!(negative.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn capture_mode_is_constructed_once_from_validated_cli() {
        let element = Cli::try_parse_from([
            "iris",
            "example.com",
            "--selector",
            "main > h1",
            "--padding",
            "24",
        ])
        .unwrap();
        assert_eq!(
            capture_mode(&element.capture),
            CaptureMode::Element {
                selector: "main > h1".into(),
                padding: 24,
            }
        );

        let full = Cli::try_parse_from(["iris", "example.com", "--full"]).unwrap();
        assert_eq!(capture_mode(&full.capture), CaptureMode::FullPage);

        let viewport = Cli::try_parse_from(["iris", "example.com"]).unwrap();
        assert_eq!(capture_mode(&viewport.capture), CaptureMode::Viewport);
    }

    #[test]
    fn mcp_subcommand_does_not_require_a_url() {
        let cli = Cli::try_parse_from(["iris", "mcp"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Mcp(_))));

        let conflict = Cli::try_parse_from(["iris", "mcp", "--dark"])
            .err()
            .unwrap();
        assert_eq!(conflict.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn json_reports_have_stable_typed_fields() {
        let mode = CaptureMode::Element {
            selector: "h1".into(),
            padding: 24,
        };
        let shot = Shot {
            width: 180,
            height: 72,
            scale: 2.0,
            bytes: 14_231,
        };
        let success = serde_json::to_string(&success_report(
            "https://example.com/",
            Some(std::path::Path::new("/tmp/example.png")),
            &mode,
            Format::Png,
            &shot,
        ))
        .unwrap();
        assert_eq!(
            success,
            r#"{"status":"ok","url":"https://example.com/","output":"/tmp/example.png","mode":"element","selector":"h1","padding":24,"css_width":180,"css_height":72,"scale":2.0,"format":"png","bytes":14231}"#
        );

        let failure = serde_json::to_string(&capture::error_report(
            "https://example.com/",
            Some(std::path::Path::new("/tmp/example.png")),
            &mode,
            "selector never appeared: h1".into(),
        ))
        .unwrap();
        assert_eq!(
            failure,
            r#"{"status":"error","url":"https://example.com/","output":"/tmp/example.png","mode":"element","selector":"h1","padding":24,"error":"selector never appeared: h1"}"#
        );
    }
}
