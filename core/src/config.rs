//! 代理配置：监听地址与路由表（前缀到真实上游）。
//!
//! 配置文件位置按平台约定：Windows 为 exe 同目录 `config.json`（便携），
//! macOS 为 `~/Library/Application Support/Halcyon/config.json`（.app 内不可写），
//! 其他平台为 `~/.config/halcyon/config.json`。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_NAME: &str = "config.json";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8788";
pub const DEFAULT_WEB_SEARCH_MODE: &str = "note";
pub const DEFAULT_WEB_SEARCH_NOTE_MAX: usize = 20;
pub const WEB_SEARCH_MODES: [&str; 3] = ["drop", "note", "normalize"];
/// 余额/额度查询覆盖配置（可选）：不写则按上游地址自动识别供应商。
/// `template`: deepseek / kimi / glm / custom / none（none 关闭该路由的查询）；
/// `url`: 查询地址覆盖（template=custom 时必填；其余场景不常用，供应商改地址时免发包）；
/// `custom`: template=custom 时的声明式解析规则（见 [`CustomSpec`]）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceCfg {
    pub url: Option<String>,
    pub template: Option<String>,
    pub custom: Option<CustomSpec>,
}

/// 用量查询声明式规则：从供应商响应 JSON 提取结构化档位行（QuotaRow）。
/// 规则只接触**响应体**，永远不接触 Authorization 头。
///
/// 字段路径语法：`a.b[0].c`（`.` 分段、`[n]` 数组下标）；
/// `|` 分隔多个候选路径（取第一个命中），如 `nextResetTime|resetTime`；
/// 行内路径以 `/` 开头表示从**响应根**取值（默认相对当前条目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomSpec {
    /// 行来源列表（合并取行；至少一个）。
    pub sources: Vec<RowSource>,
    /// pct 直读乘数（0-1 比例制填 100），默认 1。
    pub scale: Option<f64>,
    /// 档位名映射（原始值 → 显示名），作用于 label 解析结果。
    pub label_map: Option<BTreeMap<String, String>>,
    /// label 映射未命中时剥掉的前缀（如 "limit_"）。
    pub label_strip_prefix: Option<String>,
    /// detail 插值字段映射：占位路径 →（原始值 → 替换文本）。
    /// `"*"` 为通配项，替换文本里 `{raw}` 引用原值。
    pub field_map: Option<BTreeMap<String, BTreeMap<String, String>>>,
    /// 条件后缀：字段等于给定值时给 detail 追加后缀（如"（不可用）"）。
    pub flag_suffix: Vec<FlagSuffix>,
    /// 行排序：按首个命中的规则分组（稳定排序，未命中排最后）。
    pub sort: Vec<SortRule>,
}

/// 一个行来源：响应里的一个数组 / 映射 / 单对象。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RowSource {
    /// 行列表路径（相对响应根）；省略或 "." = 整个响应即一行；
    /// 指向对象且 rows_as != "map" 时视为单行。
    pub rows_path: Option<String>,
    /// "map"：目标对象是 键→条目 的映射（label 可用 "@key" 取键名）。
    pub rows_as: Option<String>,
    /// 行字段映射。
    pub row: RowSpec,
    /// 行过滤：字段值需在 one_of 列表中，否则丢弃该行。
    pub row_filter: Option<RowFilter>,
    /// map 模式下按条目键覆盖部分行字段（Some 字段生效）。
    pub row_overrides: Option<BTreeMap<String, RowSpec>>,
}

/// 行字段映射：值均为字段路径（label 另有两种特殊形态）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RowSpec {
    /// "@key"（map 条目键）/ "str:字面量" / 字段路径；缺省回退为条目键或 "额度"。
    pub label: Option<String>,
    /// 已用百分比直读路径（配合 scale）。
    pub pct: Option<String>,
    pub used: Option<String>,
    pub limit: Option<String>,
    pub remaining: Option<String>,
    /// 重置时间路径：epoch 毫秒（数字）或 ISO 字符串均可。
    pub reset: Option<String>,
    /// 插值模板，如 "¥{balance_infos[0].total_balance}"；
    /// `{path:默认}` 带缺省值；`{{` `}}` 转义字面花括号；
    /// 任一占位字段缺失且无缺省 → 整个 detail 丢弃（不字面输出）。
    pub detail: Option<String>,
}

/// 行过滤：path 字段的字符串值需在 one_of 中。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowFilter {
    pub path: String,
    pub one_of: Vec<String>,
}

/// 条件后缀：path 字段解析值 == equals 时给 detail 追加 suffix。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlagSuffix {
    pub path: String,
    pub equals: serde_json::Value,
    pub suffix: String,
}

/// 排序规则：四个匹配方式填一个（exact / prefix / suffix / contains）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SortRule {
    pub exact: Option<String>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub contains: Option<String>,
}

/// 一条路由：`/<name>/**`（以及可选别名 `aliases`）转发到 `upstream` 的同名相对路径。
///
/// 别名让本地地址可以**字面等价**于上游 base_url——例如给 kimi 配别名 `/kimi/v1`，
/// 则 `http://127.0.0.1:8788/kimi/v1` ≡ `https://api.kimi.com/coding/v1`，
/// 后面接什么路径都逐字对应，不做任何改写。
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub name: String,
    pub upstream: String,
    pub web_search: String,
    pub web_search_note_max: usize,
    pub aliases: Vec<String>,
    pub balance: Option<BalanceCfg>,
}

impl Route {
    pub fn prefix(&self) -> String {
        format!("/{}", self.name.trim_matches('/'))
    }

    pub fn all_prefixes(&self) -> Vec<String> {
        let mut v = vec![self.prefix()];
        v.extend(self.aliases.iter().cloned());
        v
    }
}

/// 路由表项的线上（JSON/TOML）形态：路由名是表的键。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteEntry {
    pub upstream: String,
    pub web_search: String,
    pub web_search_note_max: usize,
    pub aliases: Vec<String>,
    pub balance: Option<BalanceCfg>,
}

impl Default for RouteEntry {
    fn default() -> Self {
        Self {
            upstream: String::new(),
            web_search: DEFAULT_WEB_SEARCH_MODE.to_string(),
            web_search_note_max: DEFAULT_WEB_SEARCH_NOTE_MAX,
            aliases: Vec::new(),
            balance: None,
        }
    }
}

/// 配置文件（JSON 与旧 TOML 共用同一形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub log_level: String,
    pub log_file: String,
    pub routes: BTreeMap<String, RouteEntry>,
    pub default_route: Option<String>,
    pub orphan_header: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_string(),
            log_level: "info".to_string(),
            log_file: String::new(),
            routes: BTreeMap::new(),
            default_route: None,
            orphan_header: true,
        }
    }
}

/// 校验后的运行配置。
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub listen: String,
    pub log_level: String,
    pub log_file: String,
    pub routes: Vec<Route>,
    pub default_route: Option<String>,
    pub orphan_header: bool,
}

impl ResolvedConfig {
    /// 把请求路径拆成 (路由, 上游相对路径)；未命中返回 (None, 原路径)。
    /// 一条路由可有多个前缀（路由名 + 别名），**最长前缀优先**。
    pub fn route_for_path<'a>(&'a self, path: &str) -> (Option<&'a Route>, String) {
        let mut best: Option<(usize, &'a Route)> = None; // (最长前缀长度, 路由)
        for route in &self.routes {
            for prefix in route.all_prefixes() {
                let matched = path == prefix
                    || (path.len() > prefix.len()
                        && path.starts_with(&prefix)
                        && path.as_bytes()[prefix.len()] == b'/');
                if matched && best.is_none_or(|(len, _)| prefix.len() > len) {
                    best = Some((prefix.len(), route));
                }
            }
        }
        match best {
            None => (None, path.to_string()),
            Some((len, route)) => {
                let rest = &path[len..];
                (
                    Some(route),
                    if rest.is_empty() {
                        "/".to_string()
                    } else {
                        rest.to_string()
                    },
                )
            }
        }
    }

    /// 前缀匹配；未命中时回退到 `default_route`（或唯一路由），否则 (None, path)。
    pub fn resolve_route<'a>(&'a self, path: &str) -> (Option<&'a Route>, String) {
        let (route, rest) = self.route_for_path(path);
        if route.is_some() {
            return (route, rest);
        }
        let mut fallback = self.route_by_name(self.default_route.as_deref().unwrap_or(""));
        if fallback.is_none() && self.routes.len() == 1 {
            fallback = self.routes.first();
        }
        match fallback {
            None => (None, path.to_string()),
            Some(r) => (Some(r), path.to_string()),
        }
    }

    pub fn route_by_name(&self, name: &str) -> Option<&Route> {
        if name.is_empty() {
            return None;
        }
        self.routes.iter().find(|r| r.name == name)
    }
}

/// 配置不合法。
#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl TryFrom<Config> for ResolvedConfig {
    type Error = ConfigError;

    fn try_from(data: Config) -> Result<Self, ConfigError> {
        let mut routes = Vec::new();
        for (name, entry) in &data.routes {
            if entry.upstream.is_empty() {
                return Err(ConfigError(format!("路由 {name} 缺少 upstream")));
            }
            let upstream = entry.upstream.trim_end_matches('/').to_string();
            if !(upstream.starts_with("http://") || upstream.starts_with("https://")) {
                return Err(ConfigError(format!(
                    "路由 {name} 的 upstream 必须是 http(s) URL"
                )));
            }
            if !WEB_SEARCH_MODES.contains(&entry.web_search.as_str()) {
                return Err(ConfigError(format!(
                    "路由 {name} 的 web_search 只能是 {}",
                    WEB_SEARCH_MODES.join(" / ")
                )));
            }
            if let Some(b) = &entry.balance {
                if b.template.as_deref() == Some("custom") {
                    if b.url.as_deref().map(str::trim).unwrap_or("").is_empty() {
                        return Err(ConfigError(format!(
                            "路由 {name} 的 balance template=custom 需要 url"
                        )));
                    }
                    let spec = b.custom.as_ref().ok_or_else(|| {
                        ConfigError(format!(
                            "路由 {name} 的 balance template=custom 缺少 custom 规则"
                        ))
                    })?;
                    crate::balance::validate_custom(spec).map_err(|e| {
                        ConfigError(format!("路由 {name} 的 custom 规则不合法：{e}"))
                    })?;
                }
            }
            let aliases = entry
                .aliases
                .iter()
                .filter_map(|a| {
                    let t = a.trim_matches('/');
                    if t.is_empty() {
                        None
                    } else {
                        Some(format!("/{t}"))
                    }
                })
                .collect();
            routes.push(Route {
                name: name.clone(),
                upstream,
                web_search: entry.web_search.clone(),
                web_search_note_max: entry.web_search_note_max,
                aliases,
                balance: entry.balance.clone(),
            });
        }
        if !data.listen.contains(':') {
            return Err(ConfigError("listen 需形如 127.0.0.1:8788".to_string()));
        }
        Ok(ResolvedConfig {
            listen: data.listen,
            log_level: data.log_level,
            log_file: data.log_file,
            routes,
            default_route: data.default_route.filter(|s| !s.is_empty()),
            orphan_header: data.orphan_header,
        })
    }
}

/// 读取 JSON 配置。
pub fn load_json(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read(path)
        .map_err(|e| ConfigError(format!("读取配置失败：{}: {e}", path.display())))?;
    serde_json::from_slice(&raw)
        .map_err(|e| ConfigError(format!("配置文件不是合法 JSON：{}: {e}", path.display())))
}

/// 配置文件路径（按平台约定，见模块注释）。
pub fn default_config_path() -> PathBuf {
    crate::data_dir::resolve(
        crate::data_dir::parse_cli_data_dir(&std::env::args().collect::<Vec<_>>()).as_deref(),
    )
    .path
    .join(DEFAULT_CONFIG_NAME)
}

/// 默认配置查找：只读 [`default_config_path`]（平台约定位置）。
/// 返回 (配置, 来源描述)。没有则给内置默认（空路由表）。
pub fn load_default() -> (Config, String) {
    let path = default_config_path();
    if path.exists() {
        match load_json(&path) {
            Ok(c) => return (c, path.display().to_string()),
            Err(e) => log::warn!("{e}，使用内置默认配置"),
        }
    }
    (Config::default(), "内置默认（未找到配置文件）".to_string())
}
