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
        let profile_dir = ProfileDir::unique();
        let mut config = BrowserConfig::builder()
            .window_size(viewport.width, viewport.height)
            .user_data_dir(profile_dir.path());
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

    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.browser.close()).await;
        if tokio::time::timeout(Duration::from_secs(3), self.browser.wait())
            .await
            .is_err()
        {
            let _ = self.browser.kill().await;
        }
        self.handler.abort();
        let _ = tokio::fs::remove_dir_all(self.profile_dir.path()).await;
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
                &page_opts(viewport, CaptureMode::Viewport, true),
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
                &page_opts(viewport, CaptureMode::FullPage, false),
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
                &page_opts(phone, CaptureMode::Viewport, false),
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
            timeout: Duration::from_secs(3),
            format,
        }
    }

    fn page_opts(viewport: Viewport, mode: CaptureMode, dark: bool) -> Opts {
        Opts {
            viewport,
            mode,
            dark,
            wait_ms: 0,
            wait_for: None,
            timeout: Duration::from_secs(5),
            format: Format::Png,
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
