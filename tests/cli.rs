use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use base64::Engine as _;
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

#[test]
fn mcp_stdio_lists_one_tool_and_returns_an_inline_capture() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/precise-capture.html")
        .canonicalize()
        .unwrap();
    let url = Url::from_file_path(fixture).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_iris"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            sender.send(line.unwrap()).unwrap();
        }
    });
    let mut stdin = child.stdin.take().unwrap();

    send_message(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "iris-test", "version": "0.0.0" }
            }
        }),
    );
    let initialized = receive_response(&receiver, 1, Duration::from_secs(5));
    assert_eq!(initialized["result"]["serverInfo"]["name"], "iris");

    send_message(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    );
    send_message(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let listed = receive_response(&receiver, 2, Duration::from_secs(5));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "capture");

    send_message(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "capture",
                "arguments": {
                    "url": url.as_str(),
                    "selector": ".capture-target",
                    "padding": 10,
                    "size": "320x240",
                    "scale": 1,
                    "timeout_seconds": 3
                }
            }
        }),
    );
    let captured = receive_response(&receiver, 3, Duration::from_secs(15));
    let result = &captured["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["status"], "ok");
    assert_eq!(result["structuredContent"]["css_width"], 141);
    assert_eq!(result["structuredContent"]["css_height"], 81);
    assert!(result["structuredContent"].get("output").is_none());
    let image = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|content| content["type"] == "image")
        .unwrap();
    assert_eq!(image["mimeType"], "image/png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(png_dimensions(&bytes), (141, 81));

    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("Iris MCP server did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    reader.join().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.is_empty(),
        "MCP diagnostics leaked to stderr: {stderr}"
    );
}

fn send_message(stdin: &mut impl Write, message: Value) {
    writeln!(stdin, "{}", serde_json::to_string(&message).unwrap()).unwrap();
    stdin.flush().unwrap();
}

fn receive_response(receiver: &Receiver<String>, id: u64, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = receiver
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for MCP response id {id}"));
        let message: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("MCP stdout was not JSON: {error}: {line}"));
        if message["id"] == id {
            return message;
        }
    }
}

fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    (
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    )
}
