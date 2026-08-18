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
    /// Emulate prefers-color-scheme: dark.
    #[serde(default)]
    dark: bool,
    /// Image format. Defaults to png; a recognized output extension wins.
    format: Option<ImageFormat>,
    /// Extra settle delay in milliseconds after smart waiting.
    #[serde(default)]
    wait_ms: u64,
    /// Wait until this CSS selector exists before capturing.
    wait_for: Option<String>,
    /// Device scale factor overriding the viewport preset.
    scale: Option<f64>,
    /// Per-page timeout in seconds. Defaults to 30.
    timeout_seconds: Option<u64>,
    /// Optional image path. Relative paths resolve from the MCP server working directory.
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
            },
        })
    }
}

struct McpState {
    chrome: Option<PathBuf>,
    session: Mutex<Option<Arc<Session>>>,
    permits: Semaphore,
}

impl McpState {
    fn new(chrome: Option<PathBuf>) -> Self {
        Self {
            chrome,
            session: Mutex::new(None),
            permits: Semaphore::new(4),
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
            output: None,
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
