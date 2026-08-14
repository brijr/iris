use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::error::CdpError;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use tokio::task::JoinHandle;

const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

#[derive(Clone, Copy)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub mobile: bool,
}

#[derive(Clone, Copy, PartialEq)]
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

    fn cdp(self) -> CaptureScreenshotFormat {
        match self {
            Self::Png => CaptureScreenshotFormat::Png,
            Self::Jpeg => CaptureScreenshotFormat::Jpeg,
            Self::Webp => CaptureScreenshotFormat::Webp,
        }
    }
}

pub struct Opts {
    pub viewport: Viewport,
    pub full: bool,
    pub dark: bool,
    pub wait_ms: u64,
    pub wait_for: Option<String>,
    pub timeout: Duration,
    pub format: Format,
}

pub struct Shot {
    /// Captured page height in CSS pixels (viewport height, or document height when full).
    pub height: u32,
    /// Device scale factor actually used (full-page shots too tall for Chrome's
    /// ~16k texture limit fall back to 1x).
    pub scale: f64,
    pub bytes: u64,
}

pub struct Session {
    browser: Browser,
    handler: JoinHandle<()>,
    /// Browser's real UA with "HeadlessChrome" scrubbed, so sites don't serve degraded pages.
    user_agent: Option<String>,
}

impl Session {
    pub async fn launch(chrome: Option<PathBuf>, viewport: Viewport) -> Result<Self> {
        let mut config = BrowserConfig::builder().window_size(viewport.width, viewport.height);
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
        Ok(Self { browser, handler, user_agent })
    }

    pub async fn capture(&self, url: &str, out: &Path, opts: &Opts) -> Result<Shot> {
        let page = tokio::time::timeout(opts.timeout, self.browser.new_page("about:blank"))
            .await
            .map_err(|_| anyhow!("timed out opening a tab"))??;
        let result = tokio::time::timeout(opts.timeout, self.pipeline(&page, url, out, opts))
            .await
            .map_err(|_| anyhow!("timed out after {}s", opts.timeout.as_secs()))
            .and_then(|r| r);
        let _ = page.close().await;
        result
    }

    async fn pipeline(&self, page: &Page, url: &str, out: &Path, opts: &Opts) -> Result<Shot> {
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

        let ua = if v.mobile { Some(IPHONE_UA.to_string()) } else { self.user_agent.clone() };
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
            self.eval(page, wait_for_js(selector, budget.as_millis() as u64)).await?;
        }
        if opts.full {
            self.eval(page, SCROLL_JS.into()).await?;
        }
        if opts.wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(opts.wait_ms)).await;
            self.eval(page, SETTLE_JS.into()).await?;
        }

        let mut params = ScreenshotParams::builder().format(opts.format.cdp());
        if opts.format != Format::Png {
            params = params.quality(90);
        }

        let (height, scale, data) = if opts.full {
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
                (doc_h, v.scale, page.screenshot(params.build()).await?)
            } else {
                (doc_h, 1.0, page.screenshot(params.full_page(true).build()).await?)
            }
        } else {
            (v.height, v.scale, page.screenshot(params.build()).await?)
        };

        let bytes = data.len() as u64;
        tokio::fs::write(out, data)
            .await
            .with_context(|| format!("failed to write {}", out.display()))?;
        Ok(Shot { height, scale, bytes })
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
        Ok(resp.result.result.value.clone().unwrap_or(serde_json::Value::Null))
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
    #[cfg(not(target_os = "macos"))]
    const CANDIDATES: &[&str] = &[
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Fonts loaded + two animation frames painted.
const SETTLE_JS: &str = r#"(async () => {
  if (document.fonts) { try { await document.fonts.ready; } catch {} }
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
    format!(
        r#"(async () => {{
  const deadline = Date.now() + {budget_ms};
  while (!document.querySelector({selector:?})) {{
    if (Date.now() > deadline) throw new Error("selector never appeared: " + {selector:?});
    await new Promise(r => setTimeout(r, 100));
  }}
}})()"#
    )
}
