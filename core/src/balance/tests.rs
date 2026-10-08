use super::*;
use crate::config::{BalanceCfg, CustomSpec, RowFilter, RowSource, RowSpec};
use serde_json::json;
use std::collections::BTreeMap;

fn route(upstream: &str) -> Route {
    Route {
        name: "r".to_string(),
        upstream: upstream.to_string(),
        web_search: "note".to_string(),
        web_search_note_max: 20,
        aliases: Vec::new(),
        balance: None,
    }
}

// ---------- 路由解析 ----------

#[test]
fn detect_by_host() {
    assert_eq!(detect("https://api.deepseek.com"), Provider::DeepSeek);
    assert_eq!(detect("https://api.kimi.com/coding/v1"), Provider::Kimi);
    assert_eq!(detect("https://api.moonshot.cn/v1"), Provider::Kimi);
    assert_eq!(detect("https://open.bigmodel.cn/api/v1"), Provider::Glm);
    assert_eq!(detect("https://example.com/x"), Provider::Unknown);
}

#[test]
fn builtin_urls() {
    let q = resolve_query(&route("https://api.deepseek.com")).unwrap();
    assert_eq!(q.url, "https://api.deepseek.com/user/balance");
    assert_eq!(q.provider, Provider::DeepSeek);
    let q = resolve_query(&route("https://api.kimi.com/coding/v1")).unwrap();
    assert_eq!(q.url, "https://api.kimi.com/coding/v1/usages");
    let q = resolve_query(&route("https://open.bigmodel.cn/api/v1")).unwrap();
    assert_eq!(
        q.url,
        "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
    );
    assert!(resolve_query(&route("https://example.com")).is_none());
}

#[test]
fn config_override_template_and_url() {
    let mut r = route("https://proxy.example.com/anything");
    r.balance = Some(BalanceCfg {
        url: Some("https://custom.example.com/q".to_string()),
        template: Some("kimi".to_string()),
        custom: None,
    });
    let q = resolve_query(&r).unwrap();
    assert_eq!(q.provider, Provider::Kimi);
    assert_eq!(q.url, "https://custom.example.com/q", "URL 覆盖生效");
}

#[test]
fn config_template_none_disables() {
    let mut r = route("https://api.deepseek.com");
    r.balance = Some(BalanceCfg {
        template: Some("none".to_string()),
        ..Default::default()
    });
    assert!(resolve_query(&r).is_none());
}

#[test]
fn config_template_only_uses_builtin_url() {
    let mut r = route("https://proxy.example.com/v1");
    r.balance = Some(BalanceCfg {
        template: Some("deepseek".to_string()),
        ..Default::default()
    });
    let q = resolve_query(&r).unwrap();
    assert_eq!(
        q.url, "https://proxy.example.com/user/balance",
        "模板指定后按上游 origin 拼内置路径"
    );
}

#[test]
fn config_template_custom_uses_user_spec() {
    let spec = CustomSpec {
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
    let mut r = route("https://newapi.example.com/v1");
    r.balance = Some(BalanceCfg {
        url: Some("https://newapi.example.com/quota".to_string()),
        template: Some("custom".to_string()),
        custom: Some(spec.clone()),
    });
    let q = resolve_query(&r).unwrap();
    assert_eq!(q.url, "https://newapi.example.com/quota");
    assert_eq!(q.spec, spec, "用户 custom 规则原样生效");
    // custom 缺 url 或缺规则 → 不展示（配置加载时已拦截，这里是防御）
    r.balance = Some(BalanceCfg {
        template: Some("custom".to_string()),
        custom: Some(spec),
        ..Default::default()
    });
    assert!(resolve_query(&r).is_none());
}

// ---------- 内置三家逐字节回归（统一解析路径产出与重构前一致） ----------

#[test]
fn regression_deepseek_balance_cny() {
    // 录制响应：DS 余额
    let body = json!({"is_available": true, "balance_infos": [{"currency": "CNY", "total_balance": "110.00"}]});
    let rows = parse(Provider::DeepSeek, &body).unwrap();
    assert_eq!(
        rows,
        vec![QuotaRow {
            label: "余额".to_string(),
            pct: None,
            reset: None,
            reset_at_ms: None,
            detail: Some("¥110.00".to_string())
        }]
    );
    assert_eq!(rows[0].menu_text(), "余额 ¥110.00");
}

#[test]
fn regression_deepseek_unavailable_suffix_and_usd() {
    let body = json!({"is_available": false, "balance_infos": [{"currency": "USD", "total_balance": "3.50"}]});
    let rows = parse(Provider::DeepSeek, &body).unwrap();
    assert_eq!(rows[0].detail.as_deref(), Some("$3.50（不可用）"));
}

#[test]
fn regression_kimi_production_shape() {
    // 录制响应：Kimi 生产响应样例
    let body = json!({
        "limits": [
            {"detail": {"limit": "100", "remaining": "79", "resetTime": "2026-09-20T22:11:53Z", "used": "21"},
             "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"}}
        ],
        "usage": {"limit": "100", "remaining": "65", "resetTime": "2026-09-26T15:11:53Z", "used": "35"},
        "usages": {
            "limit_5h": {"reset_time": "2026-09-20T22:11:53Z", "used_ratio": 0.209618},
            "limit_7d": {"reset_time": "2026-09-26T15:11:53Z", "used_ratio": 0.354648}
        }
    });
    let rows = parse(Provider::Kimi, &body).unwrap();
    assert_eq!(
        rows,
        vec![
            QuotaRow {
                label: "5h".to_string(),
                pct: Some(21),
                reset: Some("2026-09-20 22:11".to_string()),
                reset_at_ms: Some(1789942313000),
                detail: Some("21/100".to_string()),
            },
            QuotaRow {
                label: "周".to_string(),
                pct: Some(35),
                reset: Some("2026-09-26 15:11".to_string()),
                reset_at_ms: Some(1790435513000),
                detail: Some("35/100".to_string()),
            },
        ]
    );
    let s = summary_row(&rows).unwrap();
    assert_eq!(s.label, "周", "摘要取用量最高档");
}

#[test]
fn regression_glm_quota() {
    // 录制响应：Zhipu 生产响应样例。
    // 关键形状：limits 位于 data 下；两个 TOKENS_LIMIT 分别是 5h 与周窗口；
    // TIME_LIMIT 是 MCP 月度工具额度（usageDetails 里为 search/web-reader/zread）。
    let body = json!({
        "code": 200,
        "data": {
            "level": "max",
            "limits": [
                {
                    "currentValue": 0,
                    "nextResetTime": 1792029670998i64,
                    "number": 1,
                    "percentage": 0,
                    "remaining": 4000,
                    "type": "TIME_LIMIT",
                    "unit": 5,
                    "usage": 4000,
                    "usageDetails": [
                        {"modelCode": "search-prime", "usage": 0},
                        {"modelCode": "web-reader", "usage": 0},
                        {"modelCode": "zread", "usage": 0}
                    ]
                },
                {"number": 5, "percentage": 0, "type": "TOKENS_LIMIT", "unit": 3},
                {"nextResetTime": 1790585461983i64, "number": 1, "percentage": 23, "type": "TOKENS_LIMIT", "unit": 6}
            ]
        },
        "msg": "操作成功",
        "success": true
    });
    let rows = parse(Provider::Glm, &body).unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0].label, "5h 额度");
    assert_eq!(rows[0].pct, Some(0));
    assert_eq!(rows[0].reset, None);
    assert_eq!(rows[0].detail, None);
    assert_eq!(rows[1].label, "周额度");
    assert_eq!(rows[1].pct, Some(23));
    assert!(
        rows[1]
            .reset
            .as_deref()
            .unwrap_or_default()
            .starts_with("09-28"),
        "{rows:?}"
    );
    assert_eq!(rows[2].label, "MCP 月额度");
    assert_eq!(rows[2].pct, Some(0));
    assert_eq!(rows[2].detail.as_deref(), Some("0/4k"));
    assert!(
        rows[2]
            .reset
            .as_deref()
            .unwrap_or_default()
            .starts_with("10-15"),
        "{rows:?}"
    );
    assert!(
        rows[1].menu_text().starts_with("周额度 23% · 09-28"),
        "{}",
        rows[1].menu_text()
    );
}

// ---------- 异常退化（供应商改形状） ----------

#[test]
fn degraded_shape_errors_but_does_not_crash() {
    // 各家形状整个变掉 → 解析失败（面板显示错误 + 原始数据仍可见），不崩溃、不显假零
    assert!(parse(Provider::DeepSeek, &json!({})).is_err());
    assert!(parse(Provider::DeepSeek, &json!({"balance_infos": []})).is_err());
    assert!(parse(Provider::Kimi, &json!({"data": []})).is_err());
    assert!(parse(Provider::Glm, &json!({"data": {"limits": []}})).is_err());
    // 部分字段缺失：Kimi 缺 limits/usage → detail 丢弃但 pct 仍在（优雅降级，不是失败）
    let body = json!({"usages": {"limit_5h": {"used_ratio": 0.5}}});
    let rows = parse(Provider::Kimi, &body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pct, Some(50));
    assert_eq!(
        rows[0].detail, None,
        "绝对值来源缺失时 detail 丢弃，行仍保留"
    );
}

// ---------- 路径求值 ----------

#[test]
fn path_eval_basic() {
    let root = json!({"a": {"b": [{"c": 1}, {"c": 2}]}, "list": [10, 20]});
    let entry = &root["a"];
    assert_eq!(eval_path(&root, &root, "a.b[1].c"), Some(&json!(2)));
    assert_eq!(eval_path(&root, &root, "list[0]"), Some(&json!(10)));
    assert_eq!(
        eval_path(&root, entry, "b[0].c"),
        Some(&json!(1)),
        "相对条目"
    );
    assert_eq!(
        eval_path(&root, entry, "/list[1]"),
        Some(&json!(20)),
        "根路径"
    );
    assert_eq!(eval_path(&root, entry, "."), Some(entry), "条目自身");
    assert_eq!(
        eval_path(&root, &root, "a.missing|list[0]"),
        Some(&json!(10)),
        "| 候选回退"
    );
    assert_eq!(eval_path(&root, &root, "a.missing.deep"), None);
    assert_eq!(eval_path(&root, &root, "list[9]"), None);
    // 语法错误 → None（运行期退化；配置期由 validate_custom 拦截）
    assert_eq!(eval_path(&root, &root, "a..b"), None);
    assert_eq!(eval_path(&root, &root, "a[x]"), None);
}

// ---------- detail 插值 ----------

fn detail_of(template: &str, body: &Value) -> Option<String> {
    render_detail(template, body, body, &None)
}

#[test]
fn detail_interpolation() {
    let body = json!({"total": "110.00", "n": 1500, "missing_pad": {"x": 1}});
    assert_eq!(detail_of("¥{total}", &body).as_deref(), Some("¥110.00"));
    assert_eq!(
        detail_of("{n}", &body).as_deref(),
        Some("1.5k"),
        "数字走 fmt_num"
    );
    assert_eq!(
        detail_of("{absent:—}", &body).as_deref(),
        Some("—"),
        "缺省值"
    );
    assert_eq!(
        detail_of("{absent}", &body),
        None,
        "缺失无缺省 → 整个 detail 丢弃"
    );
    assert_eq!(
        detail_of("{{literal}} {total}", &body).as_deref(),
        Some("{literal} 110.00"),
        "转义"
    );
    assert_eq!(detail_of("{total", &body), None, "未闭合 → 丢弃");
}

#[test]
fn detail_field_map_and_wildcard() {
    let body = json!({"currency": "EUR", "total": "9.00"});
    let fm = Some(BTreeMap::from([(
        "currency".to_string(),
        map_of(&[("CNY", "¥"), ("*", "{raw} ")]),
    )]));
    let out = render_detail("{currency}{total}", &body, &body, &fm).unwrap();
    assert_eq!(out, "EUR 9.00", "通配项 {{raw}} 引用原值");
    let body = json!({"currency": "CNY", "total": "9.00"});
    let out = render_detail("{currency}{total}", &body, &body, &fm).unwrap();
    assert_eq!(out, "¥9.00", "精确命中映射");
}

// ---------- pct 规则（scale 与推算优先级） ----------

fn pct_spec(row: RowSpec, scale: Option<f64>) -> CustomSpec {
    CustomSpec {
        sources: vec![RowSource {
            row,
            ..Default::default()
        }],
        scale,
        ..Default::default()
    }
}

#[test]
fn pct_priority_and_scale() {
    // 直读 × scale
    let spec = pct_spec(
        RowSpec {
            pct: Some("ratio".to_string()),
            used: Some("u".to_string()),
            limit: Some("l".to_string()),
            ..Default::default()
        },
        Some(100.0),
    );
    let rows = parse_custom(&spec, &json!({"ratio": 0.625, "u": 1, "l": 2})).unwrap();
    assert_eq!(
        rows[0].pct,
        Some(63),
        "直读 ×scale 优先于推算（0.625×100=62.5→63）"
    );
    // 无直读 → used+limit
    let spec = pct_spec(
        RowSpec {
            used: Some("u".to_string()),
            limit: Some("l".to_string()),
            remaining: Some("r".to_string()),
            ..Default::default()
        },
        None,
    );
    let rows = parse_custom(&spec, &json!({"u": "16", "l": "100", "r": "84"})).unwrap();
    assert_eq!(rows[0].pct, Some(16), "used+limit 优先于 remaining+limit");
    // 仅 remaining+limit
    let spec = pct_spec(
        RowSpec {
            limit: Some("l".to_string()),
            remaining: Some("r".to_string()),
            ..Default::default()
        },
        None,
    );
    let rows = parse_custom(&spec, &json!({"l": 100, "r": 79})).unwrap();
    assert_eq!(rows[0].pct, Some(21));
}

// ---------- map 模式 / 过滤 / 覆盖 / 排序 ----------

#[test]
fn map_mode_label_key_and_strip() {
    let spec = CustomSpec {
        sources: vec![RowSource {
            rows_path: Some("usages".to_string()),
            rows_as: Some("map".to_string()),
            row: RowSpec {
                label: Some("@key".to_string()),
                pct: Some("p".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }],
        label_map: Some(map_of(&[("limit_5h", "5h")])),
        label_strip_prefix: Some("limit_".to_string()),
        ..Default::default()
    };
    let body = json!({"usages": {"limit_5h": {"p": 1}, "limit_14d": {"p": 2}}});
    let rows = parse_custom(&spec, &body).unwrap();
    assert_eq!(rows.len(), 2);
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"5h"), "label_map 命中：{labels:?}");
    assert!(labels.contains(&"14d"), "未命中剥前缀：{labels:?}");
}

#[test]
fn row_filter_and_overrides() {
    let spec = CustomSpec {
        sources: vec![RowSource {
            rows_path: Some("items".to_string()),
            row: RowSpec {
                label: Some("type".to_string()),
                pct: Some("pct".to_string()),
                ..Default::default()
            },
            row_filter: Some(RowFilter {
                path: "type".to_string(),
                one_of: vec!["A".to_string()],
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let body = json!({"items": [{"type": "A", "pct": 1}, {"type": "B", "pct": 2}]});
    let rows = parse_custom(&spec, &body).unwrap();
    assert_eq!(rows.len(), 1, "未通过过滤的行被丢弃");
    assert_eq!(rows[0].label, "A");
}

#[test]
fn sort_rules_stable() {
    let spec = CustomSpec {
        sources: vec![RowSource {
            rows_path: Some("items".to_string()),
            row: RowSpec {
                label: Some("name".to_string()),
                pct: Some("p".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }],
        sort: vec![
            SortRule {
                suffix: Some("h".to_string()),
                ..Default::default()
            },
            SortRule {
                contains: Some("周".to_string()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let body = json!({"items": [
        {"name": "周额度", "p": 1},
        {"name": "其他", "p": 2},
        {"name": "5h", "p": 3}
    ]});
    let rows = parse_custom(&spec, &body).unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["5h", "周额度", "其他"],
        "按规则分组，未命中排最后"
    );
}

// ---------- 校验 ----------

#[test]
fn validate_custom_rejects_bad_specs() {
    assert!(
        validate_custom(&CustomSpec::default()).is_err(),
        "sources 为空"
    );
    let bad_path = CustomSpec {
        sources: vec![RowSource {
            rows_path: Some("a..b".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(validate_custom(&bad_path).is_err(), "rows_path 语法错误");
    let bad_template = CustomSpec {
        sources: vec![RowSource {
            row: RowSpec {
                detail: Some("{oops".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(validate_custom(&bad_template).is_err(), "模板未闭合");
    let bad_key = CustomSpec {
        sources: vec![RowSource {
            row: RowSpec {
                label: Some("@key".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(validate_custom(&bad_key).is_err(), "@key 仅 map 模式");
    let bad_sort = CustomSpec {
        sources: vec![RowSource::default()],
        sort: vec![SortRule::default()],
        ..Default::default()
    };
    assert!(validate_custom(&bad_sort).is_err(), "sort 空规则");
    let ok = CustomSpec {
        sources: vec![RowSource::default()],
        ..Default::default()
    };
    assert!(validate_custom(&ok).is_ok());
}

// ---------- 托盘显示格式 ----------

#[test]
fn tray_label_compacts_week_and_month_only() {
    assert_eq!(tray_label("周"), "7d");
    assert_eq!(tray_label("月"), "30d");
    assert_eq!(tray_label("5h"), "5h");
    assert_eq!(tray_label("余额"), "余额");
    assert_eq!(tray_label("MCP"), "MCP");
}

#[test]
fn fetched_age_text_covers_minutes_hours_days_and_clock_skew() {
    let minute = 60_000;
    let now = 1_000_000_000i64;
    assert_eq!(fetched_age_text(now, now), "刚刚");
    assert_eq!(
        fetched_age_text(now, now + 5 * minute),
        "刚刚",
        "时钟回拨按刚刚处理"
    );
    assert_eq!(fetched_age_text(now, now - minute), "1 分钟前");
    assert_eq!(fetched_age_text(now, now - 59 * minute), "59 分钟前");
    assert_eq!(fetched_age_text(now, now - 60 * minute), "1 小时前");
    assert_eq!(fetched_age_text(now, now - 23 * 60 * minute), "23 小时前");
    assert_eq!(fetched_age_text(now, now - 24 * 60 * minute), "1 天前");
    assert_eq!(fetched_age_text(now, now - 60 * 60 * minute), "2 天前");
}

#[test]
fn format_remaining_compact_omits_zero_high_units() {
    let minute = 60_000;
    assert_eq!(format_remaining_compact(100 * minute), "1h40m");
    assert_eq!(format_remaining_compact(1540 * minute), "1d1h40m");
    assert_eq!(format_remaining_compact(1600 * minute), "1d2h40m");
    assert_eq!(format_remaining_compact(40 * minute), "40m");
    assert_eq!(format_remaining_compact(120 * minute), "2h");
    assert_eq!(format_remaining_compact(1440 * minute), "1d");
    assert_eq!(format_remaining_compact(1500 * minute), "1d1h");
    assert_eq!(format_remaining_compact(0), "1m", "不足 1 分钟按 1m 计");
    assert_eq!(format_remaining_compact(-5 * minute), "1m");
}

#[test]
fn tray_row_text_quota_balance_and_label_only() {
    let now = 1_000_000_000i64;
    let quota = QuotaRow {
        label: "周".to_string(),
        pct: Some(60),
        reset: None,
        reset_at_ms: Some(now + 1540 * 60_000),
        detail: Some("35/100".to_string()),
    };
    assert_eq!(tray_row_text(&quota, now), "7d: 60% ⌛1d1h40m");

    let balance = QuotaRow {
        label: "余额".to_string(),
        pct: None,
        reset: None,
        reset_at_ms: None,
        detail: Some("¥12.34".to_string()),
    };
    assert_eq!(tray_row_text(&balance, now), "余额: ¥12.34");

    let bare = QuotaRow {
        label: "MCP".to_string(),
        pct: None,
        reset: None,
        reset_at_ms: None,
        detail: None,
    };
    assert_eq!(tray_row_text(&bare, now), "MCP");

    let expired = QuotaRow {
        reset_at_ms: Some(now - 60_000),
        ..quota.clone()
    };
    assert_eq!(
        tray_row_text(&expired, now),
        "7d: 60%",
        "已过期的重置时间不显示"
    );
}
