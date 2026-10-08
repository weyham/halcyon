//! 余额/额度查询：按上游地址识别供应商，构造查询 URL，解析响应为结构化档位行。
//!
//! 供应商语义不同：DeepSeek 是余额制（¥），Kimi coding 与 GLM 是套餐额度制
//! （5 小时滚动窗口 / 周 / 月等多档，各带重置时间）。统一解析为 [`QuotaRow`] 列表，
//! 托盘菜单显示最紧一档 + 子菜单看全部，设置面板看完整表格。
//!
//! 解析走**统一声明式规则引擎**（[`parse_custom`]）：内置三家供应商模板只是内置的
//! [`CustomSpec`] 等价物，与用户配置的 `template = "custom"` 走同一条解析路径，
//! 不存在两套逻辑。路由配置 `balance = { url, template, custom }`：
//! template: deepseek / kimi / glm / custom / none；不写则按上游地址自动识别——
//! 换 URL、新增同形状供应商、接入全新形状供应商都只需要改配置，不用发包。
//!
//! 调研证据：
//! - DeepSeek：官方文档 `GET /user/balance`（无 key 探测 401）
//! - Kimi：`GET {base}/usages`（无 key 时上游返回 401；响应形状见下方样例）
//! - GLM：`GET {origin}/api/monitor/usage/quota/limit`（官方插件 zai-coding-plugins 脚本证实）

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::config::{CustomSpec, FlagSuffix, Route, RowSource, RowSpec, SortRule};

/// 解析器模板（= 供应商响应形状）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    DeepSeek,
    Kimi,
    Glm,
    Unknown,
}

/// 一档用量/余额，菜单与面板共用的最小展示单元。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaRow {
    /// 档位名：如 "5h"、"周额度"、"余额"
    pub label: String,
    /// 已用百分比（0-100）；余额制为 None
    pub pct: Option<i64>,
    /// 重置时间（已格式化）；无则 None
    pub reset: Option<String>,
    /// 原始重置时间的 Unix 毫秒时间戳；用于托盘显示剩余时间。
    pub reset_at_ms: Option<i64>,
    /// 额外文本：如 "¥110.00"、"45/100"
    pub detail: Option<String>,
}

impl QuotaRow {
    /// 菜单行文本：`周额度 16% · 09-24 03:29 重置` 或 `余额 ¥110.00`
    pub fn menu_text(&self) -> String {
        let mut text = match (&self.pct, &self.detail) {
            (Some(p), _) => format!("{} {p}%", self.label),
            (None, Some(d)) => format!("{} {d}", self.label),
            (None, None) => self.label.clone(),
        };
        if let Some(r) = &self.reset {
            text = format!("{text} · {r} 重置");
        }
        text
    }
}

/// 按上游 URL 的 host 识别供应商。
pub fn detect(upstream: &str) -> Provider {
    let host = upstream
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host == "api.deepseek.com" {
        Provider::DeepSeek
    } else if host == "api.kimi.com" || host == "api.moonshot.cn" {
        Provider::Kimi
    } else if host == "open.bigmodel.cn" {
        Provider::Glm
    } else {
        Provider::Unknown
    }
}

fn origin_of(upstream: &str) -> String {
    let rest = upstream.split("://").nth(1).unwrap_or("");
    let host = rest.split('/').next().unwrap_or("");
    let scheme = upstream.split("://").next().unwrap_or("https");
    format!("{scheme}://{host}")
}

/// 内置查询 URL。
fn builtin_url(provider: Provider, upstream: &str) -> Option<String> {
    match provider {
        Provider::DeepSeek => Some(format!("{}/user/balance", origin_of(upstream))),
        Provider::Kimi => Some(format!("{}/usages", upstream.trim_end_matches('/'))),
        Provider::Glm => Some(format!(
            "{}/api/monitor/usage/quota/limit",
            origin_of(upstream)
        )),
        Provider::Unknown => None,
    }
}

/// 一个路由最终生效的查询：供应商标识 + URL + 解析规则。
#[derive(Debug, Clone)]
pub struct ResolvedQuery {
    pub provider: Provider,
    pub url: String,
    pub spec: CustomSpec,
}

/// 解析路由的查询配置：配置覆盖优先，缺省按上游自动识别；`template="none"` 关闭。
/// `template="custom"` 需要同时给出 `custom` 规则与 `url`（配置加载时已校验，这里防御性返回 None）。
pub fn resolve_query(route: &Route) -> Option<ResolvedQuery> {
    let cfg = route.balance.as_ref();
    if cfg.and_then(|c| c.template.as_deref()) == Some("none") {
        return None;
    }
    if cfg.and_then(|c| c.template.as_deref()) == Some("custom") {
        let spec = cfg.and_then(|c| c.custom.clone())?;
        let url = cfg.and_then(|c| c.url.clone())?;
        return Some(ResolvedQuery {
            provider: Provider::Unknown,
            url,
            spec,
        });
    }
    let provider = match cfg.and_then(|c| c.template.as_deref()) {
        Some("deepseek") => Provider::DeepSeek,
        Some("kimi") => Provider::Kimi,
        Some("glm") => Provider::Glm,
        Some(_) => Provider::Unknown,
        None => detect(&route.upstream),
    };
    if provider == Provider::Unknown {
        return None;
    }
    let url = cfg
        .and_then(|c| c.url.clone())
        .or_else(|| builtin_url(provider, &route.upstream))?;
    Some(ResolvedQuery {
        provider,
        url,
        spec: builtin_spec(provider),
    })
}

/// GET JSON（10s 超时，4xx/5xx 不当异常）；Authorization 头原样带上（仅此用途，绝不记录）。
pub fn fetch_json(url: &str, auth: &str) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let req = ureq::http::Request::builder()
        .method("GET")
        .uri(url)
        .header("Authorization", auth)
        .header("Accept", "application/json")
        .body(())
        .map_err(|e| e.to_string())?;
    let mut resp = agent.run(req).map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let mut text = String::new();
    use std::io::Read;
    resp.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("JSON 解析失败：{e}"))
}

/// 数字字段：接受 JSON 数字或数字字符串（Kimi 的额度字段是字符串型）。
fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn pct_of(used: Option<f64>, limit: Option<f64>, remaining: Option<f64>) -> Option<i64> {
    match (used, limit, remaining) {
        (Some(u), Some(l), _) if l > 0.0 => Some((u / l * 100.0).round() as i64),
        (_, Some(l), Some(r)) if l > 0.0 => Some(((l - r) / l * 100.0).round() as i64),
        _ => None,
    }
}

fn reset_parts(v: Option<&Value>) -> (Option<String>, Option<i64>) {
    match v {
        Some(Value::Number(n)) => {
            let ts = n.as_i64();
            let text = ts
                .and_then(|value| jiff::Timestamp::from_millisecond(value).ok())
                .map(|t| {
                    t.to_zoned(jiff::tz::TimeZone::system())
                        .strftime("%m-%d %H:%M")
                        .to_string()
                });
            (text, ts)
        }
        Some(Value::String(s)) => {
            let text = Some(s.chars().take(16).collect::<String>().replace('T', " "));
            let ts = s
                .parse::<jiff::Timestamp>()
                .ok()
                .map(|t| t.as_millisecond());
            (text, ts)
        }
        _ => (None, None),
    }
}

fn fmt_num(n: f64) -> String {
    if n >= 1000.0 {
        format!("{}k", (n / 100.0).round() / 10.0)
    } else {
        format!("{}", n as i64)
    }
}

// ---------- 字段路径求值 ----------

#[derive(Debug, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}

/// 解析 `a.b[0].c` 为段序列。
fn parse_path(path: &str) -> Result<Vec<Seg>, String> {
    let mut segs = Vec::new();
    for part in path.split('.') {
        if part.is_empty() {
            return Err(format!("路径存在空段：{path}"));
        }
        let key_end = part.find('[').unwrap_or(part.len());
        let key = &part[..key_end];
        if !key.is_empty() {
            segs.push(Seg::Key(key.to_string()));
        }
        let mut rest = &part[key_end..];
        while rest.starts_with('[') {
            let end = rest
                .find(']')
                .ok_or_else(|| format!("路径下标未闭合：{path}"))?;
            let n: usize = rest[1..end]
                .parse()
                .map_err(|_| format!("路径下标不是非负整数：{path}"))?;
            segs.push(Seg::Index(n));
            rest = &rest[end + 1..];
        }
        if !rest.is_empty() {
            return Err(format!("路径段无法解析：{path}"));
        }
    }
    Ok(segs)
}

fn eval_segs<'a>(mut cur: &'a Value, segs: &[Seg]) -> Option<&'a Value> {
    for seg in segs {
        cur = match seg {
            Seg::Key(k) => cur.get(k)?,
            Seg::Index(i) => cur.get(i)?,
        };
    }
    Some(cur)
}

/// 求值字段路径：`|` 分隔候选（取第一个命中）；`/` 开头从响应根取值（默认相对当前条目）；
/// 空串 / "." 指条目自身。路径语法错误或未命中一律返回 None（退化为字段缺失）。
fn eval_path<'a>(root: &'a Value, entry: &'a Value, path: &str) -> Option<&'a Value> {
    for cand in path.split('|') {
        let cand = cand.trim();
        if cand.is_empty() {
            continue;
        }
        let (base, p) = match cand.strip_prefix('/') {
            Some(p) => (root, p),
            None => (entry, cand),
        };
        if p.is_empty() || p == "." {
            return Some(base);
        }
        let segs = parse_path(p).ok()?;
        if let Some(v) = eval_segs(base, &segs) {
            return Some(v);
        }
    }
    None
}

/// 值转文本：字符串原样；数字走 fmt_num（≥1000 缩写 k）；布尔转字面；其余视为缺失。
fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(fmt_num(n.as_f64()?)),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ---------- detail 插值 ----------

/// 渲染 detail 模板：`{path}` / `{path:默认}` / `{{` `}}` 转义。
/// 任一占位字段缺失且无缺省 → 返回 None（整个 detail 丢弃，不字面输出）。
/// field_map 按占位路径做值替换；`"*"` 通配项里 `{raw}` 引用原值。
fn render_detail(
    template: &str,
    root: &Value,
    entry: &Value,
    field_map: &Option<BTreeMap<String, BTreeMap<String, String>>>,
) -> Option<String> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut inner = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(c2);
                }
                if !closed {
                    return None;
                }
                let (path, default) = match inner.split_once(':') {
                    Some((p, d)) => (p, Some(d)),
                    None => (inner.as_str(), None),
                };
                let resolved = eval_path(root, entry, path)
                    .and_then(value_text)
                    .or_else(|| default.map(str::to_string))?;
                let mapped = match field_map.as_ref().and_then(|m| m.get(path)) {
                    Some(map) => match map.get(&resolved) {
                        Some(r) => r.clone(),
                        None => match map.get("*") {
                            Some(w) => w.replace("{raw}", &resolved),
                            None => resolved,
                        },
                    },
                    None => resolved,
                };
                out.push_str(&mapped);
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

// ---------- 声明式规则引擎 ----------

/// 校验 custom 规则（配置加载与保存时调用；运行期 parse_custom 也会防御性再校验一次）。
pub fn validate_custom(spec: &CustomSpec) -> Result<(), String> {
    if spec.sources.is_empty() {
        return Err("sources 至少需要一个行来源".to_string());
    }
    for src in &spec.sources {
        if let Some(p) = src.rows_path.as_deref() {
            if !p.is_empty() && p != "." {
                parse_path(p.trim_start_matches('/')).map_err(|e| format!("rows_path {e}"))?;
            }
        }
        let is_map = src.rows_as.as_deref() == Some("map");
        validate_row(&src.row, is_map)?;
        if let Some(f) = &src.row_filter {
            if f.path.is_empty() || f.one_of.is_empty() {
                return Err("row_filter 需要 path 与非空 one_of".to_string());
            }
        }
        if let Some(o) = &src.row_overrides {
            for r in o.values() {
                validate_row(r, true)?;
            }
        }
    }
    for fs in &spec.flag_suffix {
        if fs.path.is_empty() {
            return Err("flag_suffix 需要 path".to_string());
        }
    }
    for (i, r) in spec.sort.iter().enumerate() {
        if r.exact.is_none() && r.prefix.is_none() && r.suffix.is_none() && r.contains.is_none() {
            return Err(format!(
                "sort[{i}] 需要填 exact/prefix/suffix/contains 之一"
            ));
        }
    }
    if let Some(s) = spec.scale {
        if !(s.is_finite() && s > 0.0) {
            return Err("scale 必须是正数".to_string());
        }
    }
    Ok(())
}

fn validate_row(row: &RowSpec, is_map: bool) -> Result<(), String> {
    if row.label.as_deref() == Some("@key") && !is_map {
        return Err("label \"@key\" 仅 map 模式可用".to_string());
    }
    if let Some(d) = &row.detail {
        validate_template(d)?;
    }
    Ok(())
}

fn validate_template(t: &str) -> Result<(), String> {
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            '{' => {
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(format!("detail 模板花括号未闭合：{t}"));
                }
            }
            '}' => return Err(format!("detail 模板存在孤立 }}：{t}")),
            _ => {}
        }
    }
    Ok(())
}

fn sort_rule_matches(r: &SortRule, label: &str) -> bool {
    r.exact.as_deref() == Some(label)
        || r.prefix.as_deref().is_some_and(|p| label.starts_with(p))
        || r.suffix.as_deref().is_some_and(|s| label.ends_with(s))
        || r.contains.as_deref().is_some_and(|c| label.contains(c))
}

fn resolve_label(
    spec: &CustomSpec,
    row: &RowSpec,
    key: Option<&str>,
    root: &Value,
    entry: &Value,
) -> String {
    let raw = match row.label.as_deref() {
        Some("@key") => key.unwrap_or("额度").to_string(),
        Some(l) if l.starts_with("str:") => l[4..].to_string(),
        Some(path) => eval_path(root, entry, path)
            .and_then(value_text)
            .unwrap_or_else(|| key.unwrap_or("额度").to_string()),
        None => key.unwrap_or("额度").to_string(),
    };
    if let Some(m) = &spec.label_map {
        if let Some(mapped) = m.get(&raw) {
            return mapped.clone();
        }
    }
    match &spec.label_strip_prefix {
        Some(p) => raw.strip_prefix(p.as_str()).unwrap_or(&raw).to_string(),
        None => raw,
    }
}

fn build_row(
    spec: &CustomSpec,
    row: &RowSpec,
    key: Option<&str>,
    root: &Value,
    entry: &Value,
) -> Option<QuotaRow> {
    let scale = spec.scale.unwrap_or(1.0);
    let get = |p: &Option<String>| p.as_deref().and_then(|p| eval_path(root, entry, p));
    let used = num(get(&row.used));
    let limit = num(get(&row.limit));
    let remaining = num(get(&row.remaining));
    // pct 优先级：pct 直读（×scale 后四舍五入）→ used+limit → remaining+limit
    let pct = match num(get(&row.pct)) {
        Some(p) => Some((p * scale).round() as i64),
        None => pct_of(used, limit, remaining),
    };
    let (reset, reset_at_ms) = reset_parts(get(&row.reset));
    let mut detail = row
        .detail
        .as_deref()
        .and_then(|t| render_detail(t, root, entry, &spec.field_map));
    for fs in &spec.flag_suffix {
        let hit = eval_path(root, entry, &fs.path)
            .map(|v| *v == fs.equals)
            .unwrap_or(false);
        if hit {
            detail = Some(match detail {
                Some(d) => format!("{d}{}", fs.suffix),
                None => fs.suffix.clone(),
            });
        }
    }
    if pct.is_none() && detail.is_none() {
        return None;
    }
    Some(QuotaRow {
        label: resolve_label(spec, row, key, root, entry),
        pct,
        reset,
        reset_at_ms,
        detail,
    })
}

fn merge_row(base: &RowSpec, ov: &RowSpec) -> RowSpec {
    RowSpec {
        label: ov.label.clone().or_else(|| base.label.clone()),
        pct: ov.pct.clone().or_else(|| base.pct.clone()),
        used: ov.used.clone().or_else(|| base.used.clone()),
        limit: ov.limit.clone().or_else(|| base.limit.clone()),
        remaining: ov.remaining.clone().or_else(|| base.remaining.clone()),
        reset: ov.reset.clone().or_else(|| base.reset.clone()),
        detail: ov.detail.clone().or_else(|| base.detail.clone()),
    }
}

fn collect_source(spec: &CustomSpec, src: &RowSource, root: &Value) -> Vec<QuotaRow> {
    let target: &Value = match src.rows_path.as_deref() {
        None | Some("") | Some(".") => root,
        Some(p) => match eval_path(root, root, p) {
            Some(v) => v,
            None => return Vec::new(),
        },
    };
    let entries: Vec<(Option<&str>, &Value)> = if src.rows_as.as_deref() == Some("map") {
        target
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (Some(k.as_str()), v)).collect())
            .unwrap_or_default()
    } else if let Some(arr) = target.as_array() {
        arr.iter().map(|v| (None, v)).collect()
    } else if target.is_object() {
        vec![(None, target)]
    } else {
        Vec::new()
    };
    let mut rows = Vec::new();
    for (key, entry) in entries {
        if let Some(f) = &src.row_filter {
            let v = eval_path(root, entry, &f.path)
                .and_then(value_text)
                .unwrap_or_default();
            if !f.one_of.iter().any(|x| x == &v) {
                continue;
            }
        }
        let merged = match key.and_then(|k| src.row_overrides.as_ref().and_then(|o| o.get(k))) {
            Some(ov) => merge_row(&src.row, ov),
            None => src.row.clone(),
        };
        if let Some(r) = build_row(spec, &merged, key, root, entry) {
            rows.push(r);
        }
    }
    rows
}

/// 统一解析入口：按声明式规则把供应商响应解析为档位行列表。
/// 提取不到任何行 → Err（面板显示"解析失败"，原始响应仍可见，不崩溃、不显假零）。
pub fn parse_custom(spec: &CustomSpec, body: &Value) -> Result<Vec<QuotaRow>, String> {
    validate_custom(spec)?;
    let mut rows = Vec::new();
    for src in &spec.sources {
        rows.extend(collect_source(spec, src, body));
    }
    if rows.is_empty() {
        let keys = body
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        return Err(format!("规则未提取到任何档位行（响应顶层键：{keys}）"));
    }
    if !spec.sort.is_empty() {
        let rank = |label: &str| {
            spec.sort
                .iter()
                .position(|r| sort_rule_matches(r, label))
                .unwrap_or(spec.sort.len())
        };
        let mut indexed: Vec<(usize, QuotaRow)> =
            rows.into_iter().map(|r| (rank(&r.label), r)).collect();
        indexed.sort_by_key(|(i, _)| *i); // 稳定排序：同组保持响应内顺序
        rows = indexed.into_iter().map(|(_, r)| r).collect();
    }
    Ok(rows)
}

// ---------- 内置供应商模板（= 内置 CustomSpec，与用户规则走同一解析路径） ----------

fn map_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 内置供应商模板的声明式等价物。
///
/// DeepSeek：单行余额（currency 经 field_map 转符号；is_available=false 追加"（不可用）"）。
/// Kimi：usages 键→窗口映射（label 取键名，used_ratio ×100；周档绝对值取根 usage，
/// 其余档取 limits[0]（单窗口时成立）；缺 limits/usage 时 detail 自动丢弃，pct 仍在）。
/// GLM：limits 数组过滤两种类型，percentage 直读。
pub fn builtin_spec(provider: Provider) -> CustomSpec {
    match provider {
        Provider::DeepSeek => CustomSpec {
            sources: vec![RowSource {
                rows_path: Some("balance_infos[0]".to_string()),
                row: RowSpec {
                    label: Some("str:余额".to_string()),
                    detail: Some("{currency:}{total_balance}".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }],
            field_map: Some(BTreeMap::from([(
                "currency".to_string(),
                map_of(&[("", ""), ("CNY", "¥"), ("USD", "$"), ("*", "{raw} ")]),
            )])),
            flag_suffix: vec![FlagSuffix {
                path: "/is_available".to_string(),
                equals: json!(false),
                suffix: "（不可用）".to_string(),
            }],
            ..Default::default()
        },
        Provider::Kimi => CustomSpec {
            sources: vec![RowSource {
                rows_path: Some("usages".to_string()),
                rows_as: Some("map".to_string()),
                row: RowSpec {
                    label: Some("@key".to_string()),
                    pct: Some("used_ratio".to_string()),
                    reset: Some("reset_time".to_string()),
                    detail: Some("{/limits[0].detail.used}/{/limits[0].detail.limit}".to_string()),
                    ..Default::default()
                },
                row_overrides: Some(BTreeMap::from([(
                    "limit_7d".to_string(),
                    RowSpec {
                        detail: Some("{/usage.used}/{/usage.limit}".to_string()),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            }],
            scale: Some(100.0),
            label_map: Some(map_of(&[
                ("limit_5h", "5h"),
                ("limit_7d", "周"),
                ("limit_30d", "月"),
                ("limit_month", "月"),
                ("limit_1m", "月"),
            ])),
            label_strip_prefix: Some("limit_".to_string()),
            sort: vec![
                SortRule {
                    suffix: Some("h".to_string()),
                    ..Default::default()
                },
                SortRule {
                    contains: Some("周".to_string()),
                    ..Default::default()
                },
                SortRule {
                    contains: Some("月".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
        Provider::Glm => CustomSpec {
            sources: vec![
                // 官方窗口语义（zai-coding-plugins / CodexBar 交叉验证）：
                // unit=3 且 number=5 → 5 小时；unit=6 且 number=1 → 周；
                // TIME_LIMIT(unit=5,number=1) → MCP 月度工具额度。
                // 实际响应为 { code, data: { limits } }，不是根层级 limits。
                RowSource {
                    rows_path: Some("data.limits".to_string()),
                    row: RowSpec {
                        label: Some("str:5h 额度".to_string()),
                        pct: Some("percentage".to_string()),
                        reset: Some("nextResetTime|resetTime".to_string()),
                        ..Default::default()
                    },
                    row_filter: Some(crate::config::RowFilter {
                        path: "unit".to_string(),
                        one_of: vec!["3".to_string()],
                    }),
                    ..Default::default()
                },
                RowSource {
                    rows_path: Some("data.limits".to_string()),
                    row: RowSpec {
                        label: Some("str:周额度".to_string()),
                        pct: Some("percentage".to_string()),
                        reset: Some("nextResetTime|resetTime".to_string()),
                        ..Default::default()
                    },
                    row_filter: Some(crate::config::RowFilter {
                        path: "unit".to_string(),
                        one_of: vec!["6".to_string()],
                    }),
                    ..Default::default()
                },
                RowSource {
                    rows_path: Some("data.limits".to_string()),
                    row: RowSpec {
                        label: Some("str:MCP 月额度".to_string()),
                        pct: Some("percentage".to_string()),
                        reset: Some("nextResetTime|resetTime".to_string()),
                        detail: Some("{currentValue}/{usage}".to_string()),
                        ..Default::default()
                    },
                    row_filter: Some(crate::config::RowFilter {
                        path: "type".to_string(),
                        one_of: vec!["TIME_LIMIT".to_string()],
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
        Provider::Unknown => CustomSpec::default(),
    }
}

/// 按内置供应商模板解析响应（= 内置 spec 走统一引擎）。
pub fn parse(provider: Provider, body: &Value) -> Result<Vec<QuotaRow>, String> {
    parse_custom(&builtin_spec(provider), body)
}

/// 摘要行：用量百分比最高的一档（最紧的窗口）；余额制取唯一行。
pub fn summary_row(rows: &[QuotaRow]) -> Option<&QuotaRow> {
    rows.iter().max_by_key(|r| r.pct.unwrap_or(-1))
}

/// 托盘菜单的档位标签：周/月在托盘显示层压缩为 7d/30d，其余原样（5h、余额、自定义）。
pub fn tray_label(label: &str) -> String {
    match label {
        "周" => "7d".to_string(),
        "月" => "30d".to_string(),
        other => other.to_string(),
    }
}

/// 紧凑小写剩余时间：`40m` / `1h40m` / `1d1h40m`；零高位省略，不足 1 分钟按 1m 计。
pub fn format_remaining_compact(ms: i64) -> String {
    let total_minutes = (ms / 60_000).max(1);
    let days = total_minutes / 1440;
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;
    let mut text = String::new();
    if days > 0 {
        text.push_str(&format!("{days}d"));
    }
    if hours > 0 {
        text.push_str(&format!("{hours}h"));
    }
    if minutes > 0 || text.is_empty() {
        text.push_str(&format!("{minutes}m"));
    }
    text
}

/// 数据年龄文案：`刚刚` / `3 分钟前` / `2 小时前` / `1 天前`。
/// 用于托盘用量区，提醒用户看到的是快照而不是现值。未来时间按「刚刚」处理。
pub fn fetched_age_text(now_ms: i64, fetched_at_ms: i64) -> String {
    let minutes = (now_ms - fetched_at_ms).max(0) / 60_000;
    match minutes {
        0 => "刚刚".to_string(),
        1..=59 => format!("{minutes} 分钟前"),
        _ => {
            let hours = minutes / 60;
            if hours < 24 {
                format!("{hours} 小时前")
            } else {
                format!("{} 天前", hours / 24)
            }
        }
    }
}

/// 托盘菜单单行：套餐档 `5h: 30% ⌛1h40m`；余额 `余额: ¥12.34`；只有标签时原样。
/// `now_ms` 显式传入以便测试；已过期的重置时间不显示。
pub fn tray_row_text(row: &QuotaRow, now_ms: i64) -> String {
    let label = tray_label(&row.label);
    let mut text = match (&row.pct, &row.detail) {
        (Some(pct), _) => format!("{label}: {pct}%"),
        (None, Some(detail)) => format!("{label}: {detail}"),
        (None, None) => label,
    };
    if let Some(reset_at) = row.reset_at_ms {
        if reset_at > now_ms {
            text.push_str(&format!(
                " ⌛{}",
                format_remaining_compact(reset_at - now_ms)
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests;
