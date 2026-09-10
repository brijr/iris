mod capture;
mod mcp;
mod workflow;

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

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve Iris as a local Model Context Protocol camera
    Mcp(mcp::McpArgs),
    /// Save a signed-in browser session for later reuse
    Login(LoginArgs),
}

#[derive(Debug, Args)]
struct LoginArgs {
    /// Session name to save
    #[arg(long, value_name = "NAME")]
    session: String,

    /// URL to open for sign-in
    #[arg(value_name = "URL")]
    url: String,

    /// Chrome/Chromium binary [auto-detected]
    #[arg(long, env = "CHROME", value_name = "PATH")]
    chrome: Option<PathBuf>,
}

#[derive(Args)]
struct CaptureArgs {
    /// URLs to capture; `-` reads newline-separated URLs from stdin
    #[arg(required_unless_present = "workflow", value_name = "URL")]
    urls: Vec<String>,

    /// Run recipe files instead of flags: URL, viewport, steps, redactions,
    /// annotations, selector, session, and output per file (repeatable).
    /// Recipes carry their own capture spec, including timeout; --jobs,
    /// --chrome, and --json still apply. Use --dry-run to print the plan.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = [
            "urls", "out", "size", "full", "selector", "padding", "dark", "light",
            "color_scheme", "format", "wait", "wait_for", "click", "fill", "hover",
            "press", "mask", "mask_blur", "mask_patterns", "point", "highlight",
            "dim", "scale", "session",
        ]
    )]
    workflow: Vec<PathBuf>,

    /// Print the resolved workflow plan as JSON without launching Chrome
    #[arg(long, requires = "workflow")]
    dry_run: bool,

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

    /// Emulate prefers-color-scheme: dark (same as --color-scheme dark)
    #[arg(long, conflicts_with = "light", conflicts_with = "color_scheme")]
    dark: bool,

    /// Emulate prefers-color-scheme: light (same as --color-scheme light)
    #[arg(long, conflicts_with = "dark", conflicts_with = "color_scheme")]
    light: bool,

    /// Force a color scheme: light, dark, or system [default: system]
    #[arg(long, value_name = "SCHEME")]
    color_scheme: Option<capture::ColorScheme>,

    /// Image format (a recognized --out file extension wins) [default: png]
    #[arg(long, value_parser = ["png", "jpg", "jpeg", "webp"], value_name = "FMT")]
    format: Option<String>,

    /// Extra settle delay in ms after smart waiting
    #[arg(long, default_value_t = 0, value_name = "MS")]
    wait: u64,

    /// Wait until this CSS selector exists before capturing
    #[arg(long, value_name = "CSS")]
    wait_for: Option<String>,

    /// Click the first match of SELECTOR (or `text=...` for the deepest
    /// visible element containing the text) before capturing (repeatable,
    /// flag order kept). `--open` is the same action, for menus and drawers.
    #[arg(long, visible_alias = "open", value_name = "TARGET")]
    click: Vec<String>,

    /// Fill an input with text: --fill SELECTOR TEXT (repeatable, flag order kept)
    #[arg(long = "fill", value_names = ["SELECTOR", "TEXT"], num_args = 2)]
    fill: Vec<String>,

    /// Hover the first match of this CSS selector before capturing (repeatable)
    #[arg(long, value_name = "CSS")]
    hover: Vec<String>,

    /// Press a keyboard key before capturing, e.g. Enter or Escape (repeatable)
    #[arg(long, value_name = "KEY")]
    press: Vec<String>,

    /// Redact elements: cover every match of this CSS selector (repeatable)
    #[arg(long, value_name = "CSS")]
    mask: Vec<String>,

    /// Blur masked content instead of covering it with ink
    #[arg(long, value_name = "PX")]
    mask_blur: Option<u32>,

    /// Also redact sensitive-data matches (repeatable, off by default)
    #[arg(long, value_name = "NAME")]
    mask_patterns: Vec<capture::MaskPattern>,

    /// Point a numbered marker and label at an element: --point SELECTOR LABEL
    /// (repeatable; numbering follows flag order)
    #[arg(long = "point", value_names = ["SELECTOR", "LABEL"], num_args = 2)]
    point: Vec<String>,

    /// Outline the first element matching this CSS selector (repeatable)
    #[arg(long, value_name = "CSS")]
    highlight: Vec<String>,

    /// Dim the page outside annotated and highlighted elements
    #[arg(long)]
    dim: bool,

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

    /// Capture with a saved session (`iris login --session NAME <url>`)
    #[arg(long, value_name = "NAME")]
    session: Option<String>,

    /// Emit one JSON object per completed capture
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Mcp(args)) => mcp::run(args).await,
        Some(Command::Login(args)) => run_login(args).await,
        None => run_capture(cli.capture).await,
    }
}

/// Open a visible browser at `url` so the user can sign in, then keep the
/// profile on disk for later `--session` captures.
async fn run_login(args: LoginArgs) -> Result<()> {
    let url = normalize_url(&args.url)?;
    let name = args.session.trim().to_owned();
    let dir = capture::session_dir(&name)?;
    tokio::fs::create_dir_all(&dir).await?;
    let session =
        Session::launch_headed(args.chrome, capture::Viewport::desktop(), dir.clone()).await?;
    println!("Signing in as session {name:?} at {url}.");
    println!("Use the opened browser window to sign in, then press Enter here.");
    session.login_and_wait(url.as_str()).await?;
    session.close().await;
    println!("Saved session {name:?} → {}", dir.display());
    Ok(())
}

async fn run_capture(cli: CaptureArgs) -> Result<()> {
    if !cli.workflow.is_empty() {
        return run_workflows(cli).await;
    }
    let urls = collect_urls(&cli.urls)?;
    let viewport = parse_viewport(&cli.size, cli.scale)?;
    let mode = capture_mode(&cli);
    let flag_format = cli.format.as_deref().and_then(Format::from_ext);
    let (targets, format) = resolve_outputs(&urls, cli.out.as_deref(), flag_format).await?;
    let jobs = cli.jobs.unwrap_or_else(|| urls.len().min(4)).max(1);

    let annotations = build_annotations(&cli.point);

    let steps = build_steps(&cli);
    let color_scheme = resolve_color_scheme(&cli);
    let opts = Arc::new(Opts {
        viewport,
        mode: mode.clone(),
        color_scheme,
        wait_ms: cli.wait,
        wait_for: cli.wait_for,
        timeout: Duration::from_secs(cli.timeout.max(1)),
        format,
        annotations,
        highlights: cli.highlight,
        dim: cli.dim,
        steps,
        masks: cli.mask,
        mask_blur_px: cli.mask_blur,
        mask_patterns: cli.mask_patterns,
    });

    let session = if let Some(name) = cli.session.as_deref() {
        let dir = capture::resolve_session_dir(name)?;
        Arc::new(Session::launch_with_profile(cli.chrome, viewport, dir).await?)
    } else {
        Arc::new(Session::launch(cli.chrome, viewport).await?)
    };

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
                report_success(cli.json, url.as_str(), &path, &opts, &shot)?;
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

/// Run recipe files: load and validate everything first (typos fail before
/// Chrome launches), then capture concurrently across one browser per session.
async fn run_workflows(cli: CaptureArgs) -> Result<()> {
    let mut prepared = Vec::new();
    for file in &cli.workflow {
        let recipe = workflow::load_recipe(file)?;
        let request = mcp::CaptureRequest::from(recipe);
        prepared.push(
            request
                .prepare()
                .map_err(|err| anyhow::anyhow!("{}: {err:#}", file.display()))?,
        );
    }
    if cli.dry_run {
        for capture in &prepared {
            println!("{}", workflow_plan(capture)?);
        }
        return Ok(());
    }

    let jobs = cli.jobs.unwrap_or_else(|| prepared.len().min(4)).max(1);
    let mut sessions: HashMap<String, Arc<Session>> = HashMap::new();
    let mut launched: std::collections::HashSet<String> = std::collections::HashSet::new();
    for capture in &prepared {
        let key = capture.session.clone().unwrap_or_default();
        if !launched.insert(key.clone()) {
            continue;
        }
        let session = if key.is_empty() {
            Session::launch(cli.chrome.clone(), capture::Viewport::desktop()).await?
        } else {
            let dir = capture::resolve_session_dir(&key)?;
            Session::launch_with_profile(cli.chrome.clone(), capture::Viewport::desktop(), dir)
                .await?
        };
        sessions.insert(key, Arc::new(session));
    }
    let sessions = Arc::new(sessions);

    let mut failed = 0usize;
    let mut stream = futures::stream::iter(prepared)
        .map({
            let sessions = Arc::clone(&sessions);
            move |capture: mcp::PreparedCapture| {
                let sessions = Arc::clone(&sessions);
                async move {
                    let key = capture.session.clone().unwrap_or_default();
                    let session = Arc::clone(sessions.get(&key).expect("session pre-launched"));
                    let url = capture.url.clone();
                    let result = match session.capture(url.as_str(), &capture.opts).await {
                        Ok(image) => match &capture.output {
                            Some(path) => image
                                .write_to(path)
                                .await
                                .map(|()| (path.clone(), image.shot)),
                            None => Err(anyhow::anyhow!("workflow is missing its output path")),
                        },
                        Err(error) => Err(error),
                    };
                    (capture, result)
                }
            }
        })
        .buffer_unordered(jobs);

    while let Some((capture, result)) = stream.next().await {
        let url = capture.url.as_str();
        match result {
            Ok((path, shot)) => {
                report_success(cli.json, url, &path, &capture.opts, &shot)?;
            }
            Err(err) => {
                failed += 1;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string(&capture::error_report(
                            url,
                            capture.output.as_deref(),
                            &capture.opts.mode,
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
    if let Ok(sessions) = Arc::try_unwrap(sessions) {
        for (_, session) in sessions {
            if let Ok(session) = Arc::try_unwrap(session) {
                session.close().await;
            }
        }
    }

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn report_success(
    json: bool,
    url: &str,
    path: &std::path::Path,
    opts: &Opts,
    shot: &capture::Shot,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&success_report(
                url,
                Some(path),
                &opts.mode,
                opts.format,
                shot,
                opts.annotations.len(),
                opts.color_scheme,
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
    Ok(())
}

/// Resolved workflow plan for `--dry-run`: everything except pixels.
fn workflow_plan(capture: &mcp::PreparedCapture) -> Result<String> {
    let mode = &capture.opts.mode;
    Ok(serde_json::to_string(&serde_json::json!({
        "status": "ok",
        "url": capture.url.as_str(),
        "output": capture.output.as_ref().map(|path| capture::absolute_output(path)),
        "mode": mode.name(),
        "selector": mode.selector(),
        "padding": mode.padding(),
        "color_scheme": capture.opts.color_scheme.name(),
        "format": capture.opts.format.ext(),
        "session": capture.session,
        "steps": capture.opts.steps.len(),
        "annotations": capture.opts.annotations.len(),
        "masks": capture.opts.masks.len(),
    }))?)
}

/// Pair up flat `--point SELECTOR LABEL` values; numbering follows flag order.
fn build_annotations(points: &[String]) -> Vec<capture::Annotation> {
    points
        .chunks_exact(2)
        .enumerate()
        .map(|(i, pair)| capture::Annotation {
            selector: pair[0].clone(),
            label: pair[1].clone(),
            number: i as u32 + 1,
        })
        .collect()
}

/// Build the pre-capture interaction list. Order is kept within each flag;
/// different kinds run grouped — clicks, then fills, hovers, presses — so
/// exact mixed ordering belongs in MCP `steps` or a workflow file.
fn build_steps(cli: &CaptureArgs) -> Vec<capture::InteractionStep> {
    let mut steps = Vec::new();
    steps.extend(
        cli.click
            .iter()
            .map(|selector| capture::InteractionStep::Click {
                selector: selector.clone(),
            }),
    );
    steps.extend(
        cli.fill
            .chunks_exact(2)
            .map(|pair| capture::InteractionStep::Fill {
                selector: pair[0].clone(),
                text: pair[1].clone(),
            }),
    );
    steps.extend(
        cli.hover
            .iter()
            .map(|selector| capture::InteractionStep::Hover {
                selector: selector.clone(),
            }),
    );
    steps.extend(
        cli.press
            .iter()
            .map(|key| capture::InteractionStep::Press { key: key.clone() }),
    );
    steps
}

/// Resolve the effective color scheme. `--color-scheme` wins when present;
/// otherwise the `--dark` / `--light` shorthands apply; default is system
/// (no override, preserving historical output). Clap conflicts keep shorthand
/// and explicit flags from combining.
fn resolve_color_scheme(cli: &CaptureArgs) -> capture::ColorScheme {
    if let Some(scheme) = cli.color_scheme {
        scheme
    } else if cli.dark {
        capture::ColorScheme::Dark
    } else if cli.light {
        capture::ColorScheme::Light
    } else {
        capture::ColorScheme::System
    }
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
    fn login_subcommand_takes_a_session_and_a_url() {
        let cli =
            Cli::try_parse_from(["iris", "login", "--session", "matteros", "app.example.com"])
                .unwrap();
        match cli.command {
            Some(Command::Login(args)) => {
                assert_eq!(args.session, "matteros");
                assert_eq!(args.url, "app.example.com");
            }
            other => panic!("expected login subcommand, got {other:?}"),
        }

        let missing_url = Cli::try_parse_from(["iris", "login", "--session", "matteros"])
            .err()
            .unwrap();
        assert_eq!(missing_url.kind(), ErrorKind::MissingRequiredArgument);

        let capture =
            Cli::try_parse_from(["iris", "example.com", "--session", "matteros"]).unwrap();
        assert_eq!(capture.capture.session.as_deref(), Some("matteros"));
        assert!(capture.command.is_none());
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
    fn point_flags_pair_up_with_flag_order_numbering() {
        let cli = Cli::try_parse_from([
            "iris",
            "example.com",
            "--point",
            "#conditions-tab",
            "Open Conditions",
            "--point",
            "#search",
            "Find a condition",
            "--highlight",
            "#run-analysis",
            "--dim",
        ])
        .unwrap();
        assert_eq!(
            cli.capture.point,
            vec![
                "#conditions-tab",
                "Open Conditions",
                "#search",
                "Find a condition"
            ]
        );
        assert_eq!(cli.capture.highlight, vec!["#run-analysis"]);
        assert!(cli.capture.dim);

        let annotations = build_annotations(&cli.capture.point);
        assert_eq!(
            annotations,
            vec![
                capture::Annotation {
                    selector: "#conditions-tab".into(),
                    label: "Open Conditions".into(),
                    number: 1,
                },
                capture::Annotation {
                    selector: "#search".into(),
                    label: "Find a condition".into(),
                    number: 2,
                },
            ]
        );
    }

    #[test]
    fn interaction_flags_keep_order_within_kind_and_group_across_kinds() {
        let cli = Cli::try_parse_from([
            "iris",
            "example.com",
            "--click",
            "text=Conditions",
            "--open",
            "#drawer",
            "--fill",
            "#search",
            "asthma",
            "--hover",
            "#menu",
            "--press",
            "Enter",
        ])
        .unwrap();
        // --open is the same action as --click and shares its order.
        assert_eq!(cli.capture.click, vec!["text=Conditions", "#drawer"]);
        assert_eq!(
            build_steps(&cli.capture),
            vec![
                capture::InteractionStep::Click {
                    selector: "text=Conditions".into(),
                },
                capture::InteractionStep::Click {
                    selector: "#drawer".into(),
                },
                capture::InteractionStep::Fill {
                    selector: "#search".into(),
                    text: "asthma".into(),
                },
                capture::InteractionStep::Hover {
                    selector: "#menu".into(),
                },
                capture::InteractionStep::Press {
                    key: "Enter".into()
                },
            ]
        );
    }

    #[test]
    fn mask_flags_parse_selectors_blur_and_pattern_names() {
        let cli = Cli::try_parse_from([
            "iris",
            "example.com",
            "--mask",
            ".client-name",
            "--mask",
            "[data-private]",
            "--mask-blur",
            "6",
            "--mask-patterns",
            "email",
            "--mask-patterns",
            "ssn",
        ])
        .unwrap();
        assert_eq!(cli.capture.mask, vec![".client-name", "[data-private]"]);
        assert_eq!(cli.capture.mask_blur, Some(6));
        assert_eq!(
            cli.capture.mask_patterns,
            vec![capture::MaskPattern::Email, capture::MaskPattern::Ssn,]
        );

        let error = Cli::try_parse_from(["iris", "example.com", "--mask-patterns", "passport"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn point_requires_both_selector_and_label() {
        let error = Cli::try_parse_from(["iris", "example.com", "--point", "#only-selector"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::WrongNumberOfValues);
    }

    #[test]
    fn color_scheme_resolution_prefers_explicit_and_defaults_to_system() {
        let plain = Cli::try_parse_from(["iris", "example.com"]).unwrap();
        assert_eq!(
            resolve_color_scheme(&plain.capture),
            capture::ColorScheme::System
        );

        let dark = Cli::try_parse_from(["iris", "example.com", "--dark"]).unwrap();
        assert_eq!(
            resolve_color_scheme(&dark.capture),
            capture::ColorScheme::Dark
        );

        let light = Cli::try_parse_from(["iris", "example.com", "--light"]).unwrap();
        assert_eq!(
            resolve_color_scheme(&light.capture),
            capture::ColorScheme::Light
        );

        let explicit =
            Cli::try_parse_from(["iris", "example.com", "--color-scheme", "dark"]).unwrap();
        assert_eq!(
            resolve_color_scheme(&explicit.capture),
            capture::ColorScheme::Dark
        );

        let clash = Cli::try_parse_from(["iris", "example.com", "--dark", "--light"])
            .err()
            .unwrap();
        assert_eq!(clash.kind(), ErrorKind::ArgumentConflict);

        let mixed =
            Cli::try_parse_from(["iris", "example.com", "--dark", "--color-scheme", "light"])
                .err()
                .unwrap();
        assert_eq!(mixed.kind(), ErrorKind::ArgumentConflict);

        let bad = Cli::try_parse_from(["iris", "example.com", "--color-scheme", "sepia"])
            .err()
            .unwrap();
        assert_eq!(bad.kind(), ErrorKind::InvalidValue);
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
            masked: 0,
        };
        let success = serde_json::to_string(&success_report(
            "https://example.com/",
            Some(std::path::Path::new("/tmp/example.png")),
            &mode,
            Format::Png,
            &shot,
            0,
            capture::ColorScheme::System,
        ))
        .unwrap();
        assert_eq!(
            success,
            r#"{"status":"ok","url":"https://example.com/","output":"/tmp/example.png","mode":"element","selector":"h1","padding":24,"css_width":180,"css_height":72,"scale":2.0,"format":"png","bytes":14231,"color_scheme":"system"}"#
        );

        let annotated = serde_json::to_string(&success_report(
            "https://example.com/",
            Some(std::path::Path::new("/tmp/example.png")),
            &mode,
            Format::Png,
            &shot,
            2,
            capture::ColorScheme::Dark,
        ))
        .unwrap();
        assert!(annotated.ends_with(r#""color_scheme":"dark","annotations":2}"#));

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
