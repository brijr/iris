use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Args;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};

use crate::capture::{
    CaptureMode, CapturedImage, Format, Opts, Session, Viewport, error_report, normalize_url,
    parse_viewport, success_report,
};

#[derive(Debug, Args)]
pub struct McpArgs {
    /// Chrome/Chromium binary [auto-detected]
    #[arg(long, env = "CHROME", value_name = "PATH")]
    chrome: Option<PathBuf>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CaptureRequest {
    /// URL to capture. Bare localhost and loopback addresses use HTTP; other bare hosts use HTTPS.
    url: String,
    /// Capture the first element matching this CSS selector.
    selector: Option<String>,
    /// Uniform CSS-pixel padding around a selected element.
    padding: Option<u32>,
    /// Capture the full page height. Conflicts with selector.
    #[serde(default)]
    full_page: bool,
    /// Viewport as WxH or desktop, iphone, or ipad. Defaults to desktop.
    size: Option<String>,
    /// Emulate prefers-color-scheme: dark (shorthand for color_scheme dark).
    #[serde(default)]
    dark: bool,
    /// Force a color scheme: light, dark, or system. Defaults to system.
    color_scheme: Option<ColorSchemeRequest>,
    /// Image format. Defaults to png; a recognized output extension wins.
    format: Option<ImageFormat>,
    /// Extra settle delay in milliseconds after smart waiting.
    #[serde(default)]
    wait_ms: u64,
    /// Wait until this CSS selector exists before capturing.
    wait_for: Option<String>,
    /// Ordered interactions to perform before capturing.
    steps: Option<Vec<StepRequest>>,
    /// Redact elements: cover every match of each CSS selector.
    redact: Option<Vec<String>>,
    /// Blur masked content instead of covering it with ink.
    mask_blur_px: Option<u32>,
    /// Also redact sensitive-data matches (off by default).
    redact_patterns: Option<Vec<RedactPattern>>,
    /// Numbered markers with labels pointing at elements, in list order.
    annotations: Option<Vec<AnnotationRequest>>,
    /// Outline the first element matching each CSS selector.
    highlight: Option<Vec<String>>,
    /// Dim the page outside annotated and highlighted elements.
    #[serde(default)]
    dim: bool,
    /// Capture with a saved session (`iris login --session NAME <url>`).
    session: Option<String>,
    /// Device scale factor overriding the viewport preset.
    scale: Option<f64>,
    /// Per-page timeout in seconds. Defaults to 30.
    timeout_seconds: Option<u64>,
    /// Optional image path. Relative paths resolve from the MCP server working directory.
    output: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
pub struct AnnotationRequest {
    /// CSS selector of the element to point at (first match wins).
    selector: String,
    /// Short label shown beside the marker.
    label: String,
}

/// Forced color scheme. `system` applies no override; `dark: true` stays as a
/// back-compat shorthand for `dark` and conflicts with this field.
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ColorSchemeRequest {
    Light,
    Dark,
    System,
}

impl ColorSchemeRequest {
    fn capture_scheme(self) -> crate::capture::ColorScheme {
        match self {
            Self::Light => crate::capture::ColorScheme::Light,
            Self::Dark => crate::capture::ColorScheme::Dark,
            Self::System => crate::capture::ColorScheme::System,
        }
    }
}

/// Opt-in sensitive-data detector for masking.
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RedactPattern {
    Email,
    Phone,
    Ssn,
    Account,
}

impl RedactPattern {
    fn capture_pattern(self) -> crate::capture::MaskPattern {
        match self {
            Self::Email => crate::capture::MaskPattern::Email,
            Self::Phone => crate::capture::MaskPattern::Phone,
            Self::Ssn => crate::capture::MaskPattern::Ssn,
            Self::Account => crate::capture::MaskPattern::Account,
        }
    }
}

/// One pre-capture interaction. Array order is the execution order.
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum StepRequest {
    Click {
        /// CSS selector or `text=...` to click.
        click: String,
    },
    Open {
        /// CSS selector to open (menus, drawers); same action as click.
        open: String,
    },
    Fill {
        /// CSS selector of the input to fill.
        fill: String,
        /// Text to type into the input.
        text: String,
    },
    Hover {
        /// CSS selector to hover.
        hover: String,
    },
    Press {
        /// Keyboard key to press, e.g. Enter or Escape.
        press: String,
    },
    WaitFor {
        /// Wait until this CSS selector exists before continuing.
        wait_for: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpg,
    Jpeg,
    Webp,
}

impl ImageFormat {
    fn capture_format(self) -> Format {
        match self {
            Self::Png => Format::Png,
            Self::Jpg | Self::Jpeg => Format::Jpeg,
            Self::Webp => Format::Webp,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PreparedCapture {
    pub(crate) url: url::Url,
    pub(crate) output: Option<PathBuf>,
    pub(crate) session: Option<String>,
    pub(crate) opts: Opts,
}

/// Workflow recipes convert into capture requests so CLI files, MCP calls,
/// and CLI flags all validate through the same `prepare` path.
impl From<crate::workflow::Recipe> for CaptureRequest {
    fn from(recipe: crate::workflow::Recipe) -> Self {
        Self {
            url: recipe.url,
            selector: recipe.selector,
            padding: recipe.padding,
            full_page: recipe.full,
            size: recipe.size,
            dark: false,
            color_scheme: recipe.color_scheme,
            format: recipe.format,
            wait_ms: recipe.wait_ms,
            wait_for: recipe.wait_for,
            scale: recipe.scale,
            timeout_seconds: recipe.timeout_seconds,
            output: Some(recipe.output),
            steps: Some(recipe.steps),
            redact: Some(recipe.redact),
            mask_blur_px: recipe.mask_blur_px,
            redact_patterns: Some(recipe.redact_patterns),
            annotations: Some(
                recipe
                    .annotations
                    .into_iter()
                    .map(|point| AnnotationRequest {
                        selector: point.selector,
                        label: point.label,
                    })
                    .collect(),
            ),
            highlight: Some(recipe.highlight),
            dim: recipe.dim,
            session: recipe.session,
        }
    }
}

impl CaptureRequest {
    pub(crate) fn prepare(self) -> Result<PreparedCapture> {
        let url = normalize_url(self.url.trim())?;
        let selector_supplied = self.selector.is_some();
        let selector = self
            .selector
            .map(|selector| selector.trim().to_owned())
            .filter(|selector| !selector.is_empty());
        if selector_supplied && selector.is_none() {
            bail!("selector must not be empty");
        }
        if self.full_page && selector.is_some() {
            bail!("selector conflicts with full_page");
        }
        if self.padding.is_some() && selector.is_none() {
            bail!("padding requires selector");
        }
        let mode = if let Some(selector) = selector {
            CaptureMode::Element {
                selector,
                padding: self.padding.unwrap_or(0),
            }
        } else if self.full_page {
            CaptureMode::FullPage
        } else {
            CaptureMode::Viewport
        };

        let viewport = parse_viewport(self.size.as_deref().unwrap_or("desktop"), self.scale)?;
        let timeout_seconds = self.timeout_seconds.unwrap_or(30);
        if timeout_seconds == 0 {
            bail!("timeout_seconds must be greater than zero");
        }
        let wait_for_supplied = self.wait_for.is_some();
        let wait_for = self
            .wait_for
            .map(|selector| selector.trim().to_owned())
            .filter(|selector| !selector.is_empty());
        if wait_for_supplied && wait_for.is_none() {
            bail!("wait_for must not be empty");
        }
        let session_supplied = self.session.is_some();
        let session = self
            .session
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
        if session_supplied && session.is_none() {
            bail!("session must not be empty");
        }
        if let Some(name) = &session {
            crate::capture::validate_session_name(name)?;
        }

        let annotations = self
            .annotations
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, point)| {
                let selector = point.selector.trim().to_owned();
                let label = point.label.trim().to_owned();
                if selector.is_empty() {
                    bail!("annotations[{i}].selector must not be empty");
                }
                if label.is_empty() {
                    bail!("annotations[{i}].label must not be empty");
                }
                Ok(crate::capture::Annotation {
                    selector,
                    label,
                    number: i as u32 + 1,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let highlights = self
            .highlight
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, selector)| {
                let selector = selector.trim().to_owned();
                if selector.is_empty() {
                    bail!("highlight[{i}] must not be empty");
                }
                Ok(selector)
            })
            .collect::<Result<Vec<_>>>()?;

        let steps = self
            .steps
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, step)| {
                let at = format!("steps[{i}]");
                let non_empty = |value: String, field: &str| {
                    let value = value.trim().to_owned();
                    if value.is_empty() {
                        bail!("{at}.{field} must not be empty");
                    }
                    Ok(value)
                };
                match step {
                    StepRequest::Click { click } => Ok(crate::capture::InteractionStep::Click {
                        selector: non_empty(click, "click")?,
                    }),
                    StepRequest::Open { open } => Ok(crate::capture::InteractionStep::Click {
                        selector: non_empty(open, "open")?,
                    }),
                    StepRequest::Fill { fill, text } => Ok(crate::capture::InteractionStep::Fill {
                        selector: non_empty(fill, "fill")?,
                        text: text.trim().to_owned(),
                    }),
                    StepRequest::Hover { hover } => Ok(crate::capture::InteractionStep::Hover {
                        selector: non_empty(hover, "hover")?,
                    }),
                    StepRequest::Press { press } => Ok(crate::capture::InteractionStep::Press {
                        key: non_empty(press, "press")?,
                    }),
                    StepRequest::WaitFor { wait_for } => {
                        Ok(crate::capture::InteractionStep::WaitFor {
                            selector: non_empty(wait_for, "wait_for")?,
                        })
                    }
                }
            })
            .collect::<Result<Vec<_>>>()?;

        let masks = self
            .redact
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, selector)| {
                let selector = selector.trim().to_owned();
                if selector.is_empty() {
                    bail!("redact[{i}] must not be empty");
                }
                Ok(selector)
            })
            .collect::<Result<Vec<_>>>()?;
        let mask_patterns = self
            .redact_patterns
            .unwrap_or_default()
            .into_iter()
            .map(RedactPattern::capture_pattern)
            .collect();

        let mut format = self
            .format
            .map(ImageFormat::capture_format)
            .unwrap_or(Format::Png);
        if let Some(output) = &self.output {
            let extension = output
                .extension()
                .and_then(|extension| extension.to_str())
                .ok_or_else(|| anyhow!("output must end in .png, .jpg, .jpeg, or .webp"))?;
            format = Format::from_ext(extension)
                .ok_or_else(|| anyhow!("unsupported output extension: .{extension}"))?;
        }

        let color_scheme = match (self.color_scheme, self.dark) {
            (Some(scheme), false) => scheme.capture_scheme(),
            (Some(_), true) => bail!("dark conflicts with color_scheme"),
            (None, true) => crate::capture::ColorScheme::Dark,
            (None, false) => crate::capture::ColorScheme::System,
        };

        Ok(PreparedCapture {
            url,
            output: self.output,
            session,
            opts: Opts {
                viewport,
                mode,
                color_scheme,
                wait_ms: self.wait_ms,
                wait_for,
                timeout: Duration::from_secs(timeout_seconds),
                format,
                annotations,
                highlights,
                dim: self.dim,
                steps,
                masks,
                mask_blur_px: self.mask_blur_px,
                mask_patterns,
            },
        })
    }
}

struct McpState {
    chrome: Option<PathBuf>,
    sessions: Mutex<std::collections::HashMap<String, Arc<Session>>>,
    permits: Semaphore,
}

/// Cache key for the default temp-profile session.
const DEFAULT_SESSION: &str = "";

impl McpState {
    fn new(chrome: Option<PathBuf>) -> Self {
        Self {
            chrome,
            sessions: Mutex::new(std::collections::HashMap::new()),
            permits: Semaphore::new(4),
        }
    }

    async fn session(&self, name: Option<&str>) -> Result<Arc<Session>> {
        let key = name.unwrap_or(DEFAULT_SESSION).to_owned();
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(&key)
            .is_some_and(|session| !session.is_healthy())
        {
            sessions.remove(&key);
        }
        if let Some(session) = sessions.get(&key) {
            return Ok(Arc::clone(session));
        }
        let launched = if key.is_empty() {
            Arc::new(Session::launch(self.chrome.clone(), Viewport::desktop()).await?)
        } else {
            let dir = crate::capture::resolve_session_dir(&key)?;
            Arc::new(
                Session::launch_with_profile(self.chrome.clone(), Viewport::desktop(), dir).await?,
            )
        };
        sessions.insert(key, Arc::clone(&launched));
        Ok(launched)
    }

    async fn capture(
        &self,
        session_name: Option<&str>,
        url: &str,
        opts: &Opts,
    ) -> Result<CapturedImage> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| anyhow!("capture queue closed"))?;
        let session = self.session(session_name).await?;
        let result = session.capture(url, opts).await;
        if result.is_err() && !session.is_healthy() {
            let mut current = self.sessions.lock().await;
            let key = session_name.unwrap_or(DEFAULT_SESSION);
            if current
                .get(key)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &session))
            {
                current.remove(key);
            }
        }
        result
    }

    async fn close(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for (_, session) in sessions {
            if let Ok(session) = Arc::try_unwrap(session) {
                session.close().await;
            }
        }
    }
}

#[derive(Clone)]
struct IrisServer {
    state: Arc<McpState>,
    tool_router: ToolRouter<Self>,
}

impl IrisServer {
    fn new(state: Arc<McpState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    async fn capture_image(&self, request: CaptureRequest) -> CallToolResult {
        let prepared = match request.prepare() {
            Ok(prepared) => prepared,
            Err(error) => return simple_error(format!("{error:#}")),
        };

        let image = match self
            .state
            .capture(
                prepared.session.as_deref(),
                prepared.url.as_str(),
                &prepared.opts,
            )
            .await
        {
            Ok(image) => image,
            Err(error) => {
                return report_error(&prepared, format!("{error:#}"));
            }
        };
        if let Some(output) = &prepared.output
            && let Err(error) = image.write_to(output).await
        {
            return report_error(&prepared, format!("{error:#}"));
        }

        let report = success_report(
            prepared.url.as_str(),
            prepared.output.as_deref(),
            &prepared.opts.mode,
            prepared.opts.format,
            &image.shot,
            prepared.opts.annotations.len(),
            prepared.opts.color_scheme,
        );
        let structured = match serde_json::to_value(&report) {
            Ok(structured) => structured,
            Err(error) => {
                return simple_error(format!("failed to serialize capture report: {error}"));
            }
        };
        let destination = prepared
            .output
            .as_deref()
            .map(crate::capture::absolute_output)
            .map(|path| format!(" → {path}"))
            .unwrap_or_default();
        let summary = format!(
            "Captured {}×{} CSS px @{}x as {}{}",
            image.shot.width,
            image.shot.height,
            image.shot.scale,
            prepared.opts.format.ext(),
            destination,
        );
        let mut result = CallToolResult::success(vec![
            ContentBlock::image(BASE64.encode(&image.data), prepared.opts.format.mime_type()),
            ContentBlock::text(summary),
        ]);
        result.structured_content = Some(structured);
        result
    }
}

#[tool_router(router = tool_router)]
impl IrisServer {
    /// Capture one trustworthy image of a live page or its first matching element.
    #[tool(
        name = "capture",
        annotations(
            title = "Capture a web page",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn capture(&self, Parameters(request): Parameters<CaptureRequest>) -> CallToolResult {
        self.capture_image(request).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for IrisServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("iris", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Iris is a camera for coding agents. Use capture for one page or element at a time; it returns the image inline and saves a file only when output is provided.",
            )
    }
}

fn simple_error(message: String) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(message.clone())]);
    result.structured_content = Some(json!({ "status": "error", "error": message }));
    result
}

fn report_error(prepared: &PreparedCapture, message: String) -> CallToolResult {
    let report = error_report(
        prepared.url.as_str(),
        prepared.output.as_deref(),
        &prepared.opts.mode,
        message.clone(),
    );
    let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
    result.structured_content = serde_json::to_value(report).ok();
    result
}

pub async fn run(args: McpArgs) -> Result<()> {
    let state = Arc::new(McpState::new(args.chrome));
    let service = IrisServer::new(Arc::clone(&state))
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start Iris MCP server")?;
    let cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    let (result, signalled) = tokio::select! {
        result = &mut waiting => (Ok(result), false),
        signal = shutdown_signal() => {
            cancellation.cancel();
            let result = waiting.await;
            (signal.map(|()| result), true)
        }
    };
    state.close().await;
    result?.context("Iris MCP server task failed")?;
    if signalled {
        // Tokio's stdio reader uses a blocking thread that cannot be cancelled
        // while a client keeps stdin open. All Iris and RMCP cleanup is complete,
        // so exit directly instead of hanging during runtime teardown.
        std::process::exit(0);
    }
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("failed to listen for SIGINT")?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> CaptureRequest {
        CaptureRequest {
            url: url.into(),
            selector: None,
            padding: None,
            full_page: false,
            size: None,
            dark: false,
            format: None,
            wait_ms: 0,
            wait_for: None,
            scale: None,
            timeout_seconds: None,
            color_scheme: None,
            session: None,
            output: None,
            annotations: None,
            highlight: None,
            dim: false,
            steps: None,
            redact: None,
            mask_blur_px: None,
            redact_patterns: None,
        }
    }

    #[test]
    fn request_defaults_and_conflicts_are_validated() {
        let prepared = request("localhost:3000").prepare().unwrap();
        assert_eq!(prepared.url.as_str(), "http://localhost:3000/");
        assert_eq!(prepared.opts.mode, CaptureMode::Viewport);
        assert_eq!(prepared.opts.viewport.width, 1440);
        assert_eq!(prepared.opts.format, Format::Png);
        assert_eq!(prepared.opts.timeout, Duration::from_secs(30));

        let mut conflict = request("example.com");
        conflict.selector = Some("main".into());
        conflict.full_page = true;
        assert!(
            conflict
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("selector conflicts with full_page")
        );

        let mut padding = request("example.com");
        padding.padding = Some(0);
        assert!(
            padding
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("padding requires selector")
        );
    }

    #[test]
    fn annotation_requests_are_numbered_and_validated() {
        let mut capture = request("example.com");
        capture.annotations = Some(vec![
            AnnotationRequest {
                selector: " #search ".into(),
                label: " Find ".into(),
            },
            AnnotationRequest {
                selector: "#run-analysis".into(),
                label: "Run".into(),
            },
        ]);
        capture.highlight = Some(vec![" #hero ".into()]);
        capture.dim = true;
        let prepared = capture.prepare().unwrap();
        assert_eq!(
            prepared.opts.annotations,
            vec![
                crate::capture::Annotation {
                    selector: "#search".into(),
                    label: "Find".into(),
                    number: 1,
                },
                crate::capture::Annotation {
                    selector: "#run-analysis".into(),
                    label: "Run".into(),
                    number: 2,
                },
            ]
        );
        assert_eq!(prepared.opts.highlights, vec!["#hero"]);
        assert!(prepared.opts.dim);

        let mut empty_label = request("example.com");
        empty_label.annotations = Some(vec![AnnotationRequest {
            selector: "#search".into(),
            label: "  ".into(),
        }]);
        assert!(
            empty_label
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("annotations[0].label must not be empty")
        );

        let mut empty_highlight = request("example.com");
        empty_highlight.highlight = Some(vec!["  ".into()]);
        assert!(
            empty_highlight
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("highlight[0] must not be empty")
        );
    }

    #[test]
    fn step_requests_keep_array_order_and_reject_empties() {
        let mut capture = request("example.com");
        capture.steps = Some(vec![
            StepRequest::Click {
                click: " text=Conditions ".into(),
            },
            StepRequest::Open {
                open: "#drawer".into(),
            },
            StepRequest::Fill {
                fill: "#search".into(),
                text: "asthma".into(),
            },
            StepRequest::Hover {
                hover: "#menu".into(),
            },
            StepRequest::Press {
                press: "Enter".into(),
            },
            StepRequest::WaitFor {
                wait_for: "[role=dialog]".into(),
            },
        ]);
        let prepared = capture.prepare().unwrap();
        assert_eq!(
            prepared.opts.steps,
            vec![
                crate::capture::InteractionStep::Click {
                    selector: "text=Conditions".into(),
                },
                crate::capture::InteractionStep::Click {
                    selector: "#drawer".into(),
                },
                crate::capture::InteractionStep::Fill {
                    selector: "#search".into(),
                    text: "asthma".into(),
                },
                crate::capture::InteractionStep::Hover {
                    selector: "#menu".into(),
                },
                crate::capture::InteractionStep::Press {
                    key: "Enter".into()
                },
                crate::capture::InteractionStep::WaitFor {
                    selector: "[role=dialog]".into(),
                },
            ]
        );

        let mut empty = request("example.com");
        empty.steps = Some(vec![StepRequest::Click { click: "  ".into() }]);
        assert!(
            empty
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("steps[0].click must not be empty")
        );
    }

    #[test]
    fn redact_requests_map_selectors_blur_and_patterns() {
        let mut capture = request("example.com");
        capture.redact = Some(vec![" .client-name ".into(), "[data-private]".into()]);
        capture.mask_blur_px = Some(8);
        capture.redact_patterns = Some(vec![RedactPattern::Email, RedactPattern::Account]);
        let prepared = capture.prepare().unwrap();
        assert_eq!(prepared.opts.masks, vec![".client-name", "[data-private]"]);
        assert_eq!(prepared.opts.mask_blur_px, Some(8));
        assert_eq!(
            prepared.opts.mask_patterns,
            vec![
                crate::capture::MaskPattern::Email,
                crate::capture::MaskPattern::Account,
            ]
        );

        let mut empty = request("example.com");
        empty.redact = Some(vec!["  ".into()]);
        assert!(
            empty
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("redact[0] must not be empty")
        );

        let unknown: Result<CaptureRequest, _> = serde_json::from_value(serde_json::json!({
            "url": "example.com",
            "redact_patterns": ["passport"],
        }));
        assert!(unknown.is_err());
    }

    #[test]
    fn color_scheme_requests_map_with_dark_back_compat() {
        let plain = request("example.com").prepare().unwrap();
        assert_eq!(plain.opts.color_scheme, crate::capture::ColorScheme::System);

        let mut dark = request("example.com");
        dark.dark = true;
        assert_eq!(
            dark.prepare().unwrap().opts.color_scheme,
            crate::capture::ColorScheme::Dark
        );

        let mut light = request("example.com");
        light.color_scheme = Some(ColorSchemeRequest::Light);
        assert_eq!(
            light.prepare().unwrap().opts.color_scheme,
            crate::capture::ColorScheme::Light
        );

        let mut clash = request("example.com");
        clash.dark = true;
        clash.color_scheme = Some(ColorSchemeRequest::Light);
        assert!(
            clash
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("dark conflicts with color_scheme")
        );
    }

    #[test]
    fn session_requests_are_trimmed_and_validated() {
        let plain = request("example.com").prepare().unwrap();
        assert_eq!(plain.session, None);

        let mut named = request("example.com");
        named.session = Some(" matteros ".into());
        assert_eq!(
            named.prepare().unwrap().session.as_deref(),
            Some("matteros")
        );

        let mut empty = request("example.com");
        empty.session = Some("  ".into());
        assert!(
            empty
                .prepare()
                .unwrap_err()
                .to_string()
                .contains("session must not be empty")
        );

        let mut evil = request("example.com");
        evil.session = Some("../evil".into());
        assert!(
            evil.prepare()
                .unwrap_err()
                .to_string()
                .contains("invalid session name")
        );
    }

    #[test]
    fn recipe_and_request_with_same_values_prepare_identically() {
        let recipe: crate::workflow::Recipe = serde_saphyr::from_str(
            r##"
url: example.com
size: 800x600
scale: 1.5
selector: "#main"
padding: 10
color_scheme: dark
session: matteros
format: jpg
wait_ms: 100
wait_for: "#ready"
timeout_seconds: 12
steps:
  - click: "#open"
  - {fill: "#q", text: "hi"}
  - hover: "#menu"
  - press: Enter
  - wait_for: "#done"
redact: ["[data-private]"]
mask_blur_px: 4
redact_patterns: [email, ssn]
annotations:
  - selector: "#q"
    label: Type here
highlight: ["#go"]
dim: true
output: shots/all.png
"##,
        )
        .unwrap();
        let from_recipe = CaptureRequest::from(recipe).prepare().unwrap();
        let from_request = CaptureRequest {
            url: "example.com".into(),
            selector: Some("#main".into()),
            padding: Some(10),
            full_page: false,
            size: Some("800x600".into()),
            dark: false,
            color_scheme: Some(ColorSchemeRequest::Dark),
            format: Some(ImageFormat::Jpg),
            wait_ms: 100,
            wait_for: Some("#ready".into()),
            scale: Some(1.5),
            timeout_seconds: Some(12),
            output: Some(PathBuf::from("shots/all.png")),
            steps: Some(vec![
                StepRequest::Click {
                    click: "#open".into(),
                },
                StepRequest::Fill {
                    fill: "#q".into(),
                    text: "hi".into(),
                },
                StepRequest::Hover {
                    hover: "#menu".into(),
                },
                StepRequest::Press {
                    press: "Enter".into(),
                },
                StepRequest::WaitFor {
                    wait_for: "#done".into(),
                },
            ]),
            redact: Some(vec!["[data-private]".into()]),
            mask_blur_px: Some(4),
            redact_patterns: Some(vec![RedactPattern::Email, RedactPattern::Ssn]),
            annotations: Some(vec![AnnotationRequest {
                selector: "#q".into(),
                label: "Type here".into(),
            }]),
            highlight: Some(vec!["#go".into()]),
            dim: true,
            session: Some("matteros".into()),
        }
        .prepare()
        .unwrap();
        assert_eq!(
            format!("{:?}", from_recipe.opts),
            format!("{:?}", from_request.opts)
        );
        assert_eq!(from_recipe.url, from_request.url);
        assert_eq!(from_recipe.output, from_request.output);
        assert_eq!(from_recipe.session, from_request.session);
    }

    #[test]
    fn output_extension_overrides_requested_format() {
        let mut capture = request("example.com");
        capture.format = Some(ImageFormat::Png);
        capture.output = Some(PathBuf::from("shot.webp"));
        let prepared = capture.prepare().unwrap();
        assert_eq!(prepared.opts.format, Format::Webp);
    }

    #[test]
    fn server_advertises_one_focused_capture_tool() {
        let server = IrisServer::new(Arc::new(McpState::new(None)));
        let tools = server.tool_router.list_all();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name, "capture");
        assert_eq!(
            tool.input_schema
                .get("required")
                .and_then(|required| required.as_array())
                .unwrap(),
            &[serde_json::Value::String("url".into())]
        );
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    #[tokio::test]
    async fn capture_tool_returns_inline_pixels_metadata_and_optional_file() -> Result<()> {
        let temp = std::env::temp_dir().join(format!("iris-mcp-{}", std::process::id()));
        let output = temp.join("nested/target.png");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/precise-capture.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let state = Arc::new(McpState::new(None));
        let server = IrisServer::new(Arc::clone(&state));

        let mut capture = request(url.as_str());
        capture.selector = Some(".capture-target".into());
        capture.padding = Some(10);
        capture.size = Some("320x240".into());
        capture.scale = Some(1.0);
        capture.timeout_seconds = Some(3);
        capture.output = Some(output.clone());
        let result = server.capture_image(capture).await;

        assert_eq!(result.is_error, Some(false));
        let image = result
            .content
            .iter()
            .find_map(ContentBlock::as_image)
            .expect("capture result should contain an image");
        assert_eq!(image.mime_type, "image/png");
        let bytes = BASE64.decode(&image.data)?;
        assert_eq!(png_dimensions(&bytes), (141, 81));
        assert_eq!(tokio::fs::read(&output).await?, bytes);
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["status"], "ok");
        assert_eq!(structured["mode"], "element");
        assert_eq!(structured["css_width"], 141);
        assert_eq!(structured["css_height"], 81);
        assert_eq!(
            structured["output"],
            crate::capture::absolute_output(&output)
        );

        let mut invalid = request(url.as_str());
        invalid.selector = Some("[".into());
        invalid.timeout_seconds = Some(3);
        let failure = server.capture_image(invalid).await;
        assert_eq!(failure.is_error, Some(true));
        assert!(
            failure
                .content
                .iter()
                .all(|content| content.as_image().is_none())
        );
        assert!(
            failure.structured_content.unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("invalid selector: [")
        );

        state.close().await;
        tokio::fs::remove_dir_all(temp).await?;
        Ok(())
    }

    fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        )
    }
}
