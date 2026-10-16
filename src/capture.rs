use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, Viewport as ScreenshotViewport,
};
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::error::CdpError;
use chromiumoxide::page::Page;
use futures::StreamExt;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::task::JoinHandle;
use url::Url;

const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
static PROFILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ProfileDir(PathBuf);

impl ProfileDir {
    fn unique() -> Self {
        Self(std::env::temp_dir().join(format!(
            "iris-chrome-{}-{}",
            std::process::id(),
            PROFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProfileDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

#[derive(Debug)]
pub struct Opts {
    pub viewport: Viewport,
    pub mode: CaptureMode,
    pub dark: bool,
    pub wait_ms: u64,
    pub wait_for: Option<String>,
    pub timeout: Duration,
    pub format: Format,
    pub quality: u8,
    pub max_pixels: Option<u64>,
    pub freeze_animations: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    pub setup_ms: u64,
    pub navigation_ms: u64,
    pub ready_ms: u64,
    pub screenshot_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PageInfo {
    pub final_url: String,
    pub title: String,
    pub document_width: u32,
    pub document_height: u32,
    pub pending_images: u32,
    pub failed_images: u32,
    pub fonts_pending: bool,
    pub running_animations: u32,
}

#[derive(Debug)]
pub struct Shot {
    /// Capture rectangle in CSS pixels; scale records the actual image density.
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub bytes: u64,
    pub page: PageInfo,
    pub timings: Timings,
    pub warnings: Vec<String>,
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
    page: PageInfo,
    timings: Timings,
    warnings: Vec<String>,
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
        page: shot.page.clone(),
        timings: shot.timings.clone(),
        warnings: shot.warnings.clone(),
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
    /// Keep Chrome's encoded bytes for MCP instead of decoding and encoding again.
    pub data: String,
}

impl CapturedImage {
    pub fn pixels(&self) -> Result<Vec<u8>> {
        BASE64
            .decode(&self.data)
            .context("Chrome returned invalid image data")
    }

    pub async fn write_to(&self, out: &Path) -> Result<()> {
        if let Some(parent) = out.parent().filter(|path| !path.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        tokio::fs::write(out, self.pixels()?)
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

struct CaptureTab {
    page: Page,
    closing: bool,
}

impl CaptureTab {
    async fn close(mut self) {
        let result = tokio::time::timeout(Duration::from_secs(2), self.page.clone().close()).await;
        self.closing = matches!(result, Ok(Ok(_)));
    }
}

impl Drop for CaptureTab {
    fn drop(&mut self) {
        if !self.closing {
            let page = self.page.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), page.close()).await;
            });
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.handler.abort();
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl Session {
    pub async fn launch(chrome: Option<PathBuf>, viewport: Viewport) -> Result<Self> {
        let profile_dir = ProfileDir::unique();
        let mut config = BrowserConfig::builder()
            .window_size(viewport.width, viewport.height)
            .user_data_dir(profile_dir.path());
        if let Some(path) = chrome.or_else(find_chrome) {
            config = config.chrome_executable(path);
        }
        let config = config
            .build()
            .map_err(|e| anyhow!("{e}: set CHROME=/path/to/browser or pass --chrome"))?;
        let (browser, mut handler) = Browser::launch(config)
            .await
            .context("failed to launch Chrome (install a Chrome-family browser, set CHROME, or pass --chrome)")?;
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
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + opts.timeout;
        // Background tabs can stop producing the rendered frames these captures await.
        let target = CreateTargetParams::builder()
            .url("about:blank")
            .new_window(true)
            .build()
            .map_err(|error| anyhow!(error))?;
        let page = tokio::time::timeout_at(deadline, self.browser.new_page(target))
            .await
            .map_err(|_| anyhow!("timed out opening a tab"))??;
        let tab = CaptureTab {
            page,
            closing: false,
        };
        let result =
            tokio::time::timeout_at(deadline, self.pipeline(&tab.page, url, opts, started))
                .await
                .map_err(|_| anyhow!("capture timed out after {}s", opts.timeout.as_secs()))
                .and_then(|result| result);
        tab.close().await;
        result
    }

    pub fn is_healthy(&self) -> bool {
        !self.handler.is_finished()
    }

    async fn pipeline(
        &self,
        page: &Page,
        url: &str,
        opts: &Opts,
        started: Instant,
    ) -> Result<CapturedImage> {
        let v = opts.viewport;
        page.execute(SetDeviceMetricsOverrideParams::new(
            i64::from(v.width),
            i64::from(v.height),
            v.scale,
            v.mobile,
        ))
        .await?;

        let user_agent = if v.mobile {
            Some(IPHONE_UA)
        } else {
            self.user_agent.as_deref()
        };
        if let Some(user_agent) = user_agent {
            page.execute(SetUserAgentOverrideParams::new(user_agent))
                .await?;
        }
        if opts.dark {
            page.execute(
                SetEmulatedMediaParams::builder()
                    .feature(MediaFeature {
                        name: "prefers-color-scheme".into(),
                        value: "dark".into(),
                    })
                    .build(),
            )
            .await?;
        }
        let mut timings = Timings {
            setup_ms: elapsed_ms(started),
            ..Timings::default()
        };
        let navigation = Instant::now();
        page.goto(url).await.context("navigation failed")?;
        timings.navigation_ms = elapsed_ms(navigation);

        let ready = Instant::now();
        let remaining_ms = || {
            u64::try_from(
                opts.timeout
                    .saturating_sub(started.elapsed())
                    .saturating_sub(Duration::from_millis(500))
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        };
        if let Some(selector) = &opts.wait_for {
            self.eval::<()>(page, wait_for_js(selector, remaining_ms())?)
                .await?;
        }
        match &opts.mode {
            CaptureMode::Viewport => {}
            CaptureMode::FullPage => self.eval::<()>(page, SCROLL_JS.into()).await?,
            CaptureMode::Element { selector, .. } => {
                self.eval::<()>(page, wait_and_scroll_js(selector, remaining_ms())?)
                    .await?;
            }
        }
        if opts.wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(opts.wait_ms)).await;
        }
        self.eval::<()>(
            page,
            format!(
                "({SETTLE_JS})({}, {})",
                matches!(opts.mode, CaptureMode::FullPage),
                opts.freeze_animations
            ),
        )
        .await?;
        let info: PageInfo = self.eval(page, PAGE_INFO_JS.into()).await?;
        timings.ready_ms = elapsed_ms(ready);

        let clip = match &opts.mode {
            CaptureMode::Viewport => {
                let (x, y): (f64, f64) = self
                    .eval(page, "[window.scrollX, window.scrollY]".into())
                    .await?;
                ClipRect {
                    x,
                    y,
                    width: f64::from(v.width),
                    height: f64::from(v.height),
                }
            }
            CaptureMode::FullPage => ClipRect {
                x: 0.0,
                y: 0.0,
                width: f64::from(info.document_width.max(v.width)),
                height: f64::from(info.document_height.max(v.height)),
            },
            CaptureMode::Element { selector, padding } => {
                let bounds: ElementBounds = self.eval(page, element_bounds_js(selector)?).await?;
                round_clip(&bounds, *padding)
                    .with_context(|| format!("cannot capture selected element: {selector}"))?
            }
        };
        let mut scale = v.scale;
        if let Some(max_pixels) = opts.max_pixels {
            scale = scale.min(
                (max_pixels
                    .to_f64()
                    .context("pixel budget is out of range")?
                    / (clip.width * clip.height))
                    .sqrt(),
            );
            scale = ((clip.width * scale).floor() / clip.width)
                .min((clip.height * scale).floor() / clip.height);
            if scale <= 0.0 {
                bail!("pixel budget is too small for this capture's aspect ratio");
            }
        } else if matches!(opts.mode, CaptureMode::FullPage) && clip.height * scale > 16_000.0 {
            // Past Chrome's ~16k px render limit, full pages fall back to 1x.
            scale = 1.0;
        }
        let capture_started = Instant::now();
        let mut params = CaptureScreenshotParams::builder()
            .format(opts.format.cdp())
            .from_surface(true)
            .capture_beyond_viewport(true)
            .clip(ScreenshotViewport {
                x: clip.x,
                y: clip.y,
                width: clip.width,
                height: clip.height,
                scale: scale / v.scale,
            });
        if opts.format != Format::Png {
            params = params.quality(i64::from(opts.quality));
        }
        let data: String = page
            .execute(params.build())
            .await
            .context("screenshot failed")?
            .result
            .data
            .into();
        timings.screenshot_ms = elapsed_ms(capture_started);
        timings.total_ms = elapsed_ms(started);
        let padding = data
            .as_bytes()
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'=')
            .count();
        let bytes = u64::try_from(data.len() / 4 * 3 - padding)?;
        let mut warnings = Vec::new();
        if info.pending_images > 0 {
            warnings.push(format!("{} images were still loading", info.pending_images));
        }
        if info.failed_images > 0 {
            warnings.push(format!("{} images failed to load", info.failed_images));
        }
        if info.fonts_pending {
            warnings.push("fonts were still loading".into());
        }
        if info.running_animations > 0 {
            warnings.push(format!(
                "{} animations were still running",
                info.running_animations
            ));
        }
        Ok(CapturedImage {
            shot: Shot {
                width: clip
                    .width
                    .to_u32()
                    .context("capture width is out of range")?,
                height: clip
                    .height
                    .to_u32()
                    .context("capture height is out of range")?,
                scale,
                bytes,
                page: info,
                timings,
                warnings,
            },
            data,
        })
    }

    /// Run a JS expression (promises awaited); surface page-side exceptions as errors.
    async fn eval<T: DeserializeOwned>(&self, page: &Page, js: String) -> Result<T> {
        let params = EvaluateParams::builder()
            .expression(js)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .map_err(|error| anyhow!(error))?;
        let response = page.execute(params).await?.result;
        if let Some(details) = response.exception_details {
            let message = details
                .exception
                .and_then(|exception| exception.description)
                .unwrap_or(details.text);
            bail!("{}", message.lines().next().unwrap_or("page script failed"));
        }
        serde_json::from_value(response.result.value.unwrap_or(serde_json::Value::Null))
            .context("unexpected response from capture script")
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
        reap_profile_processes(self.profile_dir.path()).await;
        let _ = tokio::fs::remove_dir_all(self.profile_dir.path()).await;
    }
}

/// Chromium helpers can outlive the browser: Helium leaves its network
/// service running, reparented to launchd. Stop every process still started
/// with this session's profile, politely first.
async fn reap_profile_processes(profile: &Path) {
    let marker = format!("--user-data-dir={}", profile.display());
    for signal in ["-TERM", "-KILL"] {
        let table = process_table();
        let pids = profile_pids(&table, &marker);
        if pids.is_empty() {
            return;
        }
        let _ = std::process::Command::new("kill")
            .arg(signal)
            .args(&pids)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !profile_pids(&process_table(), &marker).is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn process_table() -> String {
    std::process::Command::new("ps")
        .args(["axww", "-o", "pid=,command="])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

/// PIDs whose command line names exactly `marker` as an argument, so the
/// profile `...-1` never matches a sibling session's `...-10`.
fn profile_pids<'a>(table: &'a str, marker: &str) -> Vec<&'a str> {
    table
        .lines()
        .filter(|line| line.split_whitespace().any(|arg| arg == marker))
        .filter_map(|line| line.split_whitespace().next())
        .collect()
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
        "/Applications/Helium.app/Contents/MacOS/Helium",
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

/// Wait once, after scrolling. Asset and finite-animation waits share the same cap.
const SETTLE_JS: &str = r#"async (fullPage, freeze) => {
  const deadline = performance.now() + 3000;
  const bounded = async (promise) => {
    let timer;
    try {
      await Promise.race([promise, new Promise(resolve => {
        timer = setTimeout(resolve, Math.max(0, deadline - performance.now()));
      })]);
    } finally { clearTimeout(timer); }
  };
  const frames = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  const near = img => {
    const rect = img.getBoundingClientRect();
    return fullPage || (rect.top < innerHeight * 1.5 && rect.bottom > -innerHeight * 0.5);
  };
  const images = Array.from(document.images).filter(img => !img.complete && near(img));
  await bounded(Promise.all([
    document.fonts?.ready,
    ...images.map(img => new Promise(resolve => {
      img.addEventListener('load', resolve, { once: true });
      img.addEventListener('error', resolve, { once: true });
    })),
  ]));
  await frames();
  const animations = document.getAnimations().filter(animation => animation.playState === 'running');
  if (freeze) {
    for (const animation of animations) {
      try {
        if (animation.effect?.getComputedTiming().endTime !== Infinity) animation.finish();
        else animation.pause();
      } catch { animation.pause(); }
    }
  } else {
    const finite = animations.filter(animation => animation.effect?.getComputedTiming().endTime !== Infinity);
    await bounded(Promise.all(finite.map(animation => animation.finished.catch(() => {}))));
  }
  await frames();
}"#;

/// Preserve the requested viewport while triggering intersection-based lazy loading.
const SCROLL_JS: &str = r#"(async () => {
  const height = () => Math.max(document.body?.scrollHeight ?? 0, document.documentElement.scrollHeight);
  const step = Math.max(200, innerHeight * 0.9);
  for (let y = 0, steps = 0; y < height() && steps < 500; y += step, steps++) {
    scrollTo(0, y);
    await new Promise(resolve => requestAnimationFrame(resolve));
  }
  scrollTo(0, 0);
  await new Promise(resolve => requestAnimationFrame(resolve));
})()"#;

const PAGE_INFO_JS: &str = r#"(() => ({
  final_url: location.href,
  title: document.title,
  document_width: Math.max(document.body?.scrollWidth ?? 0, document.documentElement.scrollWidth, innerWidth),
  document_height: Math.max(document.body?.scrollHeight ?? 0, document.documentElement.scrollHeight, innerHeight),
  pending_images: Array.from(document.images).filter(image => !image.complete).length,
  failed_images: Array.from(document.images).filter(image => image.complete && image.currentSrc && !image.naturalWidth).length,
  fonts_pending: document.fonts?.status === 'loading',
  running_animations: document.getAnimations().filter(animation => animation.playState === 'running').length,
}))()"#;

fn wait_for_js(selector: &str, budget_ms: u64) -> Result<String> {
    let selector = serde_json::to_string(selector)?;
    Ok(format!(
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
    ))
}

fn wait_and_scroll_js(selector: &str, budget_ms: u64) -> Result<String> {
    let selector = serde_json::to_string(selector)?;
    Ok(format!(
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
    ))
}

fn element_bounds_js(selector: &str) -> Result<String> {
    let selector = serde_json::to_string(selector)?;
    Ok(format!(
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
    ))
}

fn round_clip(bounds: &ElementBounds, padding: u32) -> Result<ClipRect> {
    let padding = f64::from(padding);
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
    fn profile_pids_match_the_whole_profile_argument() {
        let table = concat!(
            "  101 /Apps/Helium --user-data-dir=/tmp/iris-chrome-7-1 --headless\n",
            "  102 /Apps/Helium Helper --type=utility --user-data-dir=/tmp/iris-chrome-7-10\n",
            "  103 /Apps/Helium Helper --type=renderer --user-data-dir=/tmp/iris-chrome-7-1\n",
        );

        assert_eq!(
            profile_pids(table, "--user-data-dir=/tmp/iris-chrome-7-1"),
            ["101", "103"]
        );
    }

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
    async fn full_page_capture_keeps_viewport_relative_heights() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/full-page-layout.html")
            .canonicalize()?;
        let url = Url::from_file_path(fixture).map_err(|_| anyhow!("invalid fixture URL"))?;
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 2.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;
        let capture = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::FullPage, false),
            )
            .await;
        session.close().await;
        let capture = capture?;

        // A 100vh header, 900px main, and 100px footer make a 1240px page.
        assert_eq!(
            (capture.shot.width, capture.shot.height, capture.shot.scale),
            (320, 1240, 2.0)
        );
        let image = image::load_from_memory(&capture.pixels()?)?.to_rgb8();
        assert_eq!(image.dimensions(), (640, 2480));
        // The header stays one viewport (240px, 480 device pixels) tall.
        assert_eq!(image.get_pixel(320, 479).0, [22, 78, 154]);
        assert_eq!(image.get_pixel(320, 480).0, [233, 138, 34]);
        assert_eq!(image.get_pixel(320, 2479).0, [39, 133, 74]);
        Ok(())
    }

    #[tokio::test]
    async fn full_page_capture_past_the_render_limit_falls_back_to_1x() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
        let url =
            "data:text/html,<style>html,body{margin:0}main{height:9000px}</style><main></main>";
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 2.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;
        let capture = session
            .capture(url, &page_opts(viewport, CaptureMode::FullPage, false))
            .await;
        session.close().await;
        let capture = capture?;

        // 9000 CSS px at 2x would be 18,000 device px.
        assert_eq!(capture.shot.scale, 1.0);
        let image = image::load_from_memory(&capture.pixels()?)?.to_rgb8();
        assert_eq!(image.dimensions(), (320, 9000));
        Ok(())
    }

    #[tokio::test]
    async fn full_page_pixel_budget_preserves_viewport_relative_layout() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/full-page-layout.html")
            .canonicalize()?;
        let url = Url::from_file_path(fixture).map_err(|_| anyhow!("invalid fixture URL"))?;
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 2.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;
        let mut opts = page_opts(viewport, CaptureMode::FullPage, false);
        opts.max_pixels = Some(100_000);
        let capture = session.capture(url.as_str(), &opts).await;
        session.close().await;
        let image = image::load_from_memory(&capture?.pixels()?)?.to_rgb8();
        assert_eq!(image.dimensions(), (160, 620));
        assert_eq!(image.get_pixel(80, 119).0, [22, 78, 154]);
        assert_eq!(image.get_pixel(80, 120).0, [233, 138, 34]);
        assert_eq!(image.get_pixel(80, 619).0, [39, 133, 74]);
        Ok(())
    }

    #[tokio::test]
    async fn viewport_capture_preserves_fragment_scroll_position() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/full-page-layout.html")
            .canonicalize()?;
        let mut url = Url::from_file_path(fixture).map_err(|_| anyhow!("invalid fixture URL"))?;
        url.set_fragment(Some("totals"));
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
            mobile: false,
        };
        let session = Session::launch(None, viewport).await?;
        let capture = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::Viewport, false),
            )
            .await;
        session.close().await;
        let image = image::load_from_memory(&capture?.pixels()?)?.to_rgb8();
        assert_eq!(image.get_pixel(160, 0).0, [233, 138, 34]);
        assert_eq!(image.get_pixel(160, 239).0, [39, 133, 74]);
        Ok(())
    }

    #[tokio::test]
    async fn browser_element_capture_contract() -> Result<()> {
        let _browser_guard = crate::BROWSER_TEST_LOCK.lock().await;
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
                &page_opts(viewport, CaptureMode::Viewport, true),
            )
            .await?;
        assert_eq!(
            (dark_viewport.shot.width, dark_viewport.shot.height),
            (320, 240)
        );
        assert_eq!(png_dimensions(&dark_viewport.pixels()?), (320, 240));

        let full_page = session
            .capture(
                url.as_str(),
                &page_opts(viewport, CaptureMode::FullPage, false),
            )
            .await?;
        assert_eq!(full_page.shot.width, 320);
        assert!(full_page.shot.height > viewport.height);
        assert_eq!(
            png_dimensions(&full_page.pixels()?),
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
                &page_opts(phone, CaptureMode::Viewport, false),
            )
            .await?;
        assert_eq!((mobile.shot.width, mobile.shot.height), (390, 844));
        assert_eq!(png_dimensions(&mobile.pixels()?), (390, 844));

        let invalid = session
            .capture(url.as_str(), &element_opts(viewport, "[", 0, Format::Png))
            .await
            .unwrap_err();
        assert!(format!("{invalid:#}").contains("invalid selector: ["));

        let mut missing_url = url.clone();
        missing_url.set_query(Some("missing=1"));
        let mut missing_opts = element_opts(viewport, ".capture-target", 0, Format::Png);
        missing_opts.timeout = Duration::from_secs(3);
        let missing = session
            .capture(missing_url.as_str(), &missing_opts)
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

    fn element_opts(viewport: Viewport, selector: &str, padding: u32, format: Format) -> Opts {
        Opts {
            viewport,
            mode: CaptureMode::Element {
                selector: selector.into(),
                padding,
            },
            dark: false,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(30),
            format,
            quality: 90,
            max_pixels: None,
            freeze_animations: false,
        }
    }

    fn page_opts(viewport: Viewport, mode: CaptureMode, dark: bool) -> Opts {
        Opts {
            viewport,
            mode,
            dark,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(30),
            format: Format::Png,
            quality: 90,
            max_pixels: None,
            freeze_animations: false,
        }
    }

    async fn capture_to(session: &Session, url: &str, path: &Path, opts: &Opts) -> Result<Shot> {
        let image = session
            .capture(url, opts)
            .await
            .with_context(|| format!("capture failed: {}", path.display()))?;
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
