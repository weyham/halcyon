//! config.rs 测试：最长前缀、别名等价、回退、校验、JSON/TOML 解析。对照旧 Python config.py 行为。

use halcyon_core::config::*;

fn cfg_with(routes: &[(&str, &str, &[&str])]) -> ResolvedConfig {
    let mut map = std::collections::BTreeMap::new();
    for (name, upstream, aliases) in routes {
        map.insert(
            name.to_string(),
            RouteEntry {
                upstream: upstream.to_string(),
                aliases: aliases.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        );
    }
    ResolvedConfig::try_from(Config {
        routes: map,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn plain_prefix_strips_route_name() {
    let c = cfg_with(&[("kimi", "https://api.kimi.com/coding/v1", &[])]);
    let (route, rest) = c.route_for_path("/kimi/responses");
    assert_eq!(route.unwrap().name, "kimi");
    assert_eq!(rest, "/responses");
}

#[test]
fn alias_is_literal_equivalent_and_longest_wins() {
    let c = cfg_with(&[("kimi", "https://api.kimi.com/coding/v1", &["/kimi/v1"])]);
    // 别名：/kimi/v1/models → /models（与直连上游 base_url + /models 字面等价）
    let (route, rest) = c.route_for_path("/kimi/v1/models");
    assert_eq!(route.unwrap().name, "kimi");
    assert_eq!(rest, "/models");
    // 裸前缀仍然可用，且不受别名影响
    let (_r, rest) = c.route_for_path("/kimi/responses");
    assert_eq!(rest, "/responses");
}

#[test]
fn prefix_must_match_path_boundary() {
    let c = cfg_with(&[("kimi", "https://api.kimi.com/coding/v1", &[])]);
    let (route, _rest) = c.route_for_path("/kimiya/responses");
    assert!(route.is_none(), "/kimiya 不应命中 /kimi 前缀");
}

#[test]
fn resolve_falls_back_to_single_route() {
    let c = cfg_with(&[("ds", "https://api.deepseek.com", &[])]);
    let (route, rest) = c.resolve_route("/anything/here");
    assert_eq!(route.unwrap().name, "ds");
    assert_eq!(rest, "/anything/here");
}

#[test]
fn resolve_multi_route_no_match_returns_none() {
    let c = cfg_with(&[
        ("ds", "https://api.deepseek.com", &[]),
        ("kimi", "https://api.kimi.com/coding/v1", &[]),
    ]);
    let (route, _rest) = c.resolve_route("/other/x");
    assert!(route.is_none());
}

#[test]
fn resolve_default_route_fallback() {
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        "a".to_string(),
        RouteEntry {
            upstream: "https://a.example.com".to_string(),
            ..Default::default()
        },
    );
    map.insert(
        "b".to_string(),
        RouteEntry {
            upstream: "https://b.example.com".to_string(),
            ..Default::default()
        },
    );
    let c = ResolvedConfig::try_from(Config {
        routes: map,
        default_route: Some("b".to_string()),
        ..Default::default()
    })
    .unwrap();
    let (route, _rest) = c.resolve_route("/unmatched");
    assert_eq!(route.unwrap().name, "b");
}

#[test]
fn validation_rejects_bad_values() {
    let bad_upstream = cfg_result(&[("x", "ftp://nope", &[])]);
    assert!(bad_upstream.is_err(), "非 http(s) 上游必须拒绝");

    let mut map = std::collections::BTreeMap::new();
    map.insert(
        "x".to_string(),
        RouteEntry {
            upstream: "https://ok.example.com".to_string(),
            web_search: "bogus".to_string(),
            ..Default::default()
        },
    );
    assert!(ResolvedConfig::try_from(Config {
        routes: map,
        ..Default::default()
    })
    .is_err());

    let mut map2 = std::collections::BTreeMap::new();
    map2.insert(
        "x".to_string(),
        RouteEntry {
            upstream: "https://ok.example.com".to_string(),
            ..Default::default()
        },
    );
    assert!(ResolvedConfig::try_from(Config {
        listen: "no-colon".to_string(),
        routes: map2,
        ..Default::default()
    })
    .is_err());
}

fn cfg_result(routes: &[(&str, &str, &[&str])]) -> Result<ResolvedConfig, ConfigError> {
    let mut map = std::collections::BTreeMap::new();
    for (name, upstream, aliases) in routes {
        map.insert(
            name.to_string(),
            RouteEntry {
                upstream: upstream.to_string(),
                aliases: aliases.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        );
    }
    ResolvedConfig::try_from(Config {
        routes: map,
        ..Default::default()
    })
}

#[test]
fn default_config_path_follows_platform_convention() {
    let path = halcyon_core::config::default_config_path();
    let text = path.to_string_lossy().replace('\\', "/");
    #[cfg(target_os = "macos")]
    assert!(
        text.ends_with("Library/Application Support/Halcyon/config.json"),
        "{text}"
    );
    #[cfg(target_os = "windows")]
    assert!(text.ends_with("config.json"), "{text}");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    assert!(text.ends_with(".config/halcyon/config.json"), "{text}");
}

#[test]
fn json_config_roundtrip() {
    let dir = std::env::temp_dir().join(format!("crs-cfg-json-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{
  "listen": "127.0.0.1:8788",
  "routes": {
    "zhipu": { "upstream": "https://open.bigmodel.cn/api/v1", "aliases": ["/zhipu/v1"] }
  }
}"#,
    )
    .unwrap();
    let raw = load_json(&path).unwrap();
    let c = ResolvedConfig::try_from(raw).unwrap();
    assert_eq!(c.routes.len(), 1);
    assert_eq!(c.route_by_name("zhipu").unwrap().aliases, vec!["/zhipu/v1"]);
    // 未填字段取默认
    assert_eq!(c.log_level, "info");
    assert!(c.orphan_header);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn balance_custom_validation() {
    use halcyon_core::config::{BalanceCfg, CustomSpec, RowSource, RowSpec};
    let mk = |balance: BalanceCfg| {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "x".to_string(),
            RouteEntry {
                upstream: "https://newapi.example.com/v1".to_string(),
                balance: Some(balance),
                ..Default::default()
            },
        );
        ResolvedConfig::try_from(Config {
            routes: map,
            ..Default::default()
        })
    };
    let good_spec = || CustomSpec {
        sources: vec![RowSource {
            rows_path: Some("data".to_string()),
            row: RowSpec {
                pct: Some("p".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    // template=custom 缺 url → 拒绝
    let r = mk(BalanceCfg {
        template: Some("custom".to_string()),
        custom: Some(good_spec()),
        ..Default::default()
    });
    assert!(r.is_err(), "缺 url 应拒绝");
    // template=custom 缺规则 → 拒绝
    let r = mk(BalanceCfg {
        template: Some("custom".to_string()),
        url: Some("https://newapi.example.com/q".to_string()),
        ..Default::default()
    });
    assert!(r.is_err(), "缺 custom 规则应拒绝");
    // 规则不合法（空 sources）→ 拒绝
    let r = mk(BalanceCfg {
        template: Some("custom".to_string()),
        url: Some("https://newapi.example.com/q".to_string()),
        custom: Some(CustomSpec::default()),
    });
    assert!(r.is_err(), "空 sources 应拒绝");
    // 合法 → 通过
    let r = mk(BalanceCfg {
        template: Some("custom".to_string()),
        url: Some("https://newapi.example.com/q".to_string()),
        custom: Some(good_spec()),
    });
    assert!(r.is_ok(), "合法 custom 应通过：{:?}", r.err());
    // JSON 往返：custom 规则序列化/反序列化保持
    let entry = RouteEntry {
        upstream: "https://newapi.example.com/v1".to_string(),
        balance: Some(BalanceCfg {
            template: Some("custom".to_string()),
            url: Some("https://newapi.example.com/q".to_string()),
            custom: Some(good_spec()),
        }),
        ..Default::default()
    };
    let text = serde_json::to_string(&entry).unwrap();
    let back: RouteEntry = serde_json::from_str(&text).unwrap();
    assert_eq!(back.balance.unwrap().custom.unwrap().sources.len(), 1);
}

#[test]
fn repo_config_example_is_valid() {
    // 仓库根目录 config.example.json 必须始终通过加载与校验（含 custom 示例路由）；
    // _comment 等注释键应被 serde 忽略，不影响解析。
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.example.json");
    let raw = load_json(&path).unwrap_or_else(|e| panic!("示例配置不是合法 JSON：{e}"));
    assert!(
        raw.routes.contains_key("newapi"),
        "示例应保留 custom 样例路由"
    );
    let resolved =
        ResolvedConfig::try_from(raw).unwrap_or_else(|e| panic!("示例配置校验失败：{e}"));
    let newapi = resolved.route_by_name("newapi").unwrap();
    let q = halcyon_core::balance::resolve_query(newapi).expect("newapi 的 custom 规则应可解析");
    // 示例规则等价内置 Kimi 模板：对 Kimi 生产实测形状解析结果应与内置模板一致
    let body = serde_json::json!({
        "limits": [{"detail": {"limit": "100", "used": "21"}, "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"}}],
        "usage": {"limit": "100", "used": "35"},
        "usages": {
            "limit_5h": {"reset_time": "2026-09-20T22:11:53Z", "used_ratio": 0.209618},
            "limit_7d": {"reset_time": "2026-09-26T15:11:53Z", "used_ratio": 0.354648}
        }
    });
    let rows = halcyon_core::balance::parse_custom(&q.spec, &body).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].label, "5h");
    assert_eq!(rows[0].pct, Some(21));
    assert_eq!(rows[1].label, "周");
    assert_eq!(rows[1].pct, Some(35));
}

#[test]
fn legacy_update_section_is_tolerated() {
    // 兼容：旧配置里的 update.include_prerelease 段（已随零 API 更新源移除）必须能解析且不生效
    let cfg: Config = serde_json::from_str(r#"{"update":{"include_prerelease":true}}"#).unwrap();
    assert!(ResolvedConfig::try_from(cfg).is_ok());
}
