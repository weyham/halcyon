//! server.rs 集成测试：本地 echo 上游 + 真 TCP 客户端，验证逐字透传 / 改写 / SSE 流式 / 4xx 回传。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use halcyon_core::config::{Config, ResolvedConfig, RouteEntry};
use halcyon_core::logging::LogHealth;
use halcyon_core::server;

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    body: Vec<u8>,
}

/// 启动一个 echo 上游：/sse 发 3 块延迟 chunk，/err 回 401，其余把请求体原样回显。
/// 收到的每个请求通过 channel 上报。
fn start_echo_upstream() -> (SocketAddr, Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel::<Captured>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let tx = tx.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    return;
                }
                let mut parts = line.split_whitespace();
                let _method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("/").to_string();
                let mut length = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                if path.contains("/sse") {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .unwrap();
                    for chunk in ["data: one\n\n", "data: two\n\n", "data: three\n\n"] {
                        let frame = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
                        stream.write_all(frame.as_bytes()).unwrap();
                        stream.flush().unwrap();
                        thread::sleep(Duration::from_millis(150));
                    }
                    stream.write_all(b"0\r\n\r\n").unwrap();
                    stream.flush().unwrap();
                } else if path.contains("/err") {
                    let err = br#"{"error":{"message":"invalid api key"}}"#;
                    let head = format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        err.len()
                    );
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(err).unwrap();
                } else {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                }
                let _ = tx.send(Captured {
                    path: path.clone(),
                    body,
                });
                let _ = stream.flush();
            });
        }
    });
    (addr, rx)
}

fn shim_config(upstream: SocketAddr, extra_route: bool) -> ResolvedConfig {
    let mut routes = std::collections::BTreeMap::new();
    routes.insert(
        "e".to_string(),
        RouteEntry {
            upstream: format!("http://{upstream}/base/v1"),
            ..Default::default()
        },
    );
    if extra_route {
        routes.insert(
            "other".to_string(),
            RouteEntry {
                upstream: "http://127.0.0.1:9/none".to_string(),
                ..Default::default()
            },
        );
    }
    ResolvedConfig::try_from(Config {
        listen: "127.0.0.1:0".to_string(),
        routes,
        ..Default::default()
    })
    .unwrap()
}

/// 发一个原始 HTTP 请求，返回 (status, 响应头文本, body)。
fn raw_request(addr: SocketAddr, req: &[u8]) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(req).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut heads = String::new();
    let mut length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        if h == "\r\n" || h.is_empty() {
            break;
        }
        heads.push_str(&h);
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok();
            }
            if k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.trim().eq_ignore_ascii_case("chunked")
            {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if let Some(l) = length {
        body.resize(l, 0);
        reader.read_exact(&mut body).unwrap();
    } else if chunked {
        loop {
            let mut sz = String::new();
            reader.read_line(&mut sz).unwrap();
            let n = usize::from_str_radix(sz.trim(), 16).unwrap();
            if n == 0 {
                let mut end = String::new();
                let _ = reader.read_line(&mut end);
                break;
            }
            let mut c = vec![0u8; n];
            reader.read_exact(&mut c).unwrap();
            body.extend_from_slice(&c);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    }
    (status, heads, body)
}

#[test]
fn passthrough_byte_exact_when_nothing_to_rewrite() {
    let (up, rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, false)).unwrap();
    let body = br#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
    let req = format!(
        "POST /e/responses HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    let (status, _h, resp_body) = raw_request(shim.addr(), &full);
    assert_eq!(status, 200);
    assert_eq!(resp_body, body, "echo 回来的应与本身体一致");
    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        got.path, "/base/v1/responses",
        "前缀剥掉后接到 upstream base 路径上"
    );
    assert_eq!(got.body, body, "无改写时请求体必须逐字节一致");
    shim.shutdown();
}

#[test]
fn orphan_output_is_rewritten_before_forwarding() {
    let (up, rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, false)).unwrap();
    let body = br#"{"input":[{"type":"function_call_output","name":"send_message_to_thread","namespace":"codex_app","output":"hello"}]}"#;
    let req = format!(
        "POST /e/responses HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    let (status, _h, _b) = raw_request(shim.addr(), &full);
    assert_eq!(status, 200);
    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let text = String::from_utf8(got.body).unwrap();
    assert!(
        text.contains(r#""type":"message""#),
        "孤儿条目应被改写成 user 消息：{text}"
    );
    assert!(text.contains("跨任务消息"), "应带来源标注头：{text}");
    assert!(
        !text.contains("function_call_output"),
        "原条目不应再出现：{text}"
    );
    shim.shutdown();
}

#[test]
fn interleaved_tool_round_is_reordered_before_forwarding() {
    let (up, rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, false)).unwrap();
    let body = br#"{"input":[{"type":"function_call","call_id":"sg_call_1","name":"exec_command","arguments":"{}"},{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking"}]},{"type":"function_call_output","call_id":"sg_call_1","output":"ok"}]}"#;
    let req = format!(
        "POST /e/responses HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    let (status, _h, _b) = raw_request(shim.addr(), &full);
    assert_eq!(status, 200);

    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
    assert_eq!(forwarded["input"][0]["type"], "function_call");
    assert_eq!(forwarded["input"][1]["type"], "function_call_output");
    assert_eq!(forwarded["input"][2]["type"], "reasoning");
    shim.shutdown();
}

#[test]
fn empty_text_message_is_dropped_before_forwarding() {
    let (up, rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, false)).unwrap();
    let body = br#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}]}"#;
    let req = format!(
        "POST /e/responses HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    let (status, _h, _b) = raw_request(shim.addr(), &full);
    assert_eq!(status, 200);

    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&got.body).unwrap();
    assert_eq!(forwarded["input"].as_array().unwrap().len(), 1);
    assert_eq!(forwarded["input"][0]["role"], "user");
    shim.shutdown();
}

#[test]
fn sse_streams_chunk_by_chunk() {
    let (up, _rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, false)).unwrap();
    let mut stream = TcpStream::connect(shim.addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(
            b"POST /e/sse HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("200"), "{line}");
    let mut header_text = String::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        if h == "\r\n" || h.is_empty() {
            break;
        }
        header_text.push_str(&h);
    }
    assert!(
        header_text
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{header_text}"
    );

    // 逐块到达：第 1 块与最后一块之间应有明显时间差（上游每块延迟 150ms）
    let start = Instant::now();
    let mut arrivals = Vec::new();
    let mut content = String::new();
    loop {
        let mut sz = String::new();
        reader.read_line(&mut sz).unwrap();
        let n = usize::from_str_radix(sz.trim(), 16).unwrap();
        if n == 0 {
            break;
        }
        let mut c = vec![0u8; n];
        reader.read_exact(&mut c).unwrap();
        arrivals.push(start.elapsed());
        content.push_str(&String::from_utf8(c).unwrap());
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
    assert_eq!(arrivals.len(), 3, "应收到 3 块：{content}");
    assert_eq!(content, "data: one\n\ndata: two\n\ndata: three\n\n");
    let gap = arrivals[2] - arrivals[0];
    assert!(
        gap >= Duration::from_millis(200),
        "块间应有延迟（真流式），实际 {gap:?}"
    );
    shim.shutdown();
}

#[test]
fn upstream_4xx_is_returned_with_body_intact() {
    let (up, _rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, false)).unwrap();
    let req =
        b"POST /e/err HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, _h, body) = raw_request(shim.addr(), req);
    assert_eq!(status, 401);
    assert_eq!(body, br#"{"error":{"message":"invalid api key"}}"#);
    shim.shutdown();
}

#[test]
fn health_and_unknown_route() {
    let (up, _rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, true)).unwrap();

    let (status, _h, body) = raw_request(
        shim.addr(),
        b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains(r#""version""#), "{text}");
    assert!(text.contains(r#""e""#), "routes 里应有 e：{text}");

    let (status, _h, body) = raw_request(
        shim.addr(),
        b"GET /nope/x HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 404);
    assert!(String::from_utf8(body).unwrap().contains("no route"));
    shim.shutdown();
}

#[test]
fn health_exposes_file_log_health() {
    let (up, _rx) = start_echo_upstream();
    let log_health = Arc::new(LogHealth::new());
    let shim = server::start_with_log_health(shim_config(up, true), Some(log_health)).unwrap();

    let (status, _headers, body) = raw_request(
        shim.addr(),
        b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(health["log_writable"], serde_json::json!(false));
    assert_eq!(health["log_mode"], serde_json::json!("primary"));
    assert_eq!(health["log_write_failures"], serde_json::json!(0));
    shim.shutdown();
}

#[test]
fn route_activity_is_recorded_per_route() {
    let (up, _rx) = start_echo_upstream();
    let shim = server::start(shim_config(up, true)).unwrap();

    let body = b"{}";
    let req = format!(
        "POST /e/responses HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    let (status, _h, _b) = raw_request(shim.addr(), &full);
    assert_eq!(status, 200);

    let (status, _h, health) = raw_request(
        shim.addr(),
        b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_slice(&health).unwrap();
    let activity = json["route_last_request_ms"].as_object().unwrap();
    assert!(
        activity["e"].as_u64().unwrap_or(0) > 0,
        "有流量的路由应记录时间：{json}"
    );
    assert!(
        activity.get("other").is_none(),
        "无流量的路由不应出现：{json}"
    );
    shim.shutdown();
}
