use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, Viewport as ScreenshotViewport,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::error::CdpError;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use url::Url;

const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
static PROFILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ProfileDir {
    path: PathBuf,
    persistent: bool,
}

impl ProfileDir {
    fn unique() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "iris-chrome-{}-{}",
                std::process::id(),
                PROFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )),
            persistent: false,
        }
    }

    fn persistent(path: PathBuf) -> Self {
        Self {
            path,
            persistent: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProfileDir {
    fn drop(&mut self) {
        if !self.persistent {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Reject names that could escape the sessions directory or confuse shells.
pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name.len() > 64 {
        bail!("session name is too long (max 64 characters)");
    }
    if name == "." || name == ".." {
        bail!("invalid session name {name:?}");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid session name {name:?}: use letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

fn data_root() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/iris"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|base| base.join("iris"));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|base| base.join("iris"));
    base.ok_or_else(|| anyhow!("cannot locate a data directory for sessions"))
}

/// Directory holding a named session's browser profile, creating nothing.
pub fn session_dir(name: &str) -> Result<PathBuf> {
    let name = name.trim();
    validate_session_name(name)?;
    Ok(data_root()?.join("sessions").join(name))
}

/// Resolve a session saved by `iris login`, erroring when it does not exist.
pub fn resolve_session_dir(name: &str) -> Result<PathBuf> {
    let dir = session_dir(name)?;
    if !dir.is_dir() {
        bail!(
            "unknown session {:?}: save it first with `iris login --session {:?} <url>`",
            name.trim(),
            name.trim()
        );
    }
    Ok(dir)
}

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub mobile: bool,
}

impl Viewport {
    pub fn desktop() -> Self {
        Self {
            width: 1440,
            height: 900,
            scale: 2.0,
            mobile: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Png,
    Jpeg,
    Webp,
}

impl Format {
    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    fn cdp(self) -> CaptureScreenshotFormat {
        match self {
            Self::Png => CaptureScreenshotFormat::Png,
            Self::Jpeg => CaptureScreenshotFormat::Jpeg,
            Self::Webp => CaptureScreenshotFormat::Webp,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    Viewport,
    FullPage,
    Element { selector: String, padding: u32 },
}

impl CaptureMode {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Viewport => "viewport",
            Self::FullPage => "full_page",
            Self::Element { .. } => "element",
        }
    }

    pub fn selector(&self) -> Option<&str> {
        match self {
            Self::Element { selector, .. } => Some(selector),
            _ => None,
        }
    }

    pub fn padding(&self) -> Option<u32> {
        match self {
            Self::Element { padding, .. } => Some(*padding),
            _ => None,
        }
    }
}

/// One numbered marker + label pointing at the first element matching `selector`.
/// Numbering follows flag order; labels are auto-placed to avoid covering the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    pub selector: String,
    pub label: String,
    pub number: u32,
}

/// Forced `prefers-color-scheme` emulation. `System` applies no override and
/// preserves historical output; `Light`/`Dark` force the scheme explicitly so
/// output never depends on the machine's OS theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    Light,
    Dark,
    System,
}

impl ColorScheme {
    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    fn css_value(self) -> Option<&'static str> {
        match self {
            Self::Light => Some("light"),
            Self::Dark => Some("dark"),
            Self::System => None,
        }
    }
}

#[derive(Debug)]
pub struct Opts {
    pub viewport: Viewport,
    pub mode: CaptureMode,
    pub color_scheme: ColorScheme,
    pub wait_ms: u64,
    pub wait_for: Option<String>,
    pub timeout: Duration,
    pub format: Format,
    pub annotations: Vec<Annotation>,
    pub highlights: Vec<String>,
    pub dim: bool,
    pub steps: Vec<InteractionStep>,
    /// CSS selectors to redact; every match is covered (not just the first).
    pub masks: Vec<String>,
    /// Blur radius in px for masks; opaque ink when None.
    pub mask_blur_px: Option<u32>,
    /// Opt-in sensitive-data detectors; off when empty.
    pub mask_patterns: Vec<MaskPattern>,
}

impl Opts {
    pub fn overlays_enabled(&self) -> bool {
        !self.annotations.is_empty() || !self.highlights.is_empty() || self.dim
    }

    pub fn masking_enabled(&self) -> bool {
        !self.masks.is_empty() || !self.mask_patterns.is_empty()
    }
}

/// One pre-capture interaction, run in order after navigation settles.
/// Selectors use first-match semantics; `click` also accepts `text=...` to
/// match the deepest visible element containing the given text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractionStep {
    Click { selector: String },
    Fill { selector: String, text: String },
    Hover { selector: String },
    Press { key: String },
    WaitFor { selector: String },
}

impl InteractionStep {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::Fill { .. } => "fill",
            Self::Hover { .. } => "hover",
            Self::Press { .. } => "press",
            Self::WaitFor { .. } => "wait_for",
        }
    }
}

/// Opt-in sensitive-data detector for masking. Conservative by design:
/// patterns require separators and word boundaries so plain numbers and
/// order IDs are left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MaskPattern {
    Email,
    Phone,
    Ssn,
    Account,
}

impl MaskPattern {
    pub fn name(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Ssn => "ssn",
            Self::Account => "account",
        }
    }

    /// JavaScript `RegExp` source (no flags, no delimiters).
    pub fn regex_source(self) -> &'static str {
        match self {
            Self::Email => r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            Self::Phone => r"(?:\+?1[-.\s]?)?(?:\(\d{3}\)|\d{3})[-.\s]\d{3}[-.\s]\d{4}",
            Self::Ssn => r"\b\d{3}-\d{2}-\d{4}\b",
            Self::Account => r"\b\d{4}(?:[- ]\d{4}){2,3}\b",
        }
    }
}

/// One overlay box in document coordinates, returned by the in-page overlay
/// script so element clips can grow to include nearby markers and labels.
#[derive(Debug, Deserialize)]
struct OverlayBox {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug)]
pub struct Shot {
    /// Captured width and height in CSS pixels.
    pub width: u32,
    pub height: u32,
    /// Device scale factor actually used (full-page shots too tall for Chrome's
    /// ~16k texture limit fall back to 1x).
    pub scale: f64,
    pub bytes: u64,
    /// Redaction boxes drawn before capture.
    pub masked: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuccessReport {
    status: &'static str,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    padding: Option<u32>,
    css_width: u32,
    css_height: u32,
    scale: f64,
    format: &'static str,
    bytes: u64,
    color_scheme: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    masked: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorReport {
    status: &'static str,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    padding: Option<u32>,
    error: String,
}

pub fn success_report(
    url: &str,
    output: Option<&Path>,
    mode: &CaptureMode,
    format: Format,
    shot: &Shot,
    annotations: usize,
    color_scheme: ColorScheme,
) -> SuccessReport {
    SuccessReport {
        status: "ok",
        url: url.into(),
        output: output.map(absolute_output),
        mode: mode.name(),
        selector: mode.selector().map(str::to_owned),
        padding: mode.padding(),
        css_width: shot.width,
        css_height: shot.height,
        scale: shot.scale,
        format: format.ext(),
        bytes: shot.bytes,
        color_scheme: color_scheme.name(),
        annotations: (annotations > 0).then_some(annotations),
        masked: (shot.masked > 0).then_some(shot.masked),
    }
}

pub fn error_report(
    url: &str,
    output: Option<&Path>,
    mode: &CaptureMode,
    error: String,
) -> ErrorReport {
    ErrorReport {
        status: "error",
        url: url.into(),
        output: output.map(absolute_output),
        mode: mode.name(),
        selector: mode.selector().map(str::to_owned),
        padding: mode.padding(),
        error,
    }
}

pub fn absolute_output(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug)]
pub struct CapturedImage {
    pub shot: Shot,
    pub data: Vec<u8>,
}

impl CapturedImage {
    pub async fn write_to(&self, out: &Path) -> Result<()> {
        if let Some(parent) = out.parent().filter(|path| !path.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        tokio::fs::write(out, &self.data)
            .await
            .with_context(|| format!("failed to write {}", out.display()))
    }
}

#[derive(Debug, Deserialize)]
struct ElementBounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    doc_width: f64,
    doc_height: f64,
}

#[derive(Debug, PartialEq)]
struct ClipRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

pub struct Session {
    browser: Browser,
    handler: JoinHandle<()>,
    // Declared after Browser so fallback field-drop cleanup happens after
    // Chromiumoxide has stopped its child process.
    profile_dir: ProfileDir,
    /// Browser's real UA with "HeadlessChrome" scrubbed, so sites don't serve degraded pages.
    user_agent: Option<String>,
}

impl Session {
    pub async fn launch(chrome: Option<PathBuf>, viewport: Viewport) -> Result<Self> {
        Self::start(chrome, viewport, ProfileDir::unique(), false).await
    }

    /// Launch against a named session's directory, keeping cookies and storage
    /// on disk after close. The directory must already exist.
    pub async fn launch_with_profile(
        chrome: Option<PathBuf>,
        viewport: Viewport,
        dir: PathBuf,
    ) -> Result<Self> {
        Self::start(chrome, viewport, ProfileDir::persistent(dir), false).await
    }

    /// Launch a visible browser for `login_and_wait`: the user signs in by hand.
    pub async fn launch_headed(
        chrome: Option<PathBuf>,
        viewport: Viewport,
        dir: PathBuf,
    ) -> Result<Self> {
        Self::start(chrome, viewport, ProfileDir::persistent(dir), true).await
    }

    /// Open `url` and wait for the user to press Enter (or close stdin),
    /// leaving the profile on disk for later `--session` captures.
    pub async fn login_and_wait(&self, url: &str) -> Result<()> {
        let page = self.browser.new_page(url).await?;
        tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)
        })
        .await
        .context("failed to wait for sign-in confirmation")?
        .context("failed to read sign-in confirmation")?;
        let _ = page.close().await;
        Ok(())
    }

    async fn start(
        chrome: Option<PathBuf>,
        viewport: Viewport,
        profile_dir: ProfileDir,
        headed: bool,
    ) -> Result<Self> {
        let mut config = BrowserConfig::builder()
            .window_size(viewport.width, viewport.height)
            .user_data_dir(profile_dir.path());
        if headed {
            config = config.with_head();
        }
        if let Some(path) = chrome.or_else(find_chrome) {
            config = config.chrome_executable(path);
        }
        let config = config.build().map_err(|e| anyhow!(e))?;
        let (browser, mut handler) = Browser::launch(config)
            .await
            .context("failed to launch Chrome (install Google Chrome or pass --chrome)")?;
        // Newer Chrome versions emit CDP messages chromiumoxide can't deserialize
        // (Serde errors) — harmless, keep pumping. Transport errors mean the
        // connection is gone, so stop.
        let handler = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                match event {
                    Ok(_) | Err(CdpError::Serde(_)) => {}
                    Err(_) => break,
                }
            }
        });
        let user_agent = browser
            .version()
            .await
            .ok()
            .map(|v| v.user_agent.replace("HeadlessChrome", "Chrome"));
        Ok(Self {
            browser,
            handler,
            profile_dir,
            user_agent,
        })
    }

    pub async fn capture(&self, url: &str, opts: &Opts) -> Result<CapturedImage> {
        let page = tokio::time::timeout(opts.timeout, self.browser.new_page("about:blank"))
            .await
            .map_err(|_| anyhow!("timed out opening a tab"))??;
        let result = tokio::time::timeout(opts.timeout, self.pipeline(&page, url, opts))
            .await
            .map_err(|_| anyhow!("timed out after {}s", opts.timeout.as_secs()))
            .and_then(|r| r);
        let _ = page.close().await;
        result
    }

    pub fn is_healthy(&self) -> bool {
        !self.handler.is_finished()
    }

    async fn pipeline(&self, page: &Page, url: &str, opts: &Opts) -> Result<CapturedImage> {
        let started = std::time::Instant::now();
        let v = opts.viewport;
        page.execute(
            SetDeviceMetricsOverrideParams::builder()
                .width(v.width as i64)
                .height(v.height as i64)
                .device_scale_factor(v.scale)
                .mobile(v.mobile)
                .build()
                .map_err(|e| anyhow!(e))?,
        )
        .await?;

        let ua = if v.mobile {
            Some(IPHONE_UA.to_string())
        } else {
            self.user_agent.clone()
        };
        if let Some(ua) = ua {
            page.execute(
                SetUserAgentOverrideParams::builder()
                    .user_agent(ua)
                    .build()
                    .map_err(|e| anyhow!(e))?,
            )
            .await?;
        }

        if let Some(value) = opts.color_scheme.css_value() {
            page.execute(
                SetEmulatedMediaParams::builder()
                    .feature(MediaFeature {
                        name: "prefers-color-scheme".into(),
                        value: value.into(),
                    })
                    .build(),
            )
            .await?;
        }

        page.goto(url).await?;
        page.wait_for_navigation().await?;

        self.eval(page, SETTLE_JS.into()).await?;
        if let Some(selector) = &opts.wait_for {
            // Undercut the outer timeout so the descriptive selector error surfaces
            // instead of a generic "timed out".
            let budget = opts
                .timeout
                .saturating_sub(started.elapsed())
                .saturating_sub(Duration::from_millis(500));
            self.eval(page, wait_for_js(selector, budget.as_millis() as u64))
                .await?;
        }
        if !opts.steps.is_empty() {
            for (index, step) in opts.steps.iter().enumerate() {
                let budget = opts
                    .timeout
                    .saturating_sub(started.elapsed())
                    .saturating_sub(Duration::from_millis(500));
                self.eval(page, interaction_js(step, index, budget.as_millis() as u64))
                    .await?;
            }
            // Interactions can start image loads, IntersectionObservers, and
            // transitions that the initial settle could not see.
            self.eval(page, SETTLE_JS.into()).await?;
        }
        match &opts.mode {
            CaptureMode::Viewport => {}
            CaptureMode::FullPage => {
                self.eval(page, SCROLL_JS.into()).await?;
            }
            CaptureMode::Element { selector, .. } => {
                let budget = opts
                    .timeout
                    .saturating_sub(started.elapsed())
                    .saturating_sub(Duration::from_millis(500));
                self.eval(
                    page,
                    wait_and_scroll_js(selector, budget.as_millis() as u64),
                )
                .await?;
                // Scrolling can start image loads, IntersectionObservers, and
                // entrance transitions that the initial settle could not see.
                self.eval(page, SETTLE_JS.into()).await?;
            }
        }
        if opts.wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(opts.wait_ms)).await;
            self.eval(page, SETTLE_JS.into()).await?;
        }

        // Masks run before overlays so annotations can point at redacted boxes
        // without leaking their text. Both are plain DOM nodes; the tab closes
        // after capture, so no cleanup is needed.
        let masked = if opts.masking_enabled() {
            self.apply_masks(page, opts).await?
        } else {
            0
        };
        let overlay_boxes = if opts.overlays_enabled() {
            self.apply_overlays(page, opts).await?
        } else {
            Vec::new()
        };

        let screenshot_params = || {
            let mut params = ScreenshotParams::builder().format(opts.format.cdp());
            if opts.format != Format::Png {
                params = params.quality(90);
            }
            params
        };

        let (width, height, scale, data) = match &opts.mode {
            CaptureMode::Viewport => (
                v.width,
                v.height,
                v.scale,
                page.screenshot(screenshot_params().build()).await?,
            ),
            CaptureMode::FullPage => {
                let doc_h = self
                    .eval_u32(page, DOC_HEIGHT_JS)
                    .await
                    .unwrap_or(v.height)
                    .max(v.height);
                if doc_h as f64 * v.scale <= 16_000.0 {
                    // Retina full page: grow the viewport to the whole document so the
                    // scale factor still applies (CDP's captureBeyondViewport renders at 1x).
                    page.execute(
                        SetDeviceMetricsOverrideParams::builder()
                            .width(v.width as i64)
                            .height(doc_h as i64)
                            .device_scale_factor(v.scale)
                            .mobile(v.mobile)
                            .build()
                            .map_err(|e| anyhow!(e))?,
                    )
                    .await?;
                    self.eval(page, SETTLE_JS.into()).await?;
                    (
                        v.width,
                        doc_h,
                        v.scale,
                        page.screenshot(screenshot_params().build()).await?,
                    )
                } else {
                    (
                        v.width,
                        doc_h,
                        1.0,
                        page.screenshot(screenshot_params().full_page(true).build())
                            .await?,
                    )
                }
            }
            CaptureMode::Element { selector, padding } => {
                let value = self.eval(page, element_bounds_js(selector)).await?;
                let bounds: ElementBounds = serde_json::from_value(value)
                    .context("failed to read selected element bounds")?;
                let clip = round_clip(&bounds, *padding)
                    .with_context(|| format!("cannot capture selected element: {selector}"))?;
                // Grow the clip to include nearby markers and labels so
                // annotations are never cropped out of element captures.
                let clip = union_clip(clip, &overlay_boxes, bounds.doc_width, bounds.doc_height);
                let cdp_clip = ScreenshotViewport::builder()
                    .x(clip.x)
                    .y(clip.y)
                    .width(clip.width)
                    .height(clip.height)
                    .scale(1.0)
                    .build()
                    .map_err(|e| anyhow!(e))?;
                (
                    clip.width as u32,
                    clip.height as u32,
                    v.scale,
                    page.screenshot(
                        screenshot_params()
                            .clip(cdp_clip)
                            .capture_beyond_viewport(true)
                            .build(),
                    )
                    .await?,
                )
            }
        };

        let bytes = data.len() as u64;
        Ok(CapturedImage {
            shot: Shot {
                width,
                height,
                scale,
                bytes,
                masked,
            },
            data,
        })
    }

    /// Run a JS expression (promises awaited); surface page-side exceptions as errors.
    async fn eval(&self, page: &Page, js: String) -> Result<serde_json::Value> {
        let params = EvaluateParams::builder()
            .expression(js)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .map_err(|e| anyhow!(e))?;
        let resp = page.execute(params).await?;
        if let Some(details) = &resp.result.exception_details {
            let msg = details
                .exception
                .as_ref()
                .and_then(|e| e.description.clone())
                .unwrap_or_else(|| details.text.clone());
            bail!("{}", msg.lines().next().unwrap_or("page script failed"));
        }
        Ok(resp
            .result
            .result
            .value
            .clone()
            .unwrap_or(serde_json::Value::Null))
    }

    async fn eval_u32(&self, page: &Page, js: &str) -> Result<u32> {
        let value = self.eval(page, js.into()).await?;
        value
            .as_f64()
            .map(|n| n as u32)
            .ok_or_else(|| anyhow!("expected a number from page"))
    }

    /// Draw annotation markers, labels, highlights, and dimming in-page and
    /// return every overlay box in document coordinates.
    async fn apply_overlays(&self, page: &Page, opts: &Opts) -> Result<Vec<OverlayBox>> {
        let value = self.eval(page, overlay_js(opts)).await?;
        serde_json::from_value(value).context("failed to read overlay boxes")
    }

    /// Cover masked elements and sensitive-data matches in-page; return how
    /// many boxes were drawn.
    async fn apply_masks(&self, page: &Page, opts: &Opts) -> Result<u64> {
        let value = self.eval(page, mask_js(opts)).await?;
        value
            .as_u64()
            .ok_or_else(|| anyhow!("expected a mask count from page"))
    }

    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.browser.close()).await;
        if tokio::time::timeout(Duration::from_secs(3), self.browser.wait())
            .await
            .is_err()
        {
            let _ = self.browser.kill().await;
        }
        self.handler.abort();
        // Named sessions keep their profile for later reuse; temp profiles
        // are removed (the Drop impl covers abnormal exits too).
        if !self.profile_dir.persistent {
            let _ = tokio::fs::remove_dir_all(self.profile_dir.path()).await;
        }
    }
}

pub fn parse_viewport(size: &str, scale: Option<f64>) -> Result<Viewport> {
    let (width, height, preset_scale, mobile) = match size {
        "desktop" => (1440, 900, 2.0, false),
        "iphone" => (390, 844, 3.0, true),
        "ipad" => (1024, 1366, 2.0, false),
        custom => {
            let (width, height) = custom
                .split_once(['x', 'X'])
                .and_then(|(width, height)| {
                    Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?))
                })
                .with_context(|| {
                    format!(
                        "invalid size {custom:?}: use WxH (e.g. 1440x900) or desktop|iphone|ipad"
                    )
                })?;
            if width == 0 || height == 0 {
                bail!("invalid size {custom:?}: width and height must be greater than zero");
            }
            (width, height, 2.0, false)
        }
    };
    let scale = scale.unwrap_or(preset_scale);
    if !scale.is_finite() || scale <= 0.0 {
        bail!("invalid scale {scale}: use a finite number greater than zero");
    }
    Ok(Viewport {
        width,
        height,
        scale,
        mobile,
    })
}

pub fn normalize_url(raw: &str) -> Result<Url> {
    if raw.contains("://") {
        return Url::parse(raw).with_context(|| format!("invalid URL: {raw}"));
    }

    let http =
        Url::parse(&format!("http://{raw}")).with_context(|| format!("invalid URL: {raw}"))?;
    let local = http.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host.to_ascii_lowercase().ends_with(".localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback() || address.is_unspecified())
    });
    if local {
        Ok(http)
    } else {
        Url::parse(&format!("https://{raw}")).with_context(|| format!("invalid URL: {raw}"))
    }
}

/// Prefer real installed browser apps; PATH entries can be stale wrapper scripts
/// (e.g. a Homebrew cask whose app was deleted). Falls back to chromiumoxide's
/// own detection when none of these exist.
fn find_chrome() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    #[cfg(windows)]
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    #[cfg(not(any(target_os = "macos", windows)))]
    const CANDIDATES: &[&str] = &[
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Fonts loaded, near-viewport images loaded (3s cap each), two frames painted,
/// then any running finite animations/transitions — entrance fade-ins — allowed
/// to finish (3s cap; infinite loops are skipped, they never settle). Off-screen
/// images are ignored: they don't appear in the capture, and lazy-loaded ones
/// would stall the wait forever. Full-page captures grow the viewport to the
/// whole document before the final settle, so everything counts as near there.
const SETTLE_JS: &str = r#"(async () => {
  if (document.fonts) { try { await document.fonts.ready; } catch {} }
  const near = (img) => {
    const r = img.getBoundingClientRect();
    return r.top < innerHeight * 1.5 && r.bottom > -innerHeight * 0.5;
  };
  await Promise.all(Array.from(document.images)
    .filter(img => !img.complete && near(img))
    .map(img => new Promise(r => {
      img.addEventListener('load', r, { once: true });
      img.addEventListener('error', r, { once: true });
      setTimeout(r, 3000);
    })));
  await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
  const finite = document.getAnimations().filter(a => {
    try {
      return a.playState === 'running' && a.effect.getTiming().iterations !== Infinity;
    } catch { return false; }
  });
  await Promise.race([
    Promise.all(finite.map(a => a.finished.catch(() => {}))),
    new Promise(r => setTimeout(r, 3000)),
  ]);
  await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
})()"#;

/// Step-scroll to the bottom so IntersectionObserver lazy-loading fires, then back to top.
const SCROLL_JS: &str = r#"(async () => {
  const height = () => Math.max(
    document.body?.scrollHeight ?? 0,
    document.documentElement.scrollHeight
  );
  const step = Math.max(200, window.innerHeight);
  for (let y = 0, guard = 0; y < height() && guard < 500; y += step, guard++) {
    window.scrollTo(0, y);
    await new Promise(r => setTimeout(r, 60));
  }
  window.scrollTo(0, 0);
  await new Promise(r => setTimeout(r, 150));
  await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
})()"#;

const DOC_HEIGHT_JS: &str = r#"Math.max(
  document.body?.scrollHeight ?? 0,
  document.documentElement.scrollHeight
)"#;

fn wait_for_js(selector: &str, budget_ms: u64) -> String {
    let selector = serde_json::to_string(selector).expect("selector is serializable");
    format!(
        r#"(async () => {{
  const deadline = Date.now() + {budget_ms};
  const selector = {selector};
  const find = () => {{
    try {{ return document.querySelector(selector); }}
    catch {{ throw new Error("invalid selector: " + selector); }}
  }};
  while (!find()) {{
    if (Date.now() >= deadline) throw new Error("selector never appeared: " + selector);
    await new Promise(r => setTimeout(r, 100));
  }}
}})()"#
    )
}

fn wait_and_scroll_js(selector: &str, budget_ms: u64) -> String {
    let selector = serde_json::to_string(selector).expect("selector is serializable");
    format!(
        r#"(async () => {{
  const deadline = Date.now() + {budget_ms};
  const selector = {selector};
  const find = () => {{
    try {{ return document.querySelector(selector); }}
    catch {{ throw new Error("invalid selector: " + selector); }}
  }};
  let element;
  while (!(element = find())) {{
    if (Date.now() >= deadline) throw new Error("selector never appeared: " + selector);
    await new Promise(r => setTimeout(r, 100));
  }}
  element.scrollIntoView({{ block: "center", inline: "center" }});
  await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
}})()"#
    )
}

fn element_bounds_js(selector: &str) -> String {
    let selector = serde_json::to_string(selector).expect("selector is serializable");
    format!(
        r#"(() => {{
  const selector = {selector};
  let element;
  try {{ element = document.querySelector(selector); }}
  catch {{ throw new Error("invalid selector: " + selector); }}
  if (!element) throw new Error("selector disappeared: " + selector);
  const rect = element.getBoundingClientRect();
  if (![rect.x, rect.y, rect.width, rect.height].every(Number.isFinite) ||
      rect.width <= 0 || rect.height <= 0) {{
    throw new Error("element has no rendered size: " + selector);
  }}
  return {{
    x: rect.left + window.scrollX,
    y: rect.top + window.scrollY,
    width: rect.width,
    height: rect.height,
    doc_width: Math.max(
      document.body?.scrollWidth ?? 0,
      document.documentElement.scrollWidth,
      document.documentElement.clientWidth
    ),
    doc_height: Math.max(
      document.body?.scrollHeight ?? 0,
      document.documentElement.scrollHeight,
      document.documentElement.clientHeight
    )
  }};
}})()"#
    )
}

/// Build the in-page script for one interaction step. Each script polls for its
/// target inside the remaining budget and throws a step-indexed error, which
/// `eval` surfaces instead of a generic timeout.
fn interaction_js(step: &InteractionStep, index: usize, budget_ms: u64) -> String {
    let label = format!("step {} ({})", index + 1, step.kind());
    let label = serde_json::to_string(&label).expect("step label is serializable");
    let prelude = format!(
        r##"const deadline = Date.now() + {budget_ms};
  const label = {label};
  const sleep = (ms) => new Promise(r => setTimeout(r, ms));
  const frames = () => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
  const query = (selector) => {{
    try {{ return document.querySelector(selector); }}
    catch {{ throw new Error("invalid selector: " + selector); }}
  }};"##
    );
    match step {
        InteractionStep::Click { selector } => {
            let target = serde_json::to_string(selector).expect("selector is serializable");
            format!(
                r##"(async () => {{
  {prelude}
  const target = {target};
  const find = () => {{
    if (target.startsWith("text=")) {{
      const needle = target.slice(5).trim().toLowerCase();
      if (!needle) throw new Error(label + ": text= needs a value to match");
      // Reverse document order finds the deepest visible match first, so a
      // button wins over the containers holding it.
      const all = Array.from(document.querySelectorAll("body *")).reverse();
      for (const el of all) {{
        const rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) continue;
        const text = ((el.innerText ?? el.textContent) || "").trim().toLowerCase();
        if (text && text.includes(needle)) return el;
      }}
      return null;
    }}
    return query(target);
  }};
  let element = null;
  while (!(element = find())) {{
    if (Date.now() >= deadline) throw new Error(label + ": target never appeared: " + target);
    await sleep(100);
  }}
  element.scrollIntoView({{ block: "center", inline: "center" }});
  await frames();
  element.click();
}})()"##
            )
        }
        InteractionStep::Fill { selector, text } => {
            let selector = serde_json::to_string(selector).expect("selector is serializable");
            let text = serde_json::to_string(text).expect("fill text is serializable");
            format!(
                r##"(async () => {{
  {prelude}
  const selector = {selector};
  const text = {text};
  let element = null;
  while (!(element = query(selector))) {{
    if (Date.now() >= deadline) throw new Error(label + ": target never appeared: " + selector);
    await sleep(100);
  }}
  element.scrollIntoView({{ block: "center", inline: "center" }});
  await frames();
  const fire = (type) => element.dispatchEvent(new Event(type, {{ bubbles: true }}));
  if (element.isContentEditable) {{
    element.focus();
    document.execCommand("selectAll", false, null);
    if (!document.execCommand("insertText", false, text)) element.textContent = text;
    fire("input");
  }} else if (element.tagName === "SELECT") {{
    const wanted = text.trim().toLowerCase();
    const options = Array.from(element.options);
    const match = options.find(o => o.value.toLowerCase() === wanted)
      || options.find(o => (o.text || "").trim().toLowerCase() === wanted);
    if (!match) throw new Error(label + ": no option matches: " + text);
    element.value = match.value;
    fire("input");
    fire("change");
  }} else if (element.tagName === "INPUT" && ["checkbox", "radio"].includes(element.type)) {{
    const wanted = text.trim().toLowerCase();
    element.checked = ["true", "1", "check", "checked", "on", "yes"].includes(wanted);
    fire("input");
    fire("change");
  }} else if ("value" in element) {{
    // The native setter (not a plain assignment) notifies framework bindings.
    const proto = element.tagName === "TEXTAREA"
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(element, text);
    fire("input");
    fire("change");
  }} else {{
    throw new Error(label + ": cannot fill this element: " + selector);
  }}
}})()"##
            )
        }
        InteractionStep::Hover { selector } => {
            let selector = serde_json::to_string(selector).expect("selector is serializable");
            format!(
                r##"(async () => {{
  {prelude}
  const selector = {selector};
  let element = null;
  while (!(element = query(selector))) {{
    if (Date.now() >= deadline) throw new Error(label + ": target never appeared: " + selector);
    await sleep(100);
  }}
  element.scrollIntoView({{ block: "center", inline: "center" }});
  await frames();
  // Synthetic events reach JS listeners; CSS :hover may not apply to them.
  try {{ element.focus({{ preventScroll: true }}); }} catch {{}}
  const rect = element.getBoundingClientRect();
  const at = {{ bubbles: true, cancelable: true, view: window,
    clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2 }};
  element.dispatchEvent(new PointerEvent("pointerover", at));
  element.dispatchEvent(new MouseEvent("mouseover", at));
  element.dispatchEvent(new MouseEvent("mouseenter", at));
}})()"##
            )
        }
        InteractionStep::Press { key } => {
            let key = serde_json::to_string(key).expect("key is serializable");
            format!(
                r##"(() => {{
  {prelude}
  const key = {key};
  if (!key) throw new Error(label + ": press needs a key");
  const target = document.activeElement || document.body;
  for (const type of ["keydown", "keypress", "keyup"]) {{
    target.dispatchEvent(new KeyboardEvent(type, {{ key, bubbles: true, cancelable: true, view: window }}));
  }}
}})()"##
            )
        }
        InteractionStep::WaitFor { selector } => {
            let selector = serde_json::to_string(selector).expect("selector is serializable");
            format!(
                r##"(async () => {{
  {prelude}
  const selector = {selector};
  while (!query(selector)) {{
    if (Date.now() >= deadline) throw new Error(label + ": target never appeared: " + selector);
    await sleep(100);
  }}
}})()"##
            )
        }
    }
}

/// Cover masked elements with ink (or blur) and wrap sensitive-data matches in
/// covered spans. Explicit selectors cover every match; enabled patterns scan
/// text nodes and form values. Returns the number of boxes drawn.
fn mask_js(opts: &Opts) -> String {
    let selectors = serde_json::to_string(&opts.masks).expect("masks are serializable");
    let blur = match opts.mask_blur_px {
        Some(px) => px.to_string(),
        None => "null".into(),
    };
    let patterns: Vec<serde_json::Value> = opts
        .mask_patterns
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name(),
                "source": p.regex_source(),
            })
        })
        .collect();
    let patterns = serde_json::to_string(&patterns).expect("patterns are serializable");
    format!(
        r##"(() => {{
  const selectors = {selectors};
  const blurPx = {blur};
  const patterns = {patterns};
  let count = 0;
  const cover = (x, y, w, h) => {{
    if (!(w > 0 && h > 0) || ![x, y, w, h].every(Number.isFinite)) return;
    const veil = document.createElement("div");
    veil.dataset.iris = "mask";
    let css = "position:absolute;left:" + x + "px;top:" + y + "px;"
      + "width:" + w + "px;height:" + h + "px;z-index:2147483645;"
      + "pointer-events:none;margin:0;padding:0;";
    if (blurPx === null) css += "background:#111310;";
    else css += "background:rgba(17,19,16,.15);backdrop-filter:blur(" + blurPx + "px);"
      + "-webkit-backdrop-filter:blur(" + blurPx + "px);";
    veil.style.cssText = css;
    document.body.appendChild(veil);
    count++;
  }};
  const box = (rect) => cover(
    rect.left + window.scrollX, rect.top + window.scrollY, rect.width, rect.height,
  );
  for (const selector of selectors) {{
    let nodes;
    try {{ nodes = Array.from(document.querySelectorAll(selector)); }}
    catch {{ throw new Error("invalid selector: " + selector); }}
    for (const el of nodes) box(el.getBoundingClientRect());
  }}
  if (patterns.length > 0) {{
    const matchers = patterns.map(p => new RegExp(p.source, "g"));
    const skipped = (node) => {{
      const el = node.parentElement;
      return !el || el.closest("script,style,noscript,[data-iris]");
    }};
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    const byNode = new Map();
    while (walker.nextNode()) {{
      const node = walker.currentNode;
      if (skipped(node)) continue;
      for (const rx of matchers) {{
        rx.lastIndex = 0;
        let m;
        while ((m = rx.exec(node.data))) {{
          if (m[0].length === 0) {{ rx.lastIndex++; continue; }}
          if (!byNode.has(node)) byNode.set(node, []);
          byNode.get(node).push([m.index, m.index + m[0].length]);
        }}
      }}
    }}
    // Back-to-front per node so earlier offsets survive each split.
    for (const [node, ranges] of byNode) {{
      if (!node.isConnected) continue;
      ranges.sort((a, b) => b[0] - a[0]);
      for (const [start, end] of ranges) {{
        if (start < 0 || end > node.data.length || start >= end) continue;
        const range = document.createRange();
        range.setStart(node, start);
        range.setEnd(node, end);
        const span = document.createElement("span");
        span.dataset.iris = "redact";
        try {{ range.surroundContents(span); }} catch {{ continue; }}
      }}
    }}
    for (const span of document.querySelectorAll('span[data-iris="redact"]')) {{
      box(span.getBoundingClientRect());
    }}
    // Form values are not text nodes: cover whole inputs whose value matches.
    const testers = matchers.map(rx => new RegExp(rx.source, ""));
    for (const el of document.querySelectorAll("input,textarea")) {{
      const value = el.value || "";
      if (!value) continue;
      if (testers.some(rx => rx.test(value))) box(el.getBoundingClientRect());
    }}
  }}
  return count;
}})()"##
    )
}

/// Draw numbered markers, labels, outlines, and dimming as in-page DOM nodes.
/// Returns every overlay box in document coordinates so element clips can grow
/// to include them. Labels auto-place: prefer the right of the target, fall
/// back to the left, then clamp into the document — never over the target.
fn overlay_js(opts: &Opts) -> String {
    let points: Vec<serde_json::Value> = opts
        .annotations
        .iter()
        .map(|a| {
            serde_json::json!({
                "selector": a.selector,
                "label": a.label,
                "number": a.number,
            })
        })
        .collect();
    let points = serde_json::to_string(&points).expect("annotations are serializable");
    let highlights = serde_json::to_string(&opts.highlights).expect("highlights are serializable");
    let dim = if opts.dim { "true" } else { "false" };
    format!(
        r##"(() => {{
  const points = {points};
  const highlights = {highlights};
  const dim = {dim};
  const boxes = [];
  const find = (selector) => {{
    try {{ return document.querySelector(selector); }}
    catch {{ throw new Error("invalid selector: " + selector); }}
  }};
  const doc_width = Math.max(
    document.body?.scrollWidth ?? 0,
    document.documentElement.scrollWidth,
    document.documentElement.clientWidth
  );
  const ORANGE = "#ff4d00";
  const INK = "#141210";
  if (dim) {{
    const veil = document.createElement("div");
    veil.dataset.iris = "dim";
    veil.style.cssText = "position:fixed;inset:0;background:rgba(12,10,8,.45);"
      + "z-index:2147483646;pointer-events:none;margin:0;padding:0;";
    document.documentElement.appendChild(veil);
  }}
  const outline = (x, y, w, h) => {{
    const frame = document.createElement("div");
    frame.dataset.iris = "frame";
    frame.style.cssText = "position:absolute;left:" + x + "px;top:" + y + "px;"
      + "width:" + w + "px;height:" + h + "px;border:3px solid " + ORANGE + ";"
      + "border-radius:6px;box-sizing:border-box;z-index:2147483647;"
      + "pointer-events:none;margin:0;padding:0;";
    document.body.appendChild(frame);
    boxes.push({{ x, y, width: w, height: h }});
  }};
  const lift = (element) => {{
    // Keep annotated targets bright above the dim veil.
    if (getComputedStyle(element).position === "static") element.style.position = "relative";
    element.style.zIndex = "2147483647";
  }};
  const place_label = (x, y, w, h, number, label) => {{
    const marker = document.createElement("div");
    marker.dataset.iris = "marker";
    marker.textContent = String(number);
    const mx = x - 16, my = y - 16;
    marker.style.cssText = "position:absolute;left:" + mx + "px;top:" + my + "px;"
      + "width:30px;height:30px;border-radius:50%;background:" + INK + ";color:#fff;"
      + "border:2px solid #fff;box-shadow:0 1px 6px rgba(0,0,0,.45);"
      + "font:700 15px/26px system-ui,sans-serif;text-align:center;"
      + "z-index:2147483647;pointer-events:none;margin:0;padding:0;";
    document.body.appendChild(marker);
    boxes.push({{ x: mx, y: my, width: 30, height: 30 }});
    const tag = document.createElement("div");
    tag.dataset.iris = "label";
    tag.textContent = label;
    tag.style.cssText = "position:absolute;max-width:250px;background:" + INK + ";color:#fff;"
      + "font:500 13px/1.35 system-ui,sans-serif;padding:7px 11px;border-radius:8px;"
      + "box-shadow:0 1px 6px rgba(0,0,0,.45);z-index:2147483647;pointer-events:none;"
      + "margin:0;white-space:normal;";
    tag.style.visibility = "hidden";
    document.body.appendChild(tag);
    const tw = Math.min(tag.offsetWidth || 200, 250);
    const th = tag.offsetHeight || 32;
    let lx = x + w + 14;
    if (lx + tw > doc_width - 4) lx = x - tw - 14; // fall back to the left
    if (lx < 4) lx = Math.min(Math.max(x, 4), Math.max(doc_width - tw - 4, 4));
    let ly = Math.max(y, 4);
    tag.style.left = lx + "px";
    tag.style.top = ly + "px";
    tag.style.visibility = "visible";
    boxes.push({{ x: lx, y: ly, width: tw, height: th }});
    // Leader arrow from the marker to the label, painted beneath both.
    const x1 = mx + 15, y1 = my + 15;
    const x2 = lx + tw / 2, y2 = ly + th / 2;
    const len = Math.hypot(x2 - x1, y2 - y1);
    if (len > 20) {{
      const angle = Math.atan2(y2 - y1, x2 - x1);
      const arrow = document.createElement("div");
      arrow.dataset.iris = "arrow";
      arrow.style.cssText = "position:absolute;left:" + x1 + "px;top:" + (y1 - 2) + "px;"
        + "width:" + len + "px;height:4px;background:" + ORANGE + ";"
        + "transform:rotate(" + angle + "rad);transform-origin:0 50%;"
        + "z-index:2147483647;pointer-events:none;margin:0;padding:0;";
      document.body.insertBefore(arrow, marker);
      boxes.push({{
        x: Math.min(x1, x2) - 3, y: Math.min(y1, y2) - 3,
        width: Math.abs(x2 - x1) + 6, height: Math.abs(y2 - y1) + 6,
      }});
    }}
  }};
  for (const point of points) {{
    const element = find(point.selector);
    if (!element) throw new Error("annotation selector matched nothing: " + point.selector);
    const rect = element.getBoundingClientRect();
    const x = rect.left + window.scrollX, y = rect.top + window.scrollY;
    if (dim) lift(element);
    outline(x, y, rect.width, rect.height);
    place_label(x, y, rect.width, rect.height, point.number, point.label);
  }}
  for (const selector of highlights) {{
    const element = find(selector);
    if (!element) throw new Error("highlight selector matched nothing: " + selector);
    const rect = element.getBoundingClientRect();
    if (dim) lift(element);
    outline(rect.left + window.scrollX, rect.top + window.scrollY, rect.width, rect.height);
  }}
  return boxes;
}})()"##
    )
}

/// Grow an element clip to include overlay boxes, clamped to the document.
fn union_clip(
    mut clip: ClipRect,
    boxes: &[OverlayBox],
    doc_width: f64,
    doc_height: f64,
) -> ClipRect {
    let mut left = clip.x;
    let mut top = clip.y;
    let mut right = clip.x + clip.width;
    let mut bottom = clip.y + clip.height;
    for b in boxes {
        if ![b.x, b.y, b.width, b.height].iter().all(|n| n.is_finite())
            || b.width <= 0.0
            || b.height <= 0.0
        {
            continue;
        }
        left = left.min(b.x - 4.0);
        top = top.min(b.y - 4.0);
        right = right.max(b.x + b.width + 4.0);
        bottom = bottom.max(b.y + b.height + 4.0);
    }
    clip.x = left.clamp(0.0, doc_width);
    clip.y = top.clamp(0.0, doc_height);
    clip.width = right.clamp(0.0, doc_width) - clip.x;
    clip.height = bottom.clamp(0.0, doc_height) - clip.y;
    clip
}

fn round_clip(bounds: &ElementBounds, padding: u32) -> Result<ClipRect> {
    let padding = padding as f64;
    let left = (bounds.x - padding).floor().clamp(0.0, bounds.doc_width);
    let top = (bounds.y - padding).floor().clamp(0.0, bounds.doc_height);
    let right = (bounds.x + bounds.width + padding)
        .ceil()
        .clamp(0.0, bounds.doc_width);
    let bottom = (bounds.y + bounds.height + padding)
        .ceil()
        .clamp(0.0, bounds.doc_height);

    if ![left, top, right, bottom].iter().all(|n| n.is_finite()) {
        bail!("selected element returned invalid bounds");
    }
    if right <= left || bottom <= top {
        bail!("selected element is outside the document bounds");
    }

    Ok(ClipRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_rounds_outward_and_applies_padding() {
        let clip = round_clip(
            &ElementBounds {
                x: 10.25,
                y: 20.75,
                width: 100.5,
                height: 50.5,
                doc_width: 200.0,
                doc_height: 200.0,
            },
            5,
        )
        .unwrap();
        assert_eq!(
            clip,
            ClipRect {
                x: 5.0,
                y: 15.0,
                width: 111.0,
                height: 62.0,
            }
        );
    }

    #[test]
    fn clip_clamps_to_each_document_edge() {
        let top_left = round_clip(
            &ElementBounds {
                x: 1.2,
                y: 1.2,
                width: 20.2,
                height: 30.2,
                doc_width: 200.0,
                doc_height: 200.0,
            },
            10,
        )
        .unwrap();
        assert_eq!(
            top_left,
            ClipRect {
                x: 0.0,
                y: 0.0,
                width: 32.0,
                height: 42.0,
            }
        );

        let bottom_right = round_clip(
            &ElementBounds {
                x: 190.4,
                y: 180.4,
                width: 20.0,
                height: 30.0,
                doc_width: 200.0,
                doc_height: 200.0,
            },
            5,
        )
        .unwrap();
        assert_eq!(
            bottom_right,
            ClipRect {
                x: 185.0,
                y: 175.0,
                width: 15.0,
                height: 25.0,
            }
        );
    }

    #[test]
    fn union_clip_grows_to_include_overlays_and_clamps_to_document() {
        let clip = ClipRect {
            x: 50.0,
            y: 50.0,
            width: 100.0,
            height: 60.0,
        };
        let grown = union_clip(
            clip,
            &[
                OverlayBox {
                    x: 170.0,
                    y: 20.0,
                    width: 60.0,
                    height: 30.0,
                },
                // Degenerate boxes are ignored, never shrink the clip.
                OverlayBox {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 10.0,
                },
            ],
            200.0,
            200.0,
        );
        assert_eq!(
            grown,
            ClipRect {
                x: 50.0,
                y: 16.0,
                width: 150.0,
                height: 94.0,
            }
        );

        let clamped = union_clip(
            ClipRect {
                x: 50.0,
                y: 50.0,
                width: 100.0,
                height: 60.0,
            },
            &[OverlayBox {
                x: 190.0,
                y: 190.0,
                width: 80.0,
                height: 80.0,
            }],
            200.0,
            200.0,
        );
        assert_eq!(
            clamped,
            ClipRect {
                x: 50.0,
                y: 50.0,
                width: 150.0,
                height: 150.0,
            }
        );
    }

    #[test]
    fn interaction_scripts_carry_step_labels_and_escaped_values() {
        let click = interaction_js(
            &InteractionStep::Click {
                selector: "text=Run \"analysis\"".into(),
            },
            1,
            2500,
        );
        assert!(click.contains(r#"const label = "step 2 (click)""#));
        assert!(click.contains("Date.now() + 2500"));
        assert!(click.contains(r#"startsWith("text=")"#));
        assert!(click.contains("Run \\\"analysis\\\""));

        let fill = interaction_js(
            &InteractionStep::Fill {
                selector: "#search".into(),
                text: "a'b\"c\\d".into(),
            },
            0,
            1000,
        );
        assert!(fill.contains(r#"const label = "step 1 (fill)""#));
        assert!(fill.contains("getOwnPropertyDescriptor"));
        assert!(fill.contains("a'b\\\"c\\\\d"));

        let hover = interaction_js(
            &InteractionStep::Hover {
                selector: "#menu".into(),
            },
            2,
            1000,
        );
        assert!(hover.contains(r#"const label = "step 3 (hover)""#));
        assert!(hover.contains("pointerover"));

        let press = interaction_js(
            &InteractionStep::Press {
                key: "Enter".into(),
            },
            3,
            1000,
        );
        assert!(press.contains(r#"const label = "step 4 (press)""#));
        assert!(press.contains(r#"const key = "Enter""#));

        let wait = interaction_js(
            &InteractionStep::WaitFor {
                selector: "[role=dialog]".into(),
            },
            4,
            1000,
        );
        assert!(wait.contains(r#"const label = "step 5 (wait_for)""#));
        assert!(wait.contains("target never appeared"));
    }

    #[test]
    fn mask_patterns_have_expected_names_and_sources() {
        assert_eq!(
            [
                MaskPattern::Email,
                MaskPattern::Phone,
                MaskPattern::Ssn,
                MaskPattern::Account,
            ]
            .map(MaskPattern::name),
            ["email", "phone", "ssn", "account"]
        );
        // Sources must survive a JSON round trip into `new RegExp(source)`.
        for pattern in [
            MaskPattern::Email,
            MaskPattern::Phone,
            MaskPattern::Ssn,
            MaskPattern::Account,
        ] {
            let value = serde_json::to_string(pattern.regex_source()).unwrap();
            let back: String = serde_json::from_str(&value).unwrap();
            assert_eq!(back, pattern.regex_source());
        }
    }

    #[test]
    fn mask_script_embeds_selectors_blur_and_patterns() {
        let opts = Opts {
            viewport: Viewport::desktop(),
            mode: CaptureMode::Viewport,
            color_scheme: ColorScheme::System,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(3),
            format: Format::Png,
            annotations: Vec::new(),
            highlights: Vec::new(),
            dim: false,
            steps: Vec::new(),
            masks: vec![".client-name".into(), "[data-private]".into()],
            mask_blur_px: Some(6),
            mask_patterns: vec![MaskPattern::Email, MaskPattern::Ssn],
        };
        let js = mask_js(&opts);
        assert!(js.contains(r#"const selectors = [".client-name","[data-private]"]"#));
        assert!(js.contains("const blurPx = 6;"));
        assert!(js.contains(r#""name":"email""#));
        assert!(js.contains(r#""name":"ssn""#));
        assert!(!js.contains(r#""name":"phone""#));
        assert!(js.contains("backdrop-filter:blur("));

        let ink = Opts {
            masks: vec![".x".into()],
            mask_blur_px: None,
            mask_patterns: Vec::new(),
            ..default_opts()
        };
        let js = mask_js(&ink);
        assert!(js.contains("const blurPx = null;"));
        assert!(js.contains("background:#111310;"));
    }

    #[test]
    fn overlay_script_embeds_points_with_json_escaping() {
        let opts = Opts {
            viewport: Viewport::desktop(),
            mode: CaptureMode::Viewport,
            color_scheme: ColorScheme::System,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(3),
            format: Format::Png,
            annotations: vec![Annotation {
                selector: "#search".into(),
                label: "Find \"quoted\" \\ done".into(),
                number: 2,
            }],
            highlights: vec!["#run-analysis".into()],
            dim: true,
            steps: Vec::new(),
            masks: Vec::new(),
            mask_blur_px: None,
            mask_patterns: Vec::new(),
        };
        let js = overlay_js(&opts);
        assert!(js.contains("rotate("));
        assert!(js.contains(
            r##"{"label":"Find \"quoted\" \\ done","number":2,"selector":"#search"}"##,
        ));
        assert!(js.contains(r##"const highlights = ["#run-analysis"]"##));
        assert!(js.contains("const dim = true;"));
    }

    #[test]
    fn session_names_reject_path_escapes_and_blank_values() {
        for good in ["matteros", "va-matters_2", "v2.0"] {
            validate_session_name(good).unwrap();
        }
        for bad in [
            "",
            "a/b",
            "a\\b",
            "..",
            ".",
            "has space",
            "semi;colon",
            "üñï",
        ] {
            assert!(
                validate_session_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(validate_session_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn session_dirs_stay_under_the_sessions_root() {
        let dir = session_dir("  matteros  ").unwrap();
        assert!(dir.ends_with(Path::new("sessions").join("matteros")));
        assert!(session_dir("../evil").is_err());
        let missing = resolve_session_dir("iris-test-no-such-session-xyz").unwrap_err();
        assert!(format!("{missing:#}").contains("unknown session"));
        assert!(format!("{missing:#}").contains("iris login"));
    }

    #[test]
    fn local_urls_use_http_and_public_hosts_use_https() {
        assert_eq!(
            normalize_url("localhost:3000").unwrap().as_str(),
            "http://localhost:3000/"
        );
        assert_eq!(
            normalize_url("app.localhost:4173").unwrap().as_str(),
            "http://app.localhost:4173/"
        );
        assert_eq!(
            normalize_url("127.0.0.1:8080").unwrap().as_str(),
            "http://127.0.0.1:8080/"
        );
        assert_eq!(
            normalize_url("example.com").unwrap().as_str(),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url("http://example.com").unwrap().as_str(),
            "http://example.com/"
        );
    }

    #[test]
    fn viewport_rejects_zero_dimensions_and_invalid_scales() {
        assert!(parse_viewport("0x900", None).is_err());
        assert!(parse_viewport("1440x0", None).is_err());
        assert!(parse_viewport("desktop", Some(0.0)).is_err());
        assert!(parse_viewport("desktop", Some(f64::NAN)).is_err());
    }

    #[tokio::test]
    async fn browser_element_capture_contract() -> Result<()> {
        let temp = std::env::temp_dir().join(format!("iris-browser-{}", std::process::id()));
        tokio::fs::create_dir_all(&temp).await?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/precise-capture.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(&fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;

        let one_x_path = temp.join("target-1x.png");
        let one_x = capture_to(
            &session,
            url.as_str(),
            &one_x_path,
            &element_opts(viewport, ".capture-target", 10, Format::Png),
        )
        .await?;
        // The first duplicate starts at 80.5px and transitions to 120.5px only
        // after scrolling into view. The final 141x81 frame proves first-match,
        // automatic scroll/settle, outward rounding, and padding together.
        assert_eq!((one_x.width, one_x.height), (141, 81));
        assert_eq!(
            png_dimensions(&tokio::fs::read(&one_x_path).await?),
            (141, 81)
        );

        let two_x_viewport = Viewport {
            scale: 2.0,
            ..viewport
        };
        let two_x_path = temp.join("target-2x.png");
        let two_x = capture_to(
            &session,
            url.as_str(),
            &two_x_path,
            &element_opts(two_x_viewport, ".capture-target", 10, Format::Png),
        )
        .await?;
        assert_eq!((two_x.width, two_x.height), (141, 81));
        assert_eq!(
            png_dimensions(&tokio::fs::read(&two_x_path).await?),
            (282, 162)
        );

        let jpeg_path = temp.join("target.jpg");
        capture_to(
            &session,
            url.as_str(),
            &jpeg_path,
            &element_opts(viewport, ".capture-target", 0, Format::Jpeg),
        )
        .await?;
        let jpeg = tokio::fs::read(&jpeg_path).await?;
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]));

        let webp_path = temp.join("target.webp");
        capture_to(
            &session,
            url.as_str(),
            &webp_path,
            &element_opts(viewport, ".capture-target", 0, Format::Webp),
        )
        .await?;
        let webp = tokio::fs::read(&webp_path).await?;
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");

        let dark_viewport = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::Viewport, ColorScheme::Dark),
            )
            .await?;
        assert_eq!(
            (dark_viewport.shot.width, dark_viewport.shot.height),
            (320, 240)
        );
        assert_eq!(png_dimensions(&dark_viewport.data), (320, 240));

        let full_page = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::FullPage, ColorScheme::System),
            )
            .await?;
        assert_eq!(full_page.shot.width, 320);
        assert!(full_page.shot.height > viewport.height);
        assert_eq!(
            png_dimensions(&full_page.data),
            (full_page.shot.width, full_page.shot.height)
        );

        let phone = Viewport {
            width: 390,
            height: 844,
            scale: 1.0,
            mobile: true,
        };
        let mobile = session
            .capture(
                url.as_str(),
                &page_opts(phone, CaptureMode::Viewport, ColorScheme::System),
            )
            .await?;
        assert_eq!((mobile.shot.width, mobile.shot.height), (390, 844));
        assert_eq!(png_dimensions(&mobile.data), (390, 844));

        let invalid = session
            .capture(url.as_str(), &element_opts(viewport, "[", 0, Format::Png))
            .await
            .unwrap_err();
        assert!(format!("{invalid:#}").contains("invalid selector: ["));

        let mut missing_url = url.clone();
        missing_url.set_query(Some("missing=1"));
        let missing = session
            .capture(
                missing_url.as_str(),
                &element_opts(viewport, ".capture-target", 0, Format::Png),
            )
            .await
            .unwrap_err();
        assert!(format!("{missing:#}").contains("selector never appeared: .capture-target"));

        let zero = session
            .capture(
                url.as_str(),
                &element_opts(viewport, "#zero-size", 0, Format::Png),
            )
            .await
            .unwrap_err();
        assert!(format!("{zero:#}").contains("element has no rendered size: #zero-size"));

        session.close().await;
        tokio::fs::remove_dir_all(temp).await?;
        Ok(())
    }

    #[tokio::test]
    async fn browser_interaction_contract() -> Result<()> {
        let temp = std::env::temp_dir().join(format!("iris-interact-{}", std::process::id()));
        tokio::fs::create_dir_all(&temp).await?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/interact.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(&fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let viewport = Viewport {
            width: 480,
            height: 360,
            scale: 1.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;

        // The dialog is absent from the DOM until the button is clicked:
        // without steps the element wait expires instead of capturing.
        let hidden = session
            .capture(
                url.as_str(),
                &element_opts(viewport, "#dialog", 0, Format::Png),
            )
            .await
            .unwrap_err();
        assert!(format!("{hidden:#}").contains("selector never appeared: #dialog"));

        // A text= click opens the dialog; the follow-up element capture only
        // succeeds when the click really ran.
        let dialog_path = temp.join("dialog.png");
        let dialog = capture_to(
            &session,
            url.as_str(),
            &dialog_path,
            &step_opts(
                viewport,
                vec![InteractionStep::Click {
                    selector: "text=Run analysis".into(),
                }],
                CaptureMode::Element {
                    selector: "#dialog".into(),
                    padding: 0,
                },
            ),
        )
        .await?;
        assert_eq!(dialog.width, 220);
        assert!(dialog.height >= 60);

        // Fill + key press run cleanly ahead of a viewport capture.
        let filled_path = temp.join("filled.png");
        let filled = capture_to(
            &session,
            url.as_str(),
            &filled_path,
            &step_opts(
                viewport,
                vec![
                    InteractionStep::Fill {
                        selector: "#search".into(),
                        text: "asthma".into(),
                    },
                    InteractionStep::Press {
                        key: "Enter".into(),
                    },
                ],
                CaptureMode::Viewport,
            ),
        )
        .await?;
        assert_eq!((filled.width, filled.height), (480, 360));

        // A missing step target names its step instead of timing out blandly.
        let missing = session
            .capture(
                url.as_str(),
                &step_opts(
                    viewport,
                    vec![InteractionStep::Click {
                        selector: "#missing".into(),
                    }],
                    CaptureMode::Viewport,
                ),
            )
            .await
            .unwrap_err();
        assert!(format!("{missing:#}").contains("step 1 (click): target never appeared: #missing"));

        session.close().await;
        tokio::fs::remove_dir_all(temp).await?;
        Ok(())
    }

    #[tokio::test]
    async fn browser_masking_contract() -> Result<()> {
        let temp = std::env::temp_dir().join(format!("iris-mask-{}", std::process::id()));
        tokio::fs::create_dir_all(&temp).await?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/private.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(&fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let viewport = Viewport {
            width: 480,
            height: 360,
            scale: 1.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;

        let plain = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::Viewport, ColorScheme::System),
            )
            .await?;

        // Two explicit selectors plus email, phone, SSN, card, and the email
        // inside the input value: seven redaction boxes in total.
        let masked_path = temp.join("masked.png");
        let masked_image = session
            .capture(
                url.as_str(),
                &mask_opts(
                    viewport,
                    vec![".client-name".into(), "[data-private]".into()],
                    None,
                    vec![
                        MaskPattern::Email,
                        MaskPattern::Phone,
                        MaskPattern::Ssn,
                        MaskPattern::Account,
                    ],
                ),
            )
            .await?;
        masked_image.write_to(&masked_path).await?;
        assert_eq!(masked_image.shot.masked, 7);
        // Redaction visibly changes pixels; the order number and surrounding
        // prose keep the shot recognizable as the same page.
        assert_ne!(masked_image.data, plain.data);

        // Blur mode draws the same boxes through backdrop-filter instead.
        let blurred = session
            .capture(
                url.as_str(),
                &mask_opts(viewport, vec![".client-name".into()], Some(6), vec![]),
            )
            .await?;
        assert_eq!(blurred.shot.masked, 1);
        assert_ne!(blurred.data, plain.data);

        // Patterns alone leave the order number and prose untouched in shape:
        // only the five sensitive matches are covered.
        let patterns_only = session
            .capture(
                url.as_str(),
                &mask_opts(
                    viewport,
                    vec![],
                    None,
                    vec![
                        MaskPattern::Email,
                        MaskPattern::Phone,
                        MaskPattern::Ssn,
                        MaskPattern::Account,
                    ],
                ),
            )
            .await?;
        assert_eq!(patterns_only.shot.masked, 5);

        let invalid = session
            .capture(
                url.as_str(),
                &mask_opts(viewport, vec!["[".into()], None, vec![]),
            )
            .await
            .unwrap_err();
        assert!(format!("{invalid:#}").contains("invalid selector: ["));

        session.close().await;
        tokio::fs::remove_dir_all(temp).await?;
        Ok(())
    }

    fn default_opts() -> Opts {
        Opts {
            viewport: Viewport::desktop(),
            mode: CaptureMode::Viewport,
            color_scheme: ColorScheme::System,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(3),
            format: Format::Png,
            annotations: Vec::new(),
            highlights: Vec::new(),
            dim: false,
            steps: Vec::new(),
            masks: Vec::new(),
            mask_blur_px: None,
            mask_patterns: Vec::new(),
        }
    }

    fn mask_opts(
        viewport: Viewport,
        masks: Vec<String>,
        mask_blur_px: Option<u32>,
        mask_patterns: Vec<MaskPattern>,
    ) -> Opts {
        Opts {
            viewport,
            mode: CaptureMode::Viewport,
            color_scheme: ColorScheme::System,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(4),
            format: Format::Png,
            annotations: Vec::new(),
            highlights: Vec::new(),
            dim: false,
            steps: Vec::new(),
            masks,
            mask_blur_px,
            mask_patterns,
        }
    }

    fn step_opts(viewport: Viewport, steps: Vec<InteractionStep>, mode: CaptureMode) -> Opts {
        Opts {
            viewport,
            mode,
            color_scheme: ColorScheme::System,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(4),
            format: Format::Png,
            annotations: Vec::new(),
            highlights: Vec::new(),
            dim: false,
            steps,
            masks: Vec::new(),
            mask_blur_px: None,
            mask_patterns: Vec::new(),
        }
    }

    #[tokio::test]
    async fn browser_color_scheme_contract() -> Result<()> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/precise-capture.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(&fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;

        // The fixture swaps its background under prefers-color-scheme, so
        // forced light and dark captures must render different pixels.
        let light = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::Viewport, ColorScheme::Light),
            )
            .await?;
        let dark = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::Viewport, ColorScheme::Dark),
            )
            .await?;
        assert_eq!((light.shot.width, light.shot.height), (320, 240));
        assert_ne!(light.data, dark.data);

        let system = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::Viewport, ColorScheme::System),
            )
            .await?;
        assert_eq!((system.shot.width, system.shot.height), (320, 240));

        session.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn browser_session_contract() -> Result<()> {
        let temp = std::env::temp_dir().join(format!("iris-session-{}", std::process::id()));
        tokio::fs::create_dir_all(&temp).await?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/session.html")
            .canonicalize()?;
        let url = url::Url::from_file_path(&fixture)
            .map_err(|_| anyhow!("fixture path is not a file URL"))?;
        let viewport = Viewport {
            width: 480,
            height: 360,
            scale: 1.0,
            mobile: false,
        };
        let profile = temp.join("profile");
        tokio::fs::create_dir_all(&profile).await?;

        // First visit in the named profile stores the flag but renders no
        // #returning element yet.
        let first = Session::launch_with_profile(None, viewport, profile.clone()).await?;
        let seen = first
            .capture(
                url.as_str(),
                &element_opts(viewport, "#always", 0, Format::Png),
            )
            .await?;
        assert_eq!((seen.shot.width, seen.shot.height), (200, 40));
        // Closing a named profile keeps it on disk for the next launch.
        first.close().await;
        assert!(profile.is_dir());

        // Second launch with the same directory sees the stored flag.
        let second = Session::launch_with_profile(None, viewport, profile.clone()).await?;
        let back = second
            .capture(
                url.as_str(),
                &element_opts(viewport, "#returning", 0, Format::Png),
            )
            .await?;
        assert_eq!((back.shot.width, back.shot.height), (200, 40));
        second.close().await;

        // A fresh temp profile is isolated: the flag is absent there.
        let fresh = Session::launch(None, viewport).await?;
        let missing = fresh
            .capture(
                url.as_str(),
                &element_opts(viewport, "#returning", 0, Format::Png),
            )
            .await
            .unwrap_err();
        assert!(format!("{missing:#}").contains("selector never appeared: #returning"));
        fresh.close().await;

        tokio::fs::remove_dir_all(temp).await?;
        Ok(())
    }

    fn element_opts(viewport: Viewport, selector: &str, padding: u32, format: Format) -> Opts {
        Opts {
            viewport,
            mode: CaptureMode::Element {
                selector: selector.into(),
                padding,
            },
            color_scheme: ColorScheme::System,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(3),
            format,
            annotations: Vec::new(),
            highlights: Vec::new(),
            dim: false,
            steps: Vec::new(),
            masks: Vec::new(),
            mask_blur_px: None,
            mask_patterns: Vec::new(),
        }
    }

    fn page_opts(viewport: Viewport, mode: CaptureMode, color_scheme: ColorScheme) -> Opts {
        Opts {
            viewport,
            mode,
            color_scheme,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(5),
            format: Format::Png,
            annotations: Vec::new(),
            highlights: Vec::new(),
            dim: false,
            steps: Vec::new(),
            masks: Vec::new(),
            mask_blur_px: None,
            mask_patterns: Vec::new(),
        }
    }

    async fn capture_to(session: &Session, url: &str, path: &Path, opts: &Opts) -> Result<Shot> {
        let image = session.capture(url, opts).await?;
        image.write_to(path).await?;
        Ok(image.shot)
    }

    fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        )
    }
}
