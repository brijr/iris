use std::process::Command;

use serde_json::Value;
use url::Url;

#[test]
fn json_batch_reports_success_and_failure_on_stdout() {
    let temp = std::env::temp_dir().join(format!("iris-json-batch-{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();

    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/precise-capture.html")
        .canonicalize()
        .unwrap();
    let success_url = Url::from_file_path(&fixture).unwrap();
    let mut missing_url = success_url.clone();
    missing_url.set_query(Some("missing=1"));

    let output = Command::new(env!("CARGO_BIN_EXE_iris"))
        .current_dir(&temp)
        .args([
            "--selector",
            ".capture-target",
            "--padding",
            "8",
            "--scale",
            "1",
            "--timeout",
            "3",
            "--jobs",
            "2",
            "--json",
            "-o",
            "shots",
            success_url.as_str(),
            missing_url.as_str(),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stderr.is_empty(),
        "JSON capture errors must not leak to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let reports: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(reports.len(), 2);

    let success = reports
        .iter()
        .find(|report| report["status"] == "ok")
        .unwrap();
    let failure = reports
        .iter()
        .find(|report| report["status"] == "error")
        .unwrap();

    assert_eq!(success["mode"], "element");
    assert_eq!(success["selector"], ".capture-target");
    assert_eq!(success["padding"], 8);
    assert!(success["output"].as_str().unwrap().starts_with('/'));
    assert!(std::path::Path::new(success["output"].as_str().unwrap()).exists());

    assert_eq!(failure["mode"], "element");
    assert_eq!(failure["selector"], ".capture-target");
    assert_eq!(failure["padding"], 8);
    assert!(failure["output"].as_str().unwrap().starts_with('/'));
    assert!(
        failure["error"]
            .as_str()
            .unwrap()
            .contains("selector never appeared: .capture-target")
    );

    std::fs::remove_dir_all(temp).unwrap();
}
