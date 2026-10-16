use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use futures::future::join_all;
use rmcp::handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
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
    /// Emulate prefers-color-scheme: dark.
    #[serde(default)]
    dark: bool,
    /// Image format. Defaults to png; a recognized output extension wins.
    format: Option<ImageFormat>,
    /// JPEG/WebP quality from 0 to 100. Defaults to 90.
    quality: Option<u8>,
    /// Reduce image density to fit this pixel budget without changing page layout.
    max_pixels: Option<u64>,
    /// Finish finite animations and pause repeating ones before capture.
    #[serde(default)]
    freeze_animations: bool,
    /// Extra delay in milliseconds before the final readiness check.
    #[serde(default)]
    wait_ms: u64,
    /// Wait until this CSS selector exists before capturing.
    wait_for: Option<String>,
    /// Device scale factor overriding the viewport preset.
    scale: Option<f64>,
    /// Deadline including queue and browser startup, in seconds. Defaults to 30.
    timeout_seconds: Option<u64>,
    /// Optional image path. Relative paths resolve from the MCP server working directory.
    output: Option<PathBuf>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchRequest {
    /// Up to 16 captures, run concurrently. Results stay in request order.
    captures: Vec<CaptureRequest>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompareRequest {
    /// Path to the baseline image (PNG, JPEG, or WebP).
    before: PathBuf,
    /// Path to the new image (PNG, JPEG, or WebP).
    after: PathBuf,
    /// Ignore channel differences up to this value (0–255). Defaults to 0.
    threshold: Option<u8>,
    /// Optional PNG path for a visual difference map.
    output: Option<PathBuf>,
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
struct PreparedCapture {
    url: url::Url,
    output: Option<PathBuf>,
    opts: Opts,
}

impl CaptureRequest {
    fn prepare(self) -> Result<PreparedCapture> {
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

        let quality = self.quality.unwrap_or(90);
        if quality > 100 {
            bail!("quality must be between 0 and 100");
        }
        if self.max_pixels == Some(0) {
            bail!("max_pixels must be greater than zero");
        }
        Ok(PreparedCapture {
            url,
            output: self.output,
            opts: Opts {
                viewport,
                mode,
                dark: self.dark,
                wait_ms: self.wait_ms,
                wait_for,
                timeout: Duration::from_secs(timeout_seconds),
                format,
                quality,
                max_pixels: self.max_pixels,
                freeze_animations: self.freeze_animations,
            },
        })
    }
}

struct McpState {
    chrome: Option<PathBuf>,
    session: Mutex<Option<Arc<Session>>>,
    permits: Arc<Semaphore>,
}

impl McpState {
    fn new(chrome: Option<PathBuf>) -> Self {
        Self {
            chrome,
            session: Mutex::new(None),
            permits: Arc::new(Semaphore::new(4)),
        }
    }

    async fn session(&self) -> Result<Arc<Session>> {
        let mut session = self.session.lock().await;
        if session
            .as_ref()
            .is_some_and(|session| !session.is_healthy())
        {
            session.take();
        }
        if let Some(session) = session.as_ref() {
            return Ok(Arc::clone(session));
        }
        let launched = Arc::new(Session::launch(self.chrome.clone(), Viewport::desktop()).await?);
        *session = Some(Arc::clone(&launched));
        Ok(launched)
    }

    async fn capture(&self, url: &str, opts: &Opts) -> Result<CapturedImage> {
        tokio::time::timeout(opts.timeout, async {
            let _permit = self
                .permits
                .acquire()
                .await
                .map_err(|_| anyhow!("capture queue closed"))?;
            let session = self.session().await?;
            let result = session.capture(url, opts).await;
            if result.is_err() && !session.is_healthy() {
                let mut current = self.session.lock().await;
                if current
                    .as_ref()
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &session))
                {
                    current.take();
                }
            }
            result
        })
        .await
        .map_err(|_| {
            anyhow!(
                "capture deadline exceeded ({}s, including queue and browser startup)",
                opts.timeout.as_secs()
            )
        })?
    }

    async fn close(&self) {
        let session = self.session.lock().await.take();
        if let Some(session) = session
            && let Ok(session) = Arc::try_unwrap(session)
        {
            session.close().await;
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
            .capture(prepared.url.as_str(), &prepared.opts)
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
            ContentBlock::image(image.data, prepared.opts.format.mime_type()),
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

    /// Capture several pages, viewports, or elements in one request using the warm browser.
    #[tool(
        name = "capture_batch",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn capture_batch(&self, Parameters(request): Parameters<BatchRequest>) -> CallToolResult {
        if request.captures.is_empty() || request.captures.len() > 16 {
            return simple_error("captures must contain between 1 and 16 requests".into());
        }
        let results = join_all(
            request
                .captures
                .into_iter()
                .map(|request| self.capture_image(request)),
        )
        .await;
        let mut content = Vec::new();
        let mut reports = Vec::with_capacity(results.len());
        let mut failed = 0;
        for (index, result) in results.into_iter().enumerate() {
            if result.is_error == Some(true) {
                failed += 1;
            }
            content.push(ContentBlock::text(format!("Capture {}", index + 1)));
            content.extend(result.content);
            reports.push(result.structured_content);
        }
        let status = if failed == 0 { "ok" } else { "partial_error" };
        let mut result = CallToolResult::success(content);
        result.is_error = Some(failed > 0);
        result.structured_content =
            Some(json!({ "status": status, "failed": failed, "captures": reports }));
        result
    }

    /// Compare saved screenshots by pixels, reporting changed area and an optional difference map.
    #[tool(
        name = "compare",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn compare(
        &self,
        Parameters(request): Parameters<CompareRequest>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let permit = match Arc::clone(&self.state.permits).acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => return simple_error(format!("comparison queue closed: {error}")),
        };
        let comparison = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::compare::compare(
                &request.before,
                &request.after,
                request.threshold.unwrap_or(0),
                request.output.as_deref(),
                || context.ct.is_cancelled(),
            )
        })
        .await;
        match comparison {
            Ok(Ok(report)) => {
                let summary = format!(
                    "{} of {} pixels changed ({:.2}%)",
                    report.changed_pixels,
                    report.total_pixels,
                    report.changed_fraction * 100.0
                );
                let mut result = CallToolResult::success(vec![ContentBlock::text(summary)]);
                result.structured_content = serde_json::to_value(report).ok();
                result
            }
            Ok(Err(error)) => simple_error(format!("{error:#}")),
            Err(error) => simple_error(format!("image comparison failed: {error}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for IrisServer {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let cancelled = context.ct.clone();
        let call = ToolCallContext::new(self, request, context);
        tokio::select! {
            biased;
            () = cancelled.cancelled() => Ok(simple_error("request cancelled".into()).into()),
            result = self.tool_router.call(call) => result,
        }
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("iris", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Iris captures screenshots without clicks or login. Use capture for one page or element, capture_batch for independent views, and compare for saved before/after images. Captures include timing, asset warnings, and the final URL. Use max_pixels to control image size, freeze_animations to finish finite animations and pause repeating ones, and output only when a durable file is needed.",
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
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    fn request(url: &str) -> CaptureRequest {
        CaptureRequest {
            url: url.into(),
            selector: None,
            padding: None,
            full_page: false,
            size: None,
            dark: false,
            format: None,
            quality: None,
            max_pixels: None,
            freeze_animations: false,
            wait_ms: 0,
            wait_for: None,
            scale: None,
            timeout_seconds: None,
            output: None,
        }
    }

    #[test]
    fn conflicting_capture_modes_are_rejected() {
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
    fn output_extension_overrides_requested_format() {
        let mut capture = request("example.com");
        capture.format = Some(ImageFormat::Png);
        capture.output = Some(PathBuf::from("shot.webp"));
        let prepared = capture.prepare().unwrap();
        assert_eq!(prepared.opts.format, Format::Webp);
    }

    #[tokio::test]
    async fn capture_tool_returns_inline_pixels_metadata_and_optional_file() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
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

    #[tokio::test]
    async fn batch_deadline_includes_time_queued_behind_earlier_captures() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/full-page-layout.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let state = Arc::new(McpState::new(None));
        let server = IrisServer::new(Arc::clone(&state));
        let captures = (0..5)
            .map(|index| {
                let mut capture = request(url.as_str());
                capture.size = Some("320x240".into());
                capture.scale = Some(1.0);
                capture.timeout_seconds = Some(if index < 4 { 30 } else { 1 });
                capture.wait_ms = if index < 4 { 1500 } else { 0 };
                capture
            })
            .collect();
        let result = server
            .capture_batch(Parameters(BatchRequest { captures }))
            .await;
        state.close().await;
        let report = result.structured_content.context("batch report missing")?;
        let captures = report["captures"]
            .as_array()
            .context("capture reports missing")?;
        assert!(
            captures[..4]
                .iter()
                .all(|capture| capture["status"] == "ok"),
            "{report}"
        );
        assert_eq!(captures[4]["status"], "error");
        assert!(
            captures[4]["error"]
                .as_str()
                .is_some_and(|error| error.contains("deadline exceeded")),
            "{report}"
        );
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
