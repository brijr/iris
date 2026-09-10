use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One reproducible screenshot: the URL, viewport, interactions, redactions,
/// annotations, framing, session, and output filename. Runs locally and in CI
/// with `iris --workflow recipe.yaml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// URL to capture. Bare localhost uses HTTP; other bare hosts use HTTPS.
    pub url: String,
    /// Viewport as WxH or desktop, iphone, or ipad. Defaults to desktop.
    pub size: Option<String>,
    /// Device scale factor overriding the viewport preset.
    pub scale: Option<f64>,
    /// Capture the full page height. Conflicts with selector.
    #[serde(default)]
    pub full: bool,
    /// Capture the first element matching this CSS selector.
    pub selector: Option<String>,
    /// Uniform CSS-pixel padding around a selected element.
    pub padding: Option<u32>,
    /// Force a color scheme: light, dark, or system. Defaults to system.
    pub color_scheme: Option<crate::mcp::ColorSchemeRequest>,
    /// Capture with a saved session (`iris login --session NAME <url>`).
    pub session: Option<String>,
    /// Image format. Defaults to png; a recognized output extension wins.
    pub format: Option<crate::mcp::ImageFormat>,
    /// Extra settle delay in milliseconds after smart waiting.
    #[serde(default)]
    pub wait_ms: u64,
    /// Wait until this CSS selector exists before capturing.
    pub wait_for: Option<String>,
    /// Per-page timeout in seconds. Defaults to 30.
    pub timeout_seconds: Option<u64>,
    /// Ordered interactions to perform before capturing.
    #[serde(default)]
    pub steps: Vec<crate::mcp::StepRequest>,
    /// Redact elements: cover every match of each CSS selector.
    #[serde(default)]
    pub redact: Vec<String>,
    /// Blur masked content instead of covering it with ink.
    pub mask_blur_px: Option<u32>,
    /// Also redact sensitive-data matches (off by default).
    #[serde(default)]
    pub redact_patterns: Vec<crate::mcp::RedactPattern>,
    /// Numbered markers with labels pointing at elements, in list order.
    #[serde(default)]
    pub annotations: Vec<AnnotationRecipe>,
    /// Outline the first element matching each CSS selector.
    #[serde(default)]
    pub highlight: Vec<String>,
    /// Dim the page outside annotated and highlighted elements.
    #[serde(default)]
    pub dim: bool,
    /// Image path to write. Parent directories are created.
    pub output: PathBuf,
}

/// One numbered marker. `number` is accepted for readability but ignored:
/// markers number in list order, matching `--point` flag order.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationRecipe {
    pub selector: String,
    pub label: String,
    // Accepted so recipe files can number their own walkthrough steps; the
    // runner always numbers in list order.
    #[allow(dead_code)]
    pub number: Option<u32>,
}

/// Read and parse a recipe file. `.yaml`/`.yml` parse as YAML, `.json` as
/// JSON; anything else is rejected so typos fail fast.
pub fn load_recipe(path: &Path) -> Result<Recipe> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow {}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "yaml" | "yml" => serde_saphyr::from_str(&text)
            .with_context(|| format!("failed to parse workflow {}", path.display())),
        "json" => serde_json::from_str(&text)
            .with_context(|| format!("failed to parse workflow {}", path.display())),
        _ => bail!(
            "unsupported workflow extension for {}: use .yaml, .yml, or .json",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_YAML: &str = r##"
url: https://app.example.com
size: desktop
steps:
  - click: "text=Conditions"
redact:
  - ".client-name"
annotations:
  - selector: "#search"
    number: 1
    label: Find a condition
  - selector: "#run-analysis"
    number: 2
    label: Review documents
output: shots/conditions.png
"##;

    #[test]
    fn yaml_and_json_recipes_parse_the_feedback_shape() {
        let yaml: Recipe = serde_saphyr::from_str(EXAMPLE_YAML).unwrap();
        assert_eq!(yaml.url, "https://app.example.com");
        assert_eq!(yaml.size.as_deref(), Some("desktop"));
        assert_eq!(yaml.steps.len(), 1);
        assert_eq!(yaml.redact, vec![".client-name"]);
        assert_eq!(yaml.annotations.len(), 2);
        assert_eq!(yaml.annotations[0].label, "Find a condition");
        assert_eq!(yaml.output, PathBuf::from("shots/conditions.png"));

        let json = serde_json::json!({
            "url": "example.com",
            "full": true,
            "color_scheme": "dark",
            "format": "webp",
            "output": "shots/page.webp",
        });
        let parsed: Recipe = serde_json::from_value(json).unwrap();
        assert!(parsed.full);
        assert!(matches!(
            parsed.color_scheme,
            Some(crate::mcp::ColorSchemeRequest::Dark)
        ));
        assert!(matches!(parsed.format, Some(crate::mcp::ImageFormat::Webp)));
    }

    #[test]
    fn recipes_reject_unknown_fields_and_missing_outputs() {
        let unknown =
            serde_saphyr::from_str::<Recipe>("url: example.com\nfrobnicate: 1\noutput: a.png");
        assert!(unknown.unwrap_err().to_string().contains("frobnicate"));

        let missing: Result<Recipe, _> =
            serde_json::from_value(serde_json::json!({"url": "example.com"}));
        assert!(missing.unwrap_err().to_string().contains("output"));
    }

    #[test]
    fn recipe_request_conversion_keeps_every_field() {
        let recipe: Recipe = serde_saphyr::from_str(EXAMPLE_YAML).unwrap();
        let request = crate::mcp::CaptureRequest::from(recipe);
        let prepared = request.prepare().unwrap();
        assert_eq!(prepared.url.as_str(), "https://app.example.com/");
        assert_eq!(prepared.opts.steps.len(), 1);
        assert_eq!(prepared.opts.masks, vec![".client-name"]);
        assert_eq!(prepared.opts.annotations.len(), 2);
        assert_eq!(prepared.opts.annotations[0].number, 1);
        assert_eq!(prepared.output, Some(PathBuf::from("shots/conditions.png")));
    }
}
