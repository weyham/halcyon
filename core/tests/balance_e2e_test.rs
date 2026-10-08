//! 用量查询 e2e：本地假上游 + custom 规则 / 内置模板，断言"取数 → 解析"全链路。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::Receiver;

use halcyon_core::balance;
use halcyon_core::config::{BalanceCfg, CustomSpec, Route, RowSource, RowSpec};

/// 起一次性假上游：返回 200 + body；把收到的 Authorization 头经 channel 送回。
fn serve_once(body: &'static str) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let auth = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .unwrap_or("")
            .to_string();
        tx.send(auth).unwrap();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).unwrap();
    });
    (format!("http://{addr}"), rx)
}

fn route_with_balance(upstream: &str, balance: BalanceCfg) -> Route {
    Route {
        name: "r".to_string(),
        upstream: upstream.to_string(),
        web_search: "note".to_string(),
        web_search_note_max: 20,
        aliases: Vec::new(),
        balance: Some(balance),
    }
}

/// 全新形状的供应商：纯配置（custom 规则）完成 取数→解析，Authorization 原样带上。
#[test]
fn e2e_custom_shape_zero_code() {
    let body = r#"{"quota": {"windows": [{"name": "5小时", "usedPct": 42.4, "resetAt": "2026-09-21T20:00:00Z"}]}}"#;
    let (base, rx) = serve_once(body);
    let spec = CustomSpec {
        sources: vec![RowSource {
            rows_path: Some("quota.windows".to_string()),
            row: RowSpec {
                label: Some("name".to_string()),
                pct: Some("usedPct".to_string()),
                reset: Some("resetAt".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let route = route_with_balance(
        &format!("{base}/v1"),
        BalanceCfg {
            url: Some(format!("{base}/api/quota")),
            template: Some("custom".to_string()),
            custom: Some(spec),
        },
    );
    let query = balance::resolve_query(&route).unwrap();
    assert_eq!(query.url, format!("{base}/api/quota"));
    let resp = balance::fetch_json(&query.url, "Bearer test-key").unwrap();
    let auth = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let (name, value) = auth.split_once(':').unwrap();
    assert!(name.eq_ignore_ascii_case("authorization"), "{auth}");
    assert_eq!(value.trim(), "Bearer test-key", "Authorization 值原样透传");
    let rows = balance::parse_custom(&query.spec, &resp).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "5小时");
    assert_eq!(rows[0].pct, Some(42));
    assert_eq!(rows[0].reset.as_deref(), Some("2026-09-21 20:00"));
}

/// 内置模板 + URL 覆盖：GLM 形状经统一引擎解析（e2e 回归）。
#[test]
fn e2e_builtin_template_url_override() {
    let body = r#"{"code":200,"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":62.4}]}}"#;
    let (base, _rx) = serve_once(body);
    let route = route_with_balance(
        &format!("{base}/api/v1"),
        BalanceCfg {
            url: Some(format!("{base}/api/monitor/usage/quota/limit")),
            template: Some("glm".to_string()),
            custom: None,
        },
    );
    let query = balance::resolve_query(&route).unwrap();
    let resp = balance::fetch_json(&query.url, "Bearer k").unwrap();
    let rows = balance::parse_custom(&query.spec, &resp).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].menu_text(), "5h 额度 62%");
}

/// 供应商改形状：HTTP 成功但解析失败（面板据此显示"解析失败"+原始数据，不显假零）。
#[test]
fn e2e_degraded_shape_parse_error() {
    let body = r#"{"v2": {"everything": "changed"}}"#;
    let (base, _rx) = serve_once(body);
    let route = route_with_balance(
        &format!("{base}/api/v1"),
        BalanceCfg {
            url: Some(format!("{base}/api/monitor/usage/quota/limit")),
            template: Some("glm".to_string()),
            custom: None,
        },
    );
    let query = balance::resolve_query(&route).unwrap();
    let resp = balance::fetch_json(&query.url, "Bearer k").unwrap();
    let err = balance::parse_custom(&query.spec, &resp).unwrap_err();
    assert!(err.contains("未提取到任何档位行"), "{err}");
}
