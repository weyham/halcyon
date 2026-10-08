//! HTTP 服务：前缀路由、请求改写、转发（含 SSE 逐块回传）。1:1 对应旧 Python `server.py`。
//!
//! 职责：
//!
//! - `/<route>/**` → 改写请求体（见 rewrite.rs）→ 转发到该路由的真实上游；
//! - `GET /health` → 版本、路由数、改写计数；
//! - 请求头（含 Authorization）**逐字透传**，不读取、不记录、不落盘；
//! - 未发生改写时，请求体**逐字节**转发（保证正常条目零改动）。
//!
//! 服务端是刻意手写的最小 HTTP/1.1 实现（线程/连接、连接内串行、保活），
//! 因为 SSE 要求每块写出后立即 flush——通用框架的用户态缓冲会把流式响应整段憋住。

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::ResolvedConfig;
use crate::logging::LogHealth;
use crate::rewrite::{rewrite_request_body, RewriteReport};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 内存态密钥表：路由名 → 最近一次请求的 Authorization 头原值。
/// **绝不落盘、不进日志、不回显**（仅用于余额/额度查询）。
pub type KeyRing = Arc<Mutex<HashMap<String, String>>>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const READ_TIMEOUT: Duration = Duration::from_secs(900);
pub const READ_CHUNK: usize = 32 * 1024;
pub const ERROR_BODY_LIMIT: usize = 256 * 1024;
pub const ERROR_LOG_LIMIT: usize = 400;
const MAX_LINE: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// 运行期计数（供 /health 与日志使用，不记录任何正文）。
///
/// `clients` 按请求头 `originator` 归类（Codex CLI 是 `codex_exec`，桌面端是
/// `codex_app` 之类），用来回答"这次的请求到底是不是桌面端发出来的"。
pub struct Stats {
    started_at: Instant,
    inner: Mutex<StatsInner>,
}

#[derive(Default)]
struct StatsInner {
    requests: u64,
    rewritten_requests: u64,
    orphan_outputs: u64,
    tool_rounds_repaired: u64,
    empty_messages_dropped: u64,
    agent_messages_normalized: u64,
    empty_reasoning_dropped: u64,
    web_search_noted: u64,
    web_search_dropped: u64,
    web_search_normalized: u64,
    upstream_errors: u64,
    clients: HashMap<String, u64>,
    /// 最近一次转发请求的时间（epoch 毫秒）；0 = 还没有流量。供托盘活动闪烁用。
    last_request_ms: u64,
    /// 最近一次有响应字节流过的时间（epoch 毫秒）；0 = 还没有。token 流出 = Agent 在工作。
    last_activity_ms: u64,
    /// 每路由最近一次被命中的时间（epoch 毫秒）；托盘用量区按它过滤"当前有流量的路由"。
    route_last_request_ms: HashMap<String, u64>,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            inner: Mutex::new(StatsInner::default()),
        }
    }

    pub fn record_client(&self, originator: &str) {
        let mut g = self.inner.lock().unwrap();
        let key = if originator.is_empty() {
            "(未标注)"
        } else {
            originator
        };
        *g.clients.entry(key.to_string()).or_insert(0) += 1;
    }

    /// 请求成功匹配到路由时调用：该路由"本次启动以来有流量"。
    pub fn record_route(&self, route: &str) {
        self.inner
            .lock()
            .unwrap()
            .route_last_request_ms
            .insert(route.to_string(), now_ms());
    }

    pub fn record(&self, report: &RewriteReport) {
        let mut g = self.inner.lock().unwrap();
        g.requests += 1;
        let now = now_ms();
        g.last_request_ms = now;
        g.last_activity_ms = now;
        if report.changed() {
            g.rewritten_requests += 1;
        }
        g.orphan_outputs += report.orphan_outputs as u64;
        g.tool_rounds_repaired += report.tool_rounds_repaired as u64;
        g.empty_messages_dropped += report.empty_messages_dropped as u64;
        g.agent_messages_normalized += report.agent_messages_normalized as u64;
        g.empty_reasoning_dropped += report.empty_reasoning_dropped as u64;
        g.web_search_noted += report.web_search_noted as u64;
        g.web_search_dropped += report.web_search_dropped as u64;
        g.web_search_normalized += report.web_search_normalized as u64;
    }

    /// 转发响应字节时调用（pump 每块一次）：Agent 正在输出的信号。
    pub fn record_activity(&self) {
        self.inner.lock().unwrap().last_activity_ms = now_ms();
    }

    /// 最近一次活动时间（供托盘闪烁，避免重复构造 snapshot）。
    pub fn last_activity_ms(&self) -> u64 {
        self.inner.lock().unwrap().last_activity_ms
    }

    pub fn record_upstream_error(&self) {
        self.inner.lock().unwrap().upstream_errors += 1;
    }

    pub fn snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "version": VERSION,
            "uptime_seconds": (self.started_at.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "requests": g.requests,
            "rewritten_requests": g.rewritten_requests,
            "orphan_outputs": g.orphan_outputs,
            "tool_rounds_repaired": g.tool_rounds_repaired,
            "empty_messages_dropped": g.empty_messages_dropped,
            "agent_messages_normalized": g.agent_messages_normalized,
            "empty_reasoning_dropped": g.empty_reasoning_dropped,
            "web_search_noted": g.web_search_noted,
            "web_search_dropped": g.web_search_dropped,
            "web_search_normalized": g.web_search_normalized,
            "upstream_errors": g.upstream_errors,
            "clients": g.clients,
            "last_request_ms": g.last_request_ms,
            "last_activity_ms": g.last_activity_ms,
            "route_last_request_ms": g.route_last_request_ms,
        })
    }
}

/// 把上游错误体压成一行摘要（优先取 `error.message`），供日志定位 4xx/5xx 原因。
pub fn summarize_error_body(body: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(body).into_owned();
    if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
        if let Some(m) = parsed
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            text = m.to_string();
        } else if let Some(m) = parsed.get("message").and_then(Value::as_str) {
            text = m.to_string();
        }
    }
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(ERROR_LOG_LIMIT).collect()
}

/// 一个解析完的入站请求。
struct Request {
    method: String,
    target: String, // path + ?query
    version10: bool,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&lower))
            .map(|(_, v)| v.as_str())
    }
}

fn read_line_limited<R: BufRead>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match r.read(&mut byte) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if byte[0] != b'\r' {
                    buf.push(byte[0]);
                }
                if buf.len() > MAX_LINE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "header line too long",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header not utf-8"))
}

fn read_chunked<R: BufRead>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = read_line_limited(r)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "chunked eof"))?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            //  trailer（可能多行）读到空行
            loop {
                match read_line_limited(r)? {
                    None => break,
                    Some(l) if l.is_empty() => break,
                    _ => {}
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        r.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
    }
    Ok(out)
}

/// 解析一个请求；连接关闭/对端 EOF 返回 Ok(None)。
fn read_request<R: BufRead>(r: &mut R) -> io::Result<Option<Request>> {
    let Some(line) = read_line_limited(r)? else {
        return Ok(None);
    };
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let version = parts.next().unwrap_or("");
    if method.is_empty() || !version.starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad request line",
        ));
    }
    let version10 = version == "HTTP/1.0";
    let mut headers = Vec::new();
    loop {
        let Some(l) = read_line_limited(r)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "eof in headers",
            ));
        };
        if l.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many headers",
            ));
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut stub = Request {
        method,
        target,
        version10,
        headers,
        body: Vec::new(),
    };
    let body = if stub
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        read_chunked(r)?
    } else {
        let length: usize = stub
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut b = vec![0u8; length];
        r.read_exact(&mut b)?;
        b
    };
    stub.body = body;
    Ok(Some(stub))
}

fn reason_of(status: u16) -> String {
    ureq::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason().map(str::to_string))
        .unwrap_or_else(|| status.to_string())
}

fn write_head<W: Write>(w: &mut W, status: u16, headers: &[(String, String)]) -> io::Result<()> {
    write!(w, "HTTP/1.1 {status} {}\r\n", reason_of(status))?;
    for (k, v) in headers {
        write!(w, "{k}: {v}\r\n")?;
    }
    write!(w, "\r\n")?;
    w.flush()
}

fn send_json(stream: &mut TcpStream, status: u16, payload: Value, close: bool) -> io::Result<()> {
    let raw = serde_json::to_string(&payload)
        .unwrap_or_else(|_| "{}".to_string())
        .into_bytes();
    let mut headers = vec![
        (
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ),
        ("Content-Length".to_string(), raw.len().to_string()),
    ];
    if close {
        headers.push(("Connection".to_string(), "close".to_string()));
    }
    write_head(stream, status, &headers)?;
    stream.write_all(&raw)?;
    stream.flush()
}

/// 把上游响应逐块回写给客户端（SSE 关键路径：每块写完立刻 flush）。
fn pump<W: Write>(
    mut reader: ureq::BodyReader<'static>,
    w: &mut W,
    chunked: bool,
    stats: &Stats,
) -> io::Result<()> {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e),
        };
        if chunked {
            write!(w, "{n:x}\r\n")?;
            w.write_all(&buf[..n])?;
            write!(w, "\r\n")?;
        } else {
            w.write_all(&buf[..n])?;
        }
        w.flush()?;
        stats.record_activity();
    }
    if chunked {
        write!(w, "0\r\n\r\n")?;
        w.flush()?;
    }
    Ok(())
}

struct Ctx {
    config: ResolvedConfig,
    stats: Arc<Stats>,
    agent: ureq::Agent,
    keyring: KeyRing,
    log_health: Option<Arc<LogHealth>>,
}

/// 处理单个请求（在连接线程内调用）。
fn handle_request(stream: &mut TcpStream, req: &Request, ctx: &Ctx) -> io::Result<bool> {
    let started = Instant::now();
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (req.target.clone(), None),
    };

    if path.trim_end_matches('/') == "/health" {
        let mut payload = ctx.stats.snapshot();
        payload["routes"] = json!(ctx
            .config
            .routes
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>());
        payload["listen"] = json!(ctx.config.listen);
        match ctx.log_health.as_ref() {
            Some(health) => {
                let logging = health.snapshot_json();
                if let Some(fields) = logging.as_object() {
                    for (key, value) in fields {
                        payload[key] = value.clone();
                    }
                }
            }
            None => payload["log_writable"] = Value::Null,
        }
        send_json(stream, 200, payload, false)?;
        return Ok(true);
    }

    let (route, rest) = ctx.config.resolve_route(&path);
    let Some(route) = route else {
        send_json(
            stream,
            404,
            json!({
                "error": "no route",
                "detail": format!("没有匹配的路由：{path}"),
                "routes": ctx.config.routes.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
            }),
            true,
        )?;
        return Ok(false);
    };

    let mut rest = rest;
    if let Some(q) = &query {
        rest = format!("{rest}?{q}");
    }

    let originator = req.header("originator").unwrap_or("").to_string();
    ctx.stats.record_client(&originator);
    ctx.stats.record_route(&route.name);
    // 内存暂存该路由最近一次 Authorization 头（仅用于余额/额度查询，绝不落盘/日志/回显）
    if let Some(auth) = req.header("authorization") {
        if !auth.is_empty() {
            ctx.keyring
                .lock()
                .unwrap()
                .insert(route.name.clone(), auth.to_string());
        }
    }

    let mut report = RewriteReport::default();
    let mut outgoing: Vec<u8> = req.body.clone();
    if !req.body.is_empty() {
        match serde_json::from_slice::<Value>(&req.body) {
            Ok(parsed) if parsed.is_object() => {
                // 裁定：模型修正全部走 app-server 离线处理，
                // 请求出口不再改写 model 字段（原 rewrite_model 链路已移除）。
                let (rewritten, rep) = rewrite_request_body(
                    &parsed,
                    &route.web_search,
                    route.web_search_note_max,
                    ctx.config.orphan_header,
                );
                report = rep;
                if report.changed() {
                    outgoing = serde_json::to_string(&rewritten)
                        .unwrap_or_else(|_| String::from_utf8_lossy(&req.body).into_owned())
                        .into_bytes();
                }
            }
            _ => {
                log::warn!(
                    "body 不是合法 JSON，原样透传 route={} path={path}",
                    route.name
                );
            }
        }
    }
    ctx.stats.record(&report);

    // 目标 URL = upstream base（去尾斜杠）+ rest
    let url = format!(
        "{}{}",
        route.upstream,
        if rest.starts_with('/') {
            rest.clone()
        } else {
            format!("/{rest}")
        }
    );

    let mut builder = ureq::http::Request::builder()
        .method(req.method.as_str())
        .uri(&url);
    for (k, v) in &req.headers {
        let lower = k.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || lower == "host" || lower == "content-length" {
            continue;
        }
        builder = builder.header(k, v);
    }
    let upstream_req = match builder.body(outgoing.clone()) {
        Ok(r) => r,
        Err(e) => {
            ctx.stats.record_upstream_error();
            send_json(
                stream,
                502,
                json!({"error": "upstream error", "detail": e.to_string()}),
                true,
            )?;
            return Ok(false);
        }
    };

    let resp = match ctx.agent.run(upstream_req) {
        Ok(r) => r,
        Err(e) => {
            ctx.stats.record_upstream_error();
            log::warn!("upstream 转发失败 route={} path={path} err={e}", route.name);
            send_json(
                stream,
                502,
                json!({"error": "upstream error", "detail": e.to_string()}),
                true,
            )?;
            return Ok(false);
        }
    };

    let status = resp.status().as_u16();
    let length: Option<usize> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    // 小错误体先读下来记摘要，再原样回给客户端（先取响应头，再消费 body）
    let resp_hdrs = resp_headers(&resp);
    let small_error = status >= 400 && length.is_some_and(|l| l <= ERROR_BODY_LIMIT);
    if small_error {
        let mut body_bytes = Vec::new();
        let mut reader = resp.into_body().into_reader();
        let _ = reader.read_to_end(&mut body_bytes);
        let summary = summarize_error_body(&body_bytes);
        let mut headers: Vec<(String, String)> = Vec::new();
        for (k, v) in resp_hdrs {
            let lower = k.to_ascii_lowercase();
            if HOP_BY_HOP.contains(&lower.as_str()) || lower == "content-length" {
                continue;
            }
            headers.push((k, v));
        }
        headers.push(("Content-Length".to_string(), body_bytes.len().to_string()));
        write_head(stream, status, &headers)?;
        stream.write_all(&body_bytes)?;
        stream.flush()?;
        if !summary.is_empty() {
            log::warn!(
                "上游 {status} route={} path={}：{summary}",
                route.name,
                req.target
            );
        }
    } else {
        let mut headers: Vec<(String, String)> = Vec::new();
        for (k, v) in resp_hdrs {
            let lower = k.to_ascii_lowercase();
            if HOP_BY_HOP.contains(&lower.as_str()) || lower == "content-length" {
                continue;
            }
            headers.push((k, v));
        }
        let chunked = length.is_none();
        if chunked {
            headers.push(("Transfer-Encoding".to_string(), "chunked".to_string()));
        } else {
            headers.push(("Content-Length".to_string(), length.unwrap().to_string()));
        }
        write_head(stream, status, &headers)?;
        let is_head = req.method.eq_ignore_ascii_case("HEAD");
        if !is_head {
            if let Err(e) = pump(resp.into_body().into_reader(), stream, chunked, &ctx.stats) {
                log::debug!("client disconnected while streaming {path}: {e}");
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    log::info!(
        "{} {} route={} originator={} status={} {:.0}ms bytes={}->{} orphan={} tool={} empty_msg={} empty_rs={} note={} drop={} normalize={} agent_msg={}",
        req.method,
        path,
        route.name,
        if originator.is_empty() { "(未标注)" } else { &originator },
        status,
        elapsed,
        req.body.len(),
        outgoing.len(),
        report.orphan_outputs,
        report.tool_rounds_repaired,
        report.empty_messages_dropped,
        report.empty_reasoning_dropped,
        report.web_search_noted,
        report.web_search_dropped,
        report.web_search_normalized,
        report.agent_messages_normalized,
    );
    if !report.notes.is_empty() {
        log::info!("route={} 备注：{}", route.name, report.notes.join("; "));
    }
    Ok(true)
}

/// 复制响应头为 (name, value) 列表（需要在消费 body 前完成）。
fn resp_headers(resp: &ureq::http::Response<ureq::Body>) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect()
}

/// 单连接服务循环：解析 → 处理 → 视保活决定是否继续。
fn serve_connection(mut stream: TcpStream, ctx: Arc<Ctx>) {
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    loop {
        match read_request(&mut reader) {
            Ok(Some(req)) => {
                let conn_close = req
                    .header("connection")
                    .is_some_and(|v| v.eq_ignore_ascii_case("close"));
                let keep_alive = !req.version10 && !conn_close;
                match handle_request(&mut stream, &req, &ctx) {
                    Ok(true) if keep_alive => continue,
                    _ => break,
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::debug!("客户端 {peer:?} 连接异常：{e}");
                break;
            }
        }
    }
}

/// 运行中的代理服务句柄。
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    stats: Arc<Stats>,
    keyring: KeyRing,
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    pub fn keyring(&self) -> KeyRing {
        self.keyring.clone()
    }

    /// 停止接受新连接并等待 accept 线程退出（在途请求由各自连接线程收尾）。
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
    }
}

/// 启动代理服务（非阻塞，accept 在后台线程）。
pub fn start(config: ResolvedConfig) -> io::Result<ServerHandle> {
    start_with_log_health(config, None)
}

/// 启动代理服务，并把文件日志健康状态并入 `/health`。
pub fn start_with_log_health(
    config: ResolvedConfig,
    log_health: Option<Arc<LogHealth>>,
) -> io::Result<ServerHandle> {
    let listener = TcpListener::bind(&config.listen)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let stats = Arc::new(Stats::new());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(None)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_body(Some(READ_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(ureq::config::AutoHeaderValue::None)
        .accept(ureq::config::AutoHeaderValue::None)
        .accept_encoding(ureq::config::AutoHeaderValue::None)
        .build()
        .into();
    let keyring: KeyRing = Arc::new(Mutex::new(HashMap::new()));
    let ctx = Arc::new(Ctx {
        config,
        stats: stats.clone(),
        agent,
        keyring: keyring.clone(),
        log_health,
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown2 = shutdown.clone();
    let accept_thread = std::thread::spawn(move || loop {
        if shutdown2.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let ctx = ctx.clone();
                std::thread::spawn(move || serve_connection(stream, ctx));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                log::warn!("accept 失败：{e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    });
    Ok(ServerHandle {
        addr,
        shutdown,
        accept_thread: Some(accept_thread),
        stats,
        keyring,
    })
}
