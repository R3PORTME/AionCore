//! E2E coverage for the agent-facing `aioncore team` CLI fallback.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

fn team_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    command.arg("team");
    command
}

#[tokio::test]
async fn team_capabilities_prints_contract_without_runtime_env() {
    let output = team_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "team capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["contract"], "agent-facing-team-cli");
    assert_eq!(stdout["data"]["tools"].as_array().unwrap().len(), 13);
    let spawn = stdout["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "team_spawn_agent")
        .unwrap();
    assert_eq!(spawn["lead_only"], true);
    assert!(spawn["stdin_json_schema"]["properties"]["assistant_id"].is_object());
}

#[tokio::test]
async fn team_help_prints_markdown_without_runtime_env() {
    let output = team_command()
        .arg("help")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(output.status.success());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["format"], "markdown");
    assert!(stdout["data"]["text"].as_str().unwrap().contains("team send-message"));
}

#[tokio::test]
async fn tool_command_rejects_forged_identity_fields_before_http_call() {
    let mut child = team_command()
        .args(["send-message"])
        .env("AIONUI_BASE_URL", "http://127.0.0.1:9")
        .env("AIONUI_CONVERSATION_ID", "conv-1")
        .env("AIONUI_USER_ID", "user-1")
        .env("AIONUI_RUNTIME_TOKEN", "token-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"to":"worker-1","message":"hi","team_id":"team-1","slot_id":"lead-1","role":"lead"}"#)
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_SCHEMA_VALIDATION_FAILED"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "schema_validation_failed");
    assert!(stdout["error"]["details"]["expected_schema"].is_object());
}

#[tokio::test]
async fn team_context_requires_runtime_env_and_prints_json_error() {
    let output = team_command()
        .arg("context")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_ENV_MISSING"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "runtime_context_missing");
    assert_eq!(stdout["meta"]["command"], "team context");
}

#[tokio::test]
async fn unknown_team_command_returns_json_error_envelope() {
    let output = team_command().arg("does-not-exist").output().await.unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_UNKNOWN_COMMAND"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "unknown_tool");
    assert_eq!(stdout["meta"]["command"], "team does-not-exist");
}

async fn run_team_call(base_url: &str, args: &[&str], input: &str) -> std::process::Output {
    let mut child = team_command()
        .args(args)
        .env("AIONUI_BASE_URL", base_url)
        .env("AIONUI_USER_ID", "user-1")
        .env("AIONUI_CONVERSATION_ID", "conv-1")
        .env("AIONUI_RUNTIME_TOKEN", "token-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(input.as_bytes()).await.unwrap();
    drop(child.stdin.take());
    child.wait_with_output().await.unwrap()
}

#[tokio::test]
async fn repeated_team_cli_calls_reach_the_same_runtime_bridge() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    let app = Router::new().route(
        "/api/runtime/team-tools/call",
        post(move |Json(request): Json<Value>| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "success": true, "data": { "tool": request["tool"] } }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let calls = [
        (&["members"][..], "{}", "team_members"),
        (&["read-messages"][..], "{}", "team_read_messages"),
        (&["task", "list"][..], "{}", "team_task_list"),
        (
            &["task", "update"][..],
            r#"{"task_id":"task-1","status":"completed"}"#,
            "team_task_update",
        ),
        (
            &["send-message"][..],
            r#"{"to":"lead-1","message":"done"}"#,
            "team_send_message",
        ),
    ];
    for _ in 0..4 {
        for (args, input, tool) in calls {
            let output = run_team_call(&base_url, args, input).await;
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(envelope["success"], true);
            assert_eq!(envelope["data"]["tool"], tool);
        }
    }
    assert_eq!(received.load(Ordering::SeqCst), 20);
    server.abort();
}

#[tokio::test]
async fn team_cli_retries_connect_failure_then_reports_success_only_after_receipt() {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, sleep};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let server = tokio::spawn(async move {
        sleep(Duration::from_millis(160)).await;
        let listener = TcpListener::bind(addr).await.unwrap();
        let app = Router::new().route(
            "/api/runtime/team-tools/call",
            post(|| async { Json(json!({ "success": true, "data": { "members": [] } })) }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let output = run_team_call(&format!("http://{addr}"), &["members"], "{}").await;
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["success"], true);
    server.abort();
}

#[tokio::test]
async fn unavailable_bridge_never_claims_task_completion_or_report_delivery() {
    use serde_json::Value;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    for (args, input) in [
        (&["task", "update"][..], r#"{"task_id":"task-1","status":"completed"}"#),
        (&["send-message"][..], r#"{"to":"lead-1","message":"done"}"#),
    ] {
        let output = run_team_call(&base_url, args, input).await;
        assert_eq!(output.status.code(), Some(2));
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["success"], false);
        assert_eq!(envelope["error"]["code"], "transport_unavailable");
        assert_eq!(envelope["error"]["details"]["connect_error"], true);
        assert_eq!(envelope["error"]["details"]["attempts"], 4);
        assert!(envelope["error"]["details"]["io_kind"].is_string());
        assert!(String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_HTTP_BRIDGE_FAILED"));
    }
}

#[tokio::test]
async fn backend_failure_does_not_claim_task_completion_or_retry_a_mutation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    let app = Router::new().route(
        "/api/runtime/team-tools/call",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "success": false, "error": { "code": "transport_unavailable", "message": "backend unavailable" } })),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let output = run_team_call(
        &base_url,
        &["task", "update"],
        r#"{"task_id":"task-1","status":"completed"}"#,
    )
    .await;
    assert_eq!(output.status.code(), Some(3));
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["success"], false);
    assert_eq!(envelope["error"]["code"], "transport_unavailable");
    assert_eq!(received.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn failure_envelope_with_http_ok_does_not_claim_report_delivery() {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    let app = Router::new().route(
        "/api/runtime/team-tools/call",
        post(|| async {
            Json(json!({
                "success": false,
                "error": { "code": "transport_unavailable", "message": "delivery failed" }
            }))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let output = run_team_call(&base_url, &["send-message"], r#"{"to":"lead-1","message":"done"}"#).await;
    assert_eq!(output.status.code(), Some(3));
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["success"], false);
    assert_eq!(envelope["error"]["code"], "transport_unavailable");
    assert!(String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_RESPONSE_ERROR"));
    server.abort();
}
