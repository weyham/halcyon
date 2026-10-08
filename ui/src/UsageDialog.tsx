import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

// ---------- 类型（与 core/src/config.rs 的 BalanceCfg/CustomSpec 对应） ----------

export interface BalanceCfg {
  url?: string | null;
  template?: string | null;
  custom?: Record<string, unknown> | null;
}

interface BalanceRow {
  label: string;
  pct: number | null;
  reset: string | null;
  detail: string | null;
  text: string;
}

interface TestResult {
  url: string;
  status: string;
  hint?: string;
  rows?: BalanceRow[] | null;
  raw?: string | null;
}

type Mode = "auto" | "none" | "deepseek" | "kimi" | "glm" | "custom";

const ROW_FIELDS = [
  ["label", '档位名（@key 取映射键 / str:字面量 / 路径）'],
  ["pct", "已用百分比路径（直读，配合倍率）"],
  ["used", "已用量路径"],
  ["limit", "总量路径"],
  ["remaining", "剩余量路径"],
  ["reset", "重置时间路径（epoch 毫秒或 ISO 串）"],
  ["detail", "附加文本模板，如 ¥{total_balance}"],
] as const;

type RowFields = Record<(typeof ROW_FIELDS)[number][0], string>;

const emptyRow: RowFields = { label: "", pct: "", used: "", limit: "", remaining: "", reset: "", detail: "" };

function originOf(upstream: string): string {
  const m = upstream.match(/^([a-z]+:\/\/[^/]+)/i);
  return m ? m[1] : upstream.replace(/\/$/, "");
}

// ---------- 预设（与 core builtin_spec 的声明式等价物一致） ----------

interface Preset {
  url: (upstream: string) => string;
  rowsPath: string;
  rowsAs: string;
  scale: string;
  row: RowFields;
  specAdv: Record<string, unknown>;
  srcAdv: Record<string, unknown>;
}

const PRESETS: Record<"deepseek" | "kimi" | "glm", Preset> = {
  deepseek: {
    url: (u) => `${originOf(u)}/user/balance`,
    rowsPath: "balance_infos[0]",
    rowsAs: "",
    scale: "",
    row: { ...emptyRow, label: "str:余额", detail: "{currency:}{total_balance}" },
    specAdv: {
      field_map: { currency: { "": "", CNY: "¥", USD: "$", "*": "{raw} " } },
      flag_suffix: [{ path: "/is_available", equals: false, suffix: "（不可用）" }],
    },
    srcAdv: {},
  },
  kimi: {
    url: (u) => `${u.replace(/\/$/, "")}/usages`,
    rowsPath: "usages",
    rowsAs: "map",
    scale: "100",
    row: {
      ...emptyRow,
      label: "@key",
      pct: "used_ratio",
      reset: "reset_time",
      detail: "{/limits[0].detail.used}/{/limits[0].detail.limit}",
    },
    specAdv: {
      label_map: { limit_5h: "5h", limit_7d: "周", limit_30d: "月", limit_month: "月", limit_1m: "月" },
      label_strip_prefix: "limit_",
      sort: [{ suffix: "h" }, { contains: "周" }, { contains: "月" }],
    },
    srcAdv: { row_overrides: { limit_7d: { detail: "{/usage.used}/{/usage.limit}" } } },
  },
  glm: {
    url: (u) => `${originOf(u)}/api/monitor/usage/quota/limit`,
    rowsPath: "limits",
    rowsAs: "",
    scale: "",
    row: { ...emptyRow, label: "type", pct: "percentage", reset: "nextResetTime|resetTime" },
    specAdv: { label_map: { TOKENS_LIMIT: "5h 额度", TIME_LIMIT: "MCP 月额度" } },
    srcAdv: { row_filter: { path: "type", one_of: ["TOKENS_LIMIT", "TIME_LIMIT"] } },
  },
};

// ---------- 初始态拆解 ----------

interface FormState {
  mode: Mode;
  url: string;
  rowsPath: string;
  rowsAs: string;
  scale: string;
  row: RowFields;
  specAdv: string;
  srcAdv: string;
  rawMode: boolean;
  rawJson: string;
}

const pretty = (v: unknown) => JSON.stringify(v, null, 2);

function fromBalance(b: BalanceCfg | null): FormState {
  const base: FormState = {
    mode: "auto", url: "", rowsPath: "", rowsAs: "", scale: "",
    row: { ...emptyRow }, specAdv: "", srcAdv: "", rawMode: false, rawJson: "",
  };
  if (!b) return base;
  base.url = b.url ?? "";
  const t = b.template;
  if (t === "none" || t === "deepseek" || t === "kimi" || t === "glm") {
    base.mode = t;
    return base;
  }
  if (t !== "custom" || !b.custom) return base;
  base.mode = "custom";
  const spec = b.custom as Record<string, unknown>;
  const sources = (spec.sources as unknown[]) ?? [];
  if (sources.length !== 1 || typeof sources[0] !== "object" || sources[0] === null) {
    // 多来源等复杂配置：整体 JSON 编辑
    base.rawMode = true;
    base.rawJson = pretty(spec);
    return base;
  }
  const src = { ...(sources[0] as Record<string, unknown>) };
  const row = { ...((src.row as Record<string, string>) ?? {}) };
  delete src.row;
  base.rowsPath = (src.rows_path as string) ?? "";
  base.rowsAs = (src.rows_as as string) ?? "";
  delete src.rows_path;
  delete src.rows_as;
  for (const [key] of ROW_FIELDS) {
    base.row[key] = row[key] ?? "";
    delete row[key];
  }
  base.srcAdv = Object.keys(src).length > 0 ? pretty(src) : "";
  const rest = { ...spec };
  delete rest.sources;
  base.scale = rest.scale !== undefined ? String(rest.scale) : "";
  delete rest.scale;
  base.specAdv = Object.keys(rest).length > 0 ? pretty(rest) : "";
  return base;
}

// ---------- 组件 ----------

export default function UsageDialog({
  routeName,
  upstream,
  initial,
  onApply,
  onClose,
}: {
  routeName: string;
  upstream: string;
  initial: BalanceCfg | null;
  onApply: (b: BalanceCfg | null) => void;
  onClose: () => void;
}) {
  const [form, setForm] = useState<FormState>(() => fromBalance(initial));
  const [error, setError] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<TestResult | null>(null);

  const patch = (p: Partial<FormState>) => setForm((f) => ({ ...f, ...p }));

  const applyPreset = (name: "deepseek" | "kimi" | "glm") => {
    const p = PRESETS[name];
    patch({
      url: p.url(upstream),
      rowsPath: p.rowsPath,
      rowsAs: p.rowsAs,
      scale: p.scale,
      row: { ...p.row },
      specAdv: Object.keys(p.specAdv).length > 0 ? pretty(p.specAdv) : "",
      srcAdv: Object.keys(p.srcAdv).length > 0 ? pretty(p.srcAdv) : "",
      rawMode: false,
      rawJson: "",
    });
    setError(null);
  };

  /** 表单 → BalanceCfg；JSON 不合法时抛错（调用方捕获展示）。 */
  const buildBalance = (): BalanceCfg | null => {
    if (form.mode === "auto") return null;
    if (form.mode === "none") return { template: "none" };
    if (form.mode !== "custom") {
      return { template: form.mode, url: form.url.trim() || null };
    }
    const url = form.url.trim();
    if (!url) throw new Error("custom 模式需要填写查询 URL");
    let custom: Record<string, unknown>;
    if (form.rawMode) {
      custom = JSON.parse(form.rawJson || "{}") as Record<string, unknown>;
      if (!Array.isArray(custom.sources) || custom.sources.length === 0) {
        throw new Error("custom 规则需要非空 sources 数组");
      }
    } else {
      const srcAdv = form.srcAdv.trim() ? (JSON.parse(form.srcAdv) as Record<string, unknown>) : {};
      const specAdv = form.specAdv.trim() ? (JSON.parse(form.specAdv) as Record<string, unknown>) : {};
      const row: Record<string, string> = {};
      for (const [key] of ROW_FIELDS) {
        if (form.row[key].trim()) row[key] = form.row[key].trim();
      }
      if (Object.keys(row).length === 0) throw new Error("行字段映射至少填一项");
      const src: Record<string, unknown> = { ...srcAdv, row };
      if (form.rowsPath.trim()) src.rows_path = form.rowsPath.trim();
      if (form.rowsAs) src.rows_as = form.rowsAs;
      custom = { ...specAdv, sources: [src] };
      if (form.scale.trim()) {
        const n = Number(form.scale);
        if (!Number.isFinite(n) || n <= 0) throw new Error("倍率必须是正数");
        custom.scale = n;
      }
    }
    return { template: "custom", url, custom };
  };

  const runTest = async () => {
    setError(null);
    setResult(null);
    let balance: BalanceCfg | null;
    try {
      balance = buildBalance();
    } catch (e) {
      setError(`${e}`);
      return;
    }
    setTesting(true);
    try {
      const res = await invoke<TestResult>("test_balance_query", {
        name: routeName,
        upstream,
        balance,
      });
      setResult(res);
    } catch (e) {
      setError(`${e}`);
    } finally {
      setTesting(false);
    }
  };

  const apply = () => {
    try {
      onApply(buildBalance());
    } catch (e) {
      setError(`${e}`);
    }
  };

  const m = form.mode;
  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="page-head">
          <h2>用量查询 · {routeName}</h2>
          <button onClick={onClose}>关闭</button>
        </div>

        <div className="field-row">
          <label>
            模式
            <select value={m} onChange={(e) => patch({ mode: e.target.value as Mode })}>
              <option value="auto">自动识别（按上游地址）</option>
              <option value="none">关闭</option>
              <option value="deepseek">内置模板 DeepSeek</option>
              <option value="kimi">内置模板 Kimi</option>
              <option value="glm">内置模板 GLM</option>
              <option value="custom">自定义规则</option>
            </select>
          </label>
          <label className="grow">
            查询 URL{m === "custom" ? "（必填）" : "（可选，留空用内置地址）"}
            <input value={form.url} onChange={(e) => patch({ url: e.target.value })} placeholder="https://…" />
          </label>
        </div>

        {m === "custom" && (
          <>
            <div className="preset-row">
              <span className="hint">预设：</span>
              <button onClick={() => applyPreset("deepseek")}>DeepSeek 余额</button>
              <button onClick={() => applyPreset("kimi")}>Kimi 额度</button>
              <button onClick={() => applyPreset("glm")}>GLM 额度</button>
              <label className="raw-toggle">
                <input
                  type="checkbox"
                  checked={form.rawMode}
                  onChange={(e) =>
                    e.target.checked
                      ? patch({
                          rawMode: true,
                          rawJson: pretty(
                            (() => {
                              try { return (buildBalance() as BalanceCfg).custom; } catch { return { sources: [] }; }
                            })(),
                          ),
                        })
                      : patch({ rawMode: false })
                  }
                />
                整体 JSON 编辑
              </label>
            </div>

            {form.rawMode ? (
              <label className="grow">
                custom 规则（JSON）
                <textarea
                  className="code-area"
                  rows={14}
                  value={form.rawJson}
                  onChange={(e) => patch({ rawJson: e.target.value })}
                  spellCheck={false}
                />
              </label>
            ) : (
              <>
                <div className="field-row">
                  <label className="grow">
                    行列表路径 rows_path（留空 = 整个响应一行）
                    <input value={form.rowsPath} onChange={(e) => patch({ rowsPath: e.target.value })} placeholder="limits" />
                  </label>
                  <label>
                    结构
                    <select value={form.rowsAs} onChange={(e) => patch({ rowsAs: e.target.value })}>
                      <option value="">自动（数组/单对象）</option>
                      <option value="map">map（键→条目）</option>
                    </select>
                  </label>
                  <label>
                    倍率 scale
                    <input className="num-input" value={form.scale} onChange={(e) => patch({ scale: e.target.value })} placeholder="1" />
                  </label>
                </div>
                {ROW_FIELDS.map(([key, hint]) => (
                  <div className="field-row" key={key}>
                    <label className="grow">
                      {key} <span className="hint">{hint}</span>
                      <input
                        value={form.row[key]}
                        onChange={(e) => patch({ row: { ...form.row, [key]: e.target.value } })}
                      />
                    </label>
                  </div>
                ))}
                <label className="grow">
                  高级 · 来源级（JSON：row_filter / row_overrides 等）
                  <textarea
                    className="code-area"
                    rows={3}
                    value={form.srcAdv}
                    onChange={(e) => patch({ srcAdv: e.target.value })}
                    placeholder="{}"
                    spellCheck={false}
                  />
                </label>
                <label className="grow">
                  高级 · 规格级（JSON：label_map / field_map / flag_suffix / sort 等）
                  <textarea
                    className="code-area"
                    rows={4}
                    value={form.specAdv}
                    onChange={(e) => patch({ specAdv: e.target.value })}
                    placeholder="{}"
                    spellCheck={false}
                  />
                </label>
              </>
            )}
          </>
        )}

        <div className="dialog-actions">
          <button className="primary" onClick={apply}>确定</button>
          <button onClick={runTest} disabled={testing}>{testing ? "测试中…" : "测试"}</button>
        </div>

        {error && <div className="msg err">{error}</div>}
        {result && (
          <div className="test-result">
            <div className={`msg ${result.status === "ok" ? "ok" : "err"}`}>
              {result.status === "ok" ? "解析成功" : result.status}
              <span className="hint"> · {result.url}</span>
            </div>
            {result.hint && <div className="hint">{result.hint}</div>}
            {result.rows && result.rows.length > 0 && (
              <div className="usage-rows">
                {result.rows.map((r, i) => (
                  <div className="usage-row" key={i}>
                    <span className="usage-label">{r.label}</span>
                    {r.pct !== null && (
                      <span className="usage-bar">
                        <span className={`usage-fill ${r.pct >= 80 ? "hot" : ""}`} style={{ width: `${Math.min(100, r.pct)}%` }} />
                      </span>
                    )}
                    <span className="usage-value">{r.text}</span>
                  </div>
                ))}
              </div>
            )}
            {result.raw && (
              <details className="usage-raw">
                <summary>原始数据</summary>
                <pre>{result.raw}</pre>
              </details>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
