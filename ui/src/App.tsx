import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import UsageDialog, { type BalanceCfg } from "./UsageDialog";
import "./App.css";

// ---------- 类型 ----------

interface RouteEntry {
  upstream: string;
  web_search: string;
  web_search_note_max: number;
  aliases: string[];
  balance?: BalanceCfg | null;
}

interface ShimConfig {
  listen: string;
  log_level: string;
  log_file: string;
  routes: Record<string, RouteEntry>;
  default_route: string | null;
  orphan_header: boolean;
}

interface Status {
  running: boolean;
  addr?: string;
  stats?: { requests: number; rewritten_requests: number; upstream_errors: number };
}

interface AppInfo {
  version: string;
  product: string;
  repository: string;
  update_source: string;
  update_status: string;
  update_note: string;
  available_version?: string | null;
  release_url?: string | null;
}


interface BalanceRow {
  label: string;
  pct: number | null;
  reset: string | null;
  reset_at_ms: number | null;
  detail: string | null;
  text: string;
}

interface RouteBalance {
  name: string;
  status: string | null;
  raw: string | null;
  rows: BalanceRow[] | null;
}

// ---------- 模型切换自动修复（队列） ----------

interface AutoQueueItem {
  threadId: string;
  title: string | null;
  project: string | null;
  // 与 Rust 侧 serde rename_all="camelCase" 对齐：Pending→"pending"、Failed→{ failed: string }
  status: "pending" | "done" | "cancelled" | { failed: string };
}

interface AutoQueue {
  targetModel: string;
  items: AutoQueueItem[];
}

interface AutoQueueState {
  queue: AutoQueue | null;
  codex_running: boolean;
  executing: boolean;
}

// ---------- 修复页统一扫描 ----------

interface StaleRoot {
  path: string;
  action: "Remove" | { Remap: string[] };
}

interface LineageBreak {
  shard: string;
  parentRef: string;
  reason: string;
}

interface UnifiedTask {
  threadId: string;
  title?: string;
  project?: string;
  badEntries: number;
  modelMismatch?: string;
  staleRoots: StaleRoot[];
  lineageBreaks: LineageBreak[];
}

interface UnifiedScanReport {
  default_model: string;
  tasks: UnifiedTask[];
}

interface RuntimeInfo {
  mode: "portable" | "installed";
  data_dir: string;
  config_path: string;
}

interface RouteRow {
  name: string;
  upstream: string;
  aliases: string;
  web_search: string;
  balance: BalanceCfg | null;
}

/** 路由卡片上"用量查询"一行的摘要文本。 */
function balanceSummary(b: BalanceCfg | null): string {
  if (!b) return "自动识别";
  if (b.template === "none") return "已关闭";
  if (b.template === "custom") return "自定义规则";
  if (b.template) return `内置模板 ${b.template}`;
  return b.url ? "自动识别（改 URL）" : "自动识别";
}

const emptyConfig: ShimConfig = {
  listen: "127.0.0.1:8788",
  log_level: "info",
  log_file: "",
  routes: {},
  default_route: null,
  orphan_header: true,
};

type Page = "usage" | "repair" | "log" | "settings" | "about";
// 修复历史记录（「记录」页）
interface RepairRecord {
  threadId: string;
  title?: string | null;
  project?: string | null;
  kinds: { model: boolean; entries: boolean; roots: boolean };
  outcome: string;
  details: string[];
  executedAt: string;
}

// ---------- 路由表编辑态转换 ----------

function toRows(routes: Record<string, RouteEntry>): RouteRow[] {
  return Object.entries(routes).map(([name, r]) => ({
    name,
    upstream: r.upstream,
    aliases: r.aliases.join(", "),
    web_search: r.web_search,
    balance: r.balance ?? null,
  }));
}

function toConfig(rows: RouteRow[], base: ShimConfig): ShimConfig {
  const routes: Record<string, RouteEntry> = {};
  for (const row of rows) {
    const name = row.name.trim();
    if (!name) continue;
    routes[name] = {
      upstream: row.upstream.trim(),
      web_search: row.web_search,
      web_search_note_max: 20,
      aliases: row.aliases
        .split(",")
        .map((a) => a.trim())
        .filter((a) => a.length > 0),
      balance: row.balance,
    };
  }
  return { ...base, routes };
}

// ---------- 主组件 ----------

export default function App() {
  const [page, setPage] = useState<Page>("usage");
  // 更新红点徽标（后台定时检查写入；安装后下一轮检查自动清除）
  const [updateBadge, setUpdateBadge] = useState<{
    version: string;
    ready: boolean;
    manualOnly: boolean;
  } | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [balances, setBalances] = useState<RouteBalance[]>([]);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);

  // 设置页状态
  const [rows, setRows] = useState<RouteRow[]>([]);
  const [usageEdit, setUsageEdit] = useState<number | null>(null);
  const [base, setBase] = useState<ShimConfig>(emptyConfig);
  const [message, setMessage] = useState<{ kind: "ok" | "err"; text: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const [runtimeInfo, setRuntimeInfo] = useState<RuntimeInfo | null>(null);
  const [runtimeError, setRuntimeError] = useState<string | null>(null);

  // 修复页状态（统一扫描）
  const [repairInput, setRepairInput] = useState("");
  const [unifiedTasks, setUnifiedTasks] = useState<UnifiedTask[] | null>(null);
  const [unifiedSelected, setUnifiedSelected] = useState<string[]>([]);
  const [unifiedBusy, setUnifiedBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [repairProgress, setRepairProgress] = useState<{
    settled: number;
    total: number;
  } | null>(null);
  const [unifiedError, setUnifiedError] = useState<string | null>(null);
  const [unifiedScannedAt, setUnifiedScannedAt] = useState<string | null>(null);
  // 模型切换自动修复队列
  const [autoQueue, setAutoQueue] = useState<AutoQueueState | null>(null);
  const [autoQueueError, setAutoQueueError] = useState<string | null>(null);
  // 修复记录页
  const [repairLog, setRepairLog] = useState<RepairRecord[] | null>(null);
  const [repairLogQuery, setRepairLogQuery] = useState("");
  const [defaultModel, setDefaultModel] = useState<string | null>(null);

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await invoke<Status>("get_status"));
    } catch {
      /* 忽略瞬时错误 */
    }
  }, []);

  const refreshBalances = useCallback(async () => {
    try {
      const res = await invoke<{ routes: RouteBalance[] }>("get_balances");
      setBalances(res.routes);
    } catch {
      /* 忽略瞬时错误 */
    }
  }, []);

  const manualRefreshBalances = async () => {
    try {
      await invoke("refresh_balances");
      setTimeout(refreshBalances, 2000);
    } catch {
      /* 忽略瞬时错误 */
    }
  };

  useEffect(() => {
    (async () => {
      try {
        const res = await invoke<{ config: ShimConfig; source: string }>("get_config");
        setBase(res.config);
        setRows(toRows(res.config.routes));
      } catch (e) {
        setMessage({ kind: "err", text: `读取配置失败：${e}` });
      }
      refreshStatus();
      refreshBalances();
      try {
        setAppInfo(await invoke<AppInfo>("get_app_info"));
      } catch (e) {
        setUpdateMessage(`读取应用信息失败：${e}`);
      }
      try {
        setRuntimeInfo(await invoke<RuntimeInfo>("get_runtime_info"));
      } catch (e) {
        setRuntimeError(`${e}`);
      }
    })();
    const timer = setInterval(refreshStatus, 5000);
    const timer2 = setInterval(refreshBalances, 30000);
    return () => {
      clearInterval(timer);
      clearInterval(timer2);
    };
  }, [refreshStatus, refreshBalances]);

  const checkUpdate = async () => {
    setUpdateBusy(true);
    setUpdateMessage(null);
    try {
      const result = await invoke<AppInfo>("check_update");
      setAppInfo(result);
      setUpdateMessage(result.update_note);
    } catch (e) {
      setUpdateMessage(`检查更新失败：${e}`);
    } finally {
      setUpdateBusy(false);
    }
  };


  const openExternal = async (url: string) => {
    try {
      await invoke("open_external_url", { url });
    } catch (e) {
      setUpdateMessage(`打开链接失败：${e}`);
    }
  };

  const downloadUpdate = async () => {
    setUpdateBusy(true);
    try {
      const result = await invoke<AppInfo>("update_download");
      setAppInfo(result);
      setUpdateMessage(result.update_note);
    } catch (e) {
      setUpdateMessage(`下载更新失败：${e}`);
    } finally {
      setUpdateBusy(false);
    }
  };

  const installUpdate = async () => {
    setUpdateBusy(true);
    try {
      await invoke("update_install");
    } catch (e) {
      setUpdateMessage(`安装更新失败：${e}`);
      setUpdateBusy(false);
    }
  };

  const addr = status?.running ? status.addr : null;

  // ---------- 设置页逻辑 ----------

  const updateRow = (i: number, patch: Partial<RouteRow>) => {
    setRows((rs) => rs.map((r, idx) => (idx === i ? { ...r, ...patch } : r)));
  };

  const save = async () => {
    setSaving(true);
    setMessage(null);
    try {
      const config = toConfig(rows, base);
      const res = await invoke<{ path: string }>("save_config", { config });
      setMessage({ kind: "ok", text: `已保存，代理已重启` });
      setTimeout(() => {
        refreshStatus();
        setPage("usage");
      }, 800);
      void res;
    } catch (e) {
      setMessage({ kind: "err", text: `${e}` });
    } finally {
      setSaving(false);
    }
  };

  // ---------- 修复页逻辑 ----------

  // ---------- 修复页逻辑（统一扫描） ----------
  const scanUnified = async (single: boolean) => {
    setUnifiedBusy(true);
    setScanning(true);
    setUnifiedError(null);
    try {
      const res = await invoke<UnifiedScanReport>("scan_unified", {
        input: single ? repairInput.trim() : null,
      });
      setUnifiedTasks(res.tasks);
      setDefaultModel(res.default_model);
      setUnifiedScannedAt(
        new Date().toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" }),
      );
      // 默认勾选可修复的任务：队列里已排队/已修复的不再预选
      const settled = new Set(
        (autoQueue?.queue?.items ?? [])
          .filter((i) => i.status === "pending" || i.status === "done")
          .map((i) => i.threadId),
      );
      // 默认只勾选可操作项（模型/目录/血缘）；仅坏条目的任务由实时保护覆盖
      setUnifiedSelected(
        res.tasks
          .filter(
            (t) =>
              !settled.has(t.threadId) &&
              (Boolean(t.modelMismatch) || t.staleRoots.length > 0 || t.lineageBreaks.length > 0),
          )
          .map((t) => t.threadId),
      );
    } catch (e) {
      setUnifiedError(`${e}`);
    } finally {
      setUnifiedBusy(false);
      setScanning(false);
    }
  };

  const toggleUnifiedTask = (threadId: string) => {
    setUnifiedSelected((ids) =>
      ids.includes(threadId) ? ids.filter((id) => id !== threadId) : [...ids, threadId],
    );
  };

  const toggleAllUnified = () => {
    if (!unifiedTasks) return;
    setUnifiedSelected((ids) =>
      ids.length === unifiedTasks.length ? [] : unifiedTasks.map((t) => t.threadId),
    );
  };

  const repairUnifiedSelected = async () => {
    if (unifiedSelected.length === 0) return;
    setUnifiedBusy(true);
    setUnifiedError(null);
    try {
      await invoke("enqueue_unified_repairs", { threadIds: unifiedSelected });
      if (!autoQueue?.codex_running) {
        try {
          await invoke("execute_auto_repair_now");
        } catch {
          /* 队列由后台 tick 接管 */
        }
      }
      setUnifiedSelected([]);
    } catch (e) {
      setUnifiedError(`${e}`);
    } finally {
      setUnifiedBusy(false);
      void refreshAutoQueue();
    }
  };

  // ---------- 模型不匹配修复逻辑 ----------
  // ---------- 模型切换自动修复（队列） ----------
  const refreshAutoQueue = useCallback(async () => {
    try {
      const st = await invoke<AutoQueueState>("get_auto_repair_queue");
      setAutoQueue(st);
      if (!st.executing) setRepairProgress(null);
    } catch {
      /* 命令不可用：保持现状 */
    }
  }, []);

  // 红点徽标由应用信息派生（与后端状态一致）：有可安装/可下载的更高版本即显示
  useEffect(() => {
    if (!appInfo) return;
    const status = appInfo.update_status;
    const version = appInfo.available_version;
    if (version && (status === "待安装" || status === "有更新" || status === "需手动下载")) {
      setUpdateBadge({
        version,
        ready: status === "待安装",
        manualOnly: status === "需手动下载",
      });
    } else {
      setUpdateBadge(null);
    }
  }, [appInfo]);

  useEffect(() => {
    void refreshAutoQueue();
    const unlisten = listen("auto-repair-changed", () => void refreshAutoQueue());
    // 逐项进度：执行期间队列被写锁持有，用该事件驱动进度条与消项。
    const unlistenProgress = listen<{
      settled: number;
      total: number;
      threadId: string;
      ok: boolean;
    }>("auto-repair-progress", (e) => {
      setRepairProgress({ settled: e.payload.settled, total: e.payload.total });
      if (e.payload.ok) {
        const tid = e.payload.threadId;
        setUnifiedTasks((ts) => ts?.filter((t) => t.threadId !== tid) ?? ts);
        setUnifiedSelected((ids) => ids.filter((id) => id !== tid));
      }
    });
    // 面板关闭 = 扫描结果作废，下次打开是全新状态
    const unlistenHidden = listen("panel-hidden", () => {
      setUnifiedTasks(null);
      setUnifiedSelected([]);
      setUnifiedError(null);
      setUnifiedScannedAt(null);
      setRepairInput("");
    });
    // 更新红点：后台定时检查（启动 60s 首查、之后每 3 小时）的广播 → 刷新应用信息
    const unlistenUpdate = listen("update-availability", () => {
      void (async () => setAppInfo(await invoke<AppInfo>("get_app_info")))();
    });
    // 托盘红点条目：跳到关于页
    const unlistenNavAbout = listen("navigate-about", () => setPage("about"));
    // codex_running 状态靠轻量轮询跟随（事件只在队列变化时发）
    const timer = setInterval(() => void refreshAutoQueue(), 5000);
    return () => {
      clearInterval(timer);
      void unlisten.then((f) => f());
      void unlistenProgress.then((f) => f());
      void unlistenHidden.then((f) => f());
      void unlistenUpdate.then((f) => f());
      void unlistenNavAbout.then((f) => f());
    };
  }, [refreshAutoQueue]);

  const cancelAutoItem = async (threadId: string) => {
    setAutoQueueError(null);
    try {
      await invoke("cancel_auto_repair_item", { threadId });
    } catch (e) {
      setAutoQueueError(`${e}`);
    }
    void refreshAutoQueue();
  };

  const executeAutoNow = async () => {
    setAutoQueueError(null);
    try {
      await invoke("execute_auto_repair_now");
    } catch (e) {
      setAutoQueueError(`${e}`);
    }
    void refreshAutoQueue();
  };

  const loadRepairLog = useCallback(async () => {
    try {
      const res = await invoke<{ records: RepairRecord[] }>("get_repair_history");
      setRepairLog(res.records);
    } catch {
      setRepairLog([]);
    }
  }, []);

  const deleteLogRecord = async (displayIndex: number) => {
    if (!window.confirm("删除这条修复记录？")) return;
    try {
      await invoke("delete_repair_history_record", { index: displayIndex });
    } catch (e) {
      setMessage({ kind: "err", text: `删除记录失败：${e}` });
    }
    void loadRepairLog();
  };

  const clearLog = async () => {
    if (!window.confirm("清空全部修复记录？此操作不可恢复。")) return;
    try {
      await invoke("clear_repair_history");
    } catch (e) {
      setMessage({ kind: "err", text: `清空记录失败：${e}` });
    }
    void loadRepairLog();
  };

  // 进入「记录」页时加载修复历史（最新在前）
  useEffect(() => {
    if (page !== "log") return;
    void loadRepairLog();
  }, [page, loadRepairLog]);

  // ---------- 布局 ----------

  const running = !!status?.running;

  return (
    <div className="layout">
      <aside className="side">
        <div className="brand">
          <div className="brand-name">Halcyon</div>
          <div className={`brand-status ${running ? "ok" : "down"}`}>
            <span className="dot" />
            {running ? "运行中" : "未运行"}
          </div>
          {addr && <div className="brand-addr">{addr}</div>}
        </div>
        <nav className="nav">
          {(
            [
              ["usage", "用量"],
              ["repair", "修复"],
              ["log", "记录"],
              ["settings", "设置"],
              ["about", "关于"],
            ] as [Page, string][]
          ).map(([key, label]) => (
            <button
              key={key}
              className={`nav-item ${page === key ? "active" : ""}`}
              onClick={() => setPage(key)}
            >
              {label}
              {key === "about" && updateBadge && (
                <span
                  className="nav-dot"
                  title={updateBadge.ready
                    ? `有更新 ${updateBadge.version} 已就绪，可安装`
                    : `有更新 ${updateBadge.version} 可用`}
                />
              )}
            </button>
          ))}
        </nav>
      </aside>

      <main className="content">
        {page === "usage" && (
          <UsagePage
            status={status}
            balances={balances}
            onRefresh={manualRefreshBalances}
          />
        )}
        {page === "log" && (
          <RepairLogPage
            records={repairLog}
            query={repairLogQuery}
            setQuery={setRepairLogQuery}
            onDelete={deleteLogRecord}
            onClear={clearLog}
          />
        )}
        {page === "repair" && (
          <RepairPage
            autoQueue={autoQueue}
            autoQueueError={autoQueueError}
            onAutoCancel={cancelAutoItem}
            onAutoExecute={executeAutoNow}
            onOpenLog={() => setPage("log")}
            input={repairInput}
            setInput={setRepairInput}
            tasks={unifiedTasks}
            selected={unifiedSelected}
            busy={unifiedBusy}
            scanning={scanning}
            repairProgress={repairProgress}
            error={unifiedError}
            scannedAt={unifiedScannedAt}
            defaultModel={defaultModel}
            onScan={scanUnified}
            onToggle={toggleUnifiedTask}
            onToggleAll={toggleAllUnified}
            onRepair={repairUnifiedSelected}
          />
        )}
        {page === "settings" && (
          <SettingsPage
            rows={rows}
            updateRow={updateRow}
            setRows={setRows}
            base={base}
            setBase={setBase}
            addr={addr}
            saving={saving}
            message={message}
            runtimeInfo={runtimeInfo}
            runtimeError={runtimeError}
            onSave={save}
            onEditUsage={setUsageEdit}
          />
        )}
        {page === "about" && (
          <AboutPage
            appInfo={appInfo}
            busy={updateBusy}
            message={updateMessage}
            onCheck={checkUpdate}
            onDownload={downloadUpdate}
            onInstall={installUpdate}
            onOpenExternal={openExternal}
          />
        )}
      </main>
      {usageEdit !== null && rows[usageEdit] && (
        <UsageDialog
          routeName={rows[usageEdit].name.trim() || `路由 ${usageEdit + 1}`}
          upstream={rows[usageEdit].upstream.trim()}
          initial={rows[usageEdit].balance}
          onApply={(b) => {
            updateRow(usageEdit, { balance: b });
            setUsageEdit(null);
            setMessage({ kind: "ok", text: "用量查询配置已更新，点「保存并重启代理」生效" });
          }}
          onClose={() => setUsageEdit(null)}
        />
      )}
    </div>
  );
}

// ---------- 用量页 ----------

function UsagePage({
  status,
  balances,
  onRefresh,
}: {
  status: Status | null;
  balances: RouteBalance[];
  onRefresh: () => void;
}) {
  const stats = status?.stats;
  return (
    <div className="page-body">
      <div className="page-head">
        <h2>用量</h2>
        <button onClick={onRefresh}>刷新</button>
      </div>

      <div className="stat-cards">
        <div className="stat-card">
          <div className="stat-num">{stats?.requests ?? "-"}</div>
          <div className="stat-label">请求</div>
        </div>
        <div className="stat-card">
          <div className="stat-num">{stats?.rewritten_requests ?? "-"}</div>
          <div className="stat-label">改写</div>
        </div>
        <div className="stat-card">
          <div className="stat-num">{stats?.upstream_errors ?? "-"}</div>
          <div className="stat-label">上游错误</div>
        </div>
      </div>

      <p className="hint">
        每个中转 API 的余额/额度详情（5 小时 / 周 / 月窗口与重置时间，以接口实际返回为准）。路由有过流量后自动出现。
      </p>

      {balances.filter((b) => b.status !== null).map((b) => (
        <div className="card" key={b.name}>
          <div className="card-title">{b.name}</div>
          {b.status === "ok" && b.rows ? (
            <div className="usage-rows">
              {b.rows.map((r, i) => (
                <div className="usage-row" key={i}>
                  <span className="usage-label">{r.label}</span>
                  {r.pct !== null && (
                    <span className="usage-bar">
                      <span
                        className={`usage-fill ${r.pct >= 80 ? "hot" : ""}`}
                        style={{ width: `${Math.min(100, r.pct)}%` }}
                      />
                    </span>
                  )}
                  <span className="usage-value">
                    {r.pct !== null ? `${r.pct}%` : ""}
                    {r.detail ? ` · ${r.detail}` : ""}
                    {r.reset ? ` · ${r.reset} 重置` : ""}
                  </span>
                </div>
              ))}
            </div>
          ) : (
            <div className="usage-status">{b.status}</div>
          )}
          {b.raw && (
            <details className="usage-raw">
              <summary>原始数据</summary>
              <pre>{b.raw}</pre>
            </details>
          )}
        </div>
      ))}
    </div>
  );
}

// ---------- 修复页 ----------

/** thread id 过长时截断展示，完整值在展开区可见。 */
function shortThreadId(id: string): string {
  return id.length > 16 ? `${id.slice(0, 16)}…` : id;
}

/** 修复记录页：可搜索的完整修复历史（原因 / 结果 / 明细 / 备份）。 */
function RepairLogPage({
  records,
  query,
  setQuery,
  onDelete,
  onClear,
}: {
  records: RepairRecord[] | null;
  query: string;
  setQuery: (v: string) => void;
  onDelete: (displayIndex: number) => void;
  onClear: () => void;
}) {
  const q = query.trim().toLowerCase();
  // i 是「最新在前」全量列表里的下标，删除时按它定位；过滤视图不能丢掉它。
  const filtered = (records ?? [])
    .map((r, i) => ({ r, i }))
    .filter(({ r }) => {
      if (!q) return true;
      return [r.title ?? "", r.project ?? "", r.threadId, r.outcome, ...r.details]
        .join("\n")
        .toLowerCase()
        .includes(q);
    });
  const kindLabel = (r: RepairRecord) => {
    const parts: string[] = [];
    if (r.kinds.entries) parts.push("坏条目");
    if (r.kinds.model) parts.push("模型");
    if (r.kinds.roots) parts.push("目录失效");
    return parts.join(" · ") || "—";
  };
  return (
    <div className="page-body">
      <div className="page-head">
        <h2>修复记录</h2>
      </div>
      <div className="repair-row">
        <input
          className="grow"
          value={query}
          placeholder="搜索任务名 / 项目 / thread id / 结果…"
          onChange={(e) => setQuery(e.target.value)}
        />
        {records !== null && records.length > 0 && (
          <button title="清空全部修复记录（不可恢复）" onClick={onClear}>
            清空记录
          </button>
        )}
      </div>
      {records === null && <p className="hint">读取中…</p>}
      {records !== null && filtered.length === 0 && (
        <p className="hint">{records.length === 0 ? "还没有修复记录。" : "没有匹配的记录。"}</p>
      )}
      {filtered.map(({ r, i }) => (
        <div className="card" key={`${r.threadId}-${i}`}>
          <div className="repair-row">
            <div className="grow">
              <div className="repair-task-name">
                {r.project ? `${r.project} › ` : ""}
                {r.title ?? shortThreadId(r.threadId)}
              </div>
              <div className="hint">
                {r.executedAt} · 原因：{kindLabel(r)} · thread {shortThreadId(r.threadId)}
              </div>
            </div>
            <span className={`repair-status ${r.outcome === "done" ? "status-ok" : "status-err"}`}>
              {r.outcome === "done" ? "修复成功" : "失败"}
            </span>
            <button className="link-btn" title="删除这条记录" onClick={() => onDelete(i)}>
              删除
            </button>
          </div>
          {r.details.map((d, j) => (
            <div className="repair-log" key={j}>
              {d}
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

/** 模型切换自动修复：队列可见、执行前可取消；排队/立即由 Codex 进程状态决定。 */
function AutoRepairSection({
  state,
  error,
  onCancel,
  onExecute,
  onOpenLog,
}: {
  state: AutoQueueState | null;
  error: string | null;
  onCancel: (threadId: string) => void;
  onExecute: () => void;
  onOpenLog: () => void;
}) {
  const queue = state?.queue ?? null;
  // 队列不变量：只含待执行项；执行结果（成功/失败）都在「记录」页。
  // 状态是小写 camelCase（serde rename_all），写成 "Pending" 会永远匹配不上。
  const pending = queue?.items.filter((i) => i.status === "pending") ?? [];
  const codexRunning = state?.codex_running ?? false;
  const executing = state?.executing ?? false;
  // 折叠摘要：条数多了默认收起，避免长列表把整页顶下去；用户显式展开/收起优先。
  const [expandedOverride, setExpandedOverride] = useState<boolean | null>(null);
  const expanded = expandedOverride ?? pending.length <= 5;
  const setExpanded = (v: boolean) => setExpandedOverride(v);
  return (
    <div className="card">
      <div className="card-title repair-queue-title">
        自动修复（模型切换）
        <button className="link-btn" onClick={onOpenLog}>
          查看修复记录 →
        </button>
      </div>
      <p className="hint">
        检测到模型切换时自动扫描并排队，Codex 退出后自动修复；
        只处理<b>有项目归属</b>的任务（散装会话与自动审查线程不动）。
        {queue?.targetModel ? ` 当前目标模型：${queue.targetModel}` : ""}
      </p>
      {pending.length > 0 && (
        <div className="repair-result">
          <div className="repair-file">
            <div className="repair-file-path">待执行 {pending.length} 个任务</div>
            <div className="repair-file-meta">
              Codex 退出后自动修复
              <button className="link-btn" onClick={() => setExpanded(!expanded)}>
                {expanded ? "收起" : "展开详情"}
              </button>
            </div>
          </div>
          {expanded &&
            pending.map((item) => (
              <div className="repair-file" key={item.threadId}>
                <div className="repair-file-path">
                  {item.project ? `${item.project} › ` : ""}
                  {item.title ?? shortThreadId(item.threadId)}
                </div>
                <div className="repair-file-meta">
                  待执行
                  <button className="link-btn" onClick={() => onCancel(item.threadId)}>
                    取消
                  </button>
                </div>
              </div>
            ))}
        </div>
      )}
      {pending.length === 0 && <p className="hint">当前没有排队任务。</p>}
      <p className="hint">执行结果（成功 / 失败及原因）见「修复记录」页。</p>
      {error && <div className="msg err">{error}</div>}
      {pending.length > 0 && (
        <div className="repair-row">
          <button
            className="primary"
            disabled={codexRunning || executing}
            onClick={onExecute}
          >
            {executing ? "执行中…" : `立即修复 (${pending.length})`}
          </button>
          {codexRunning && <span className="hint">Codex 正在运行，退出后自动执行</span>}
        </div>
      )}
    </div>
  );
}

/** 修复页（统一）：扫描 → 勾选 → 一次修复所有适用类型；排队/立即由 Codex 进程状态决定。 */
function RepairPage({
  autoQueue,
  autoQueueError,
  onAutoCancel,
  onAutoExecute,
  onOpenLog,
  input,
  setInput,
  tasks,
  selected,
  busy,
  error,
  scannedAt,
  defaultModel,
  onScan,
  onToggle,
  onToggleAll,
  onRepair,
  scanning,
  repairProgress,
}: {
  autoQueue: AutoQueueState | null;
  autoQueueError: string | null;
  onAutoCancel: (threadId: string) => void;
  onAutoExecute: () => void;
  onOpenLog: () => void;
  input: string;
  setInput: (v: string) => void;
  tasks: UnifiedTask[] | null;
  selected: string[];
  busy: boolean;
  error: string | null;
  scannedAt: string | null;
  defaultModel: string | null;
  onScan: (single: boolean) => void;
  onToggle: (threadId: string) => void;
  onToggleAll: () => void;
  onRepair: () => void;
  scanning: boolean;
  repairProgress: { settled: number; total: number } | null;
}) {
  const codexRunning = autoQueue?.codex_running ?? false;
  const queueItems = autoQueue?.queue?.items ?? [];
  const statusOf = (threadId: string): AutoQueueItem["status"] | null =>
    queueItems.find((i) => i.threadId === threadId)?.status ?? null;
  // 仅含坏条目的任务不可选：内容形态问题由代理出口实时改写覆盖
  // （2026-10-07 方针），离线修复只做元数据类（模型/目录/血缘）。
  const actionable = (t: UnifiedTask) =>
    Boolean(t.modelMismatch) || t.staleRoots.length > 0 || t.lineageBreaks.length > 0;
  const selectable = (t: UnifiedTask) => {
    const s = statusOf(t.threadId);
    return actionable(t) && s !== "pending" && s !== "done";
  };
  const groups = (tasks ?? []).reduce<{ project: string; rows: UnifiedTask[] }[]>((acc, t) => {
    const key = t.project ?? "（无项目）";
    const found = acc.find((g) => g.project === key);
    if (found) {
      found.rows.push(t);
    } else {
      acc.push({ project: key, rows: [t] });
    }
    return acc;
  }, []);
  const allChecked = tasks !== null && tasks.length > 0 && selected.length === tasks.length;

  return (
    <div className="page-body">
      <div className="page-head">
        <h2>任务修复</h2>
      </div>

      {scanning && (
        <div className="modal-overlay">
          <div className="modal-card">
            <div className="modal-title">正在扫描任务…</div>
            <div className="progress-track">
              <div className="progress-bar indeterminate" />
            </div>
            <div className="hint">遍历全部 rollout 分片，数量多时需要十几秒</div>
          </div>
        </div>
      )}

      {repairProgress && (
        <div className="modal-overlay">
          <div className="modal-card">
            <div className="modal-title">
              正在修复 {repairProgress.settled} / {repairProgress.total}
            </div>
            <div className="progress-track">
              <div
                className="progress-bar"
                style={{
                  width: `${repairProgress.total === 0 ? 0 : Math.round((repairProgress.settled / repairProgress.total) * 100)}%`,
                }}
              />
            </div>
            <div className="hint">完成一项即从列表移除；失败项会保留并显示原因</div>
          </div>
        </div>
      )}

      <div className={`repair-banner ${codexRunning ? "warn" : "ok"}`}>
        <div className="repair-banner-text">
          <strong>{codexRunning ? "Codex 正在运行" : "Codex 未运行"}</strong>
          <span>{codexRunning ? "修复将进入队列，Codex 退出后自动执行" : "可以立即执行修复"}</span>
        </div>
      </div>

      <AutoRepairSection
        state={autoQueue}
        error={autoQueueError}
        onCancel={onAutoCancel}
        onExecute={onAutoExecute}
        onOpenLog={onOpenLog}
      />

      <div className="card">
        <div className="card-title">扫描范围</div>
        <div className="repair-row">
          <input
            className="grow"
            value={input}
            placeholder="粘贴深链 codex://threads/… 或 thread id，只扫描这一个任务"
            onChange={(e) => setInput(e.target.value)}
          />
          <button disabled={busy || !input.trim()} onClick={() => onScan(true)}>
            扫描此任务
          </button>
        </div>
        <div className="repair-divider">
          <span>或</span>
        </div>
        <div className="repair-row">
          <button disabled={busy} onClick={() => onScan(false)}>
            {busy ? "处理中…" : "扫描全部任务"}
          </button>
          <span className="hint">
            检测到模型切换时自动扫描并排队，Codex 退出后自动修复（仅限有项目归属的任务）
          </span>
        </div>
      </div>

      {error && <div className="msg err">{error}</div>}

      {tasks !== null && tasks.length === 0 && (
        <div className="card">
          <div className="msg ok">没有待修复的问题{scannedAt ? `（上次扫描 ${scannedAt}）` : ""}</div>
        </div>
      )}

      {tasks !== null && tasks.length > 0 && (
        <div className="card">
          <div className="repair-row">
            <label className="repair-check">
              <input type="checkbox" checked={allChecked} onChange={onToggleAll} /> 全选
            </label>
            <span className="hint grow">
              已选 {selected.length} / {tasks.filter(selectable).length} 个待修复
              {defaultModel ? ` · 目标模型 ${defaultModel}` : ""}
            </span>
            <button className="primary" disabled={busy || selected.length === 0} onClick={onRepair}>
              {codexRunning ? `排队修复所选 (${selected.length})` : `修复所选 (${selected.length})`}
            </button>
          </div>
          <div className="hint">
            {scannedAt ? `上次扫描 ${scannedAt} · ` : ""}共 {tasks.length} 个任务存在问题 ·
            所有可离线修复的问题一次处理
          </div>
          {groups.map((g) => (
            <div key={g.project}>
              <div className="repair-group">
                {g.project}
                <span className="repair-group-count">
                  {g.rows.filter((r) => selectable(r)).length} 个待处理
                </span>
              </div>
              {g.rows.map((t) => {
                const s = statusOf(t.threadId);
                const queued = s === "pending";
                return (
                  <div className="repair-file" key={t.threadId}>
                    <div className="repair-row">
                      <span className="repair-check">
                        {!queued && (
                          <input
                            type="checkbox"
                            checked={selected.includes(t.threadId)}
                            onChange={() => onToggle(t.threadId)}
                          />
                        )}
                      </span>
                      <div className="grow">
                        <div className="repair-task-name">{t.title ?? shortThreadId(t.threadId)}</div>
                        <div className="repair-chips">
                          {t.badEntries > 0 && (
                            <span className="chip" title="对话内容形态问题，由代理出口实时改写覆盖，无需离线修复">
                              <span className="chip-dot protected" />
                              实时保护 ×{t.badEntries}
                            </span>
                          )}
                          {t.modelMismatch && (
                            <span className="chip">
                              <span className="chip-dot model" />
                              模型不匹配
                            </span>
                          )}
                          {t.staleRoots.length > 0 && (
                            <span className="chip">
                              <span className="chip-dot cwd" />
                              目录失效
                            </span>
                          )}
                          {t.lineageBreaks.length > 0 && (
                            <span className="chip">
                              <span className="chip-dot lineage" />
                              血缘断链 ×{t.lineageBreaks.length}
                            </span>
                          )}
                        </div>
                      </div>
                      <div className="repair-status">
                        {queued && <span className="status-queue">已排队 · 退出后自动执行</span>}
                        {!queued && (
                          <span className={actionable(t) ? "status-idle" : "status-protected"}>
                            {actionable(t) ? "待修复" : "实时保护中"}
                          </span>
                        )}
                      </div>
                    </div>
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
function AboutPage({
  appInfo,
  busy,
  message,
  onCheck,
  onDownload,
  onInstall,
  onOpenExternal,
}: {
  appInfo: AppInfo | null;
  busy: boolean;
  message: string | null;
  onCheck: () => void;
  onDownload: () => void;
  onInstall: () => void;
  onOpenExternal: (url: string) => void;
}) {
  const info = appInfo ?? {
    version: "1.0.1",
    product: "Halcyon",
    repository: "weyham/halcyon",
    update_source: "GitHub Releases（公开仓）",
    update_status: "未检查",
    update_note: "更新器仍处于安全接入阶段，当前只执行状态检查。",
    available_version: null,
    release_url: null,
  };
  return (
    <div className="page-body">
      <div className="page-head">
        <div>
          <h2>{info.product}</h2>
          <p className="page-subtitle">Codex 任务通信代理</p>
        </div>
        <span className={`status-chip ${info.update_status === "检查失败" ? "error" : info.update_status === "已是最新" ? "ok" : ""}`}>
          {info.update_status}
        </span>
      </div>
      <section className="about-hero card">
        <div className="about-mark">H</div>
        <div>
          <div className="about-title">Halcyon</div>
          <div className="hint">轻量、本地、可审计的 Codex API 中转与任务修复工具</div>
        </div>
      </section>
      <div className="about-grid">
        <div className="card about-item"><span>当前版本</span><strong>{info.version}</strong></div>
        <div className="card about-item">
          <span>GitHub 项目</span>
          <button
            className="link-btn about-repo"
            title="打开仓库页面"
            onClick={() => onOpenExternal(`https://github.com/${info.repository}`)}
          >
            {info.repository}
          </button>
        </div>
      </div>
      <section className="card about-update">
        <div className="card-title">检查更新</div>
        <p className="hint">公开仓 GitHub Releases 更新通道：匿名读取，无需任何授权。</p>
        <div className="about-actions">
          <button className="primary" disabled={busy} onClick={onCheck}>{busy ? "检查中…" : "检查更新"}</button>
          {info.update_status === "有更新" && <button className="secondary" disabled={busy} onClick={onDownload}>下载并校验</button>}
          {info.update_status === "待安装" && <button className="primary" disabled={busy} onClick={onInstall}>安装并重启</button>}
          {message && <span className="msg ok">{message}</span>}
        </div>
        {info.available_version && <p className="hint about-note">可用版本：{info.available_version}</p>}
      </section>
    </div>
  );
}

// ---------- 设置页 ----------

function SettingsPage({
  rows,
  updateRow,
  setRows,
  base,
  setBase,
  addr,
  saving,
  message,
  runtimeInfo,
  runtimeError,
  onSave,
  onEditUsage,
}: {
  rows: RouteRow[];
  updateRow: (i: number, patch: Partial<RouteRow>) => void;
  setRows: (fn: (rs: RouteRow[]) => RouteRow[]) => void;
  base: ShimConfig;
  setBase: (fn: (b: ShimConfig) => ShimConfig) => void;
  addr: string | null | undefined;
  saving: boolean;
  message: { kind: "ok" | "err"; text: string } | null;
  runtimeInfo: RuntimeInfo | null;
  runtimeError: string | null;
  onSave: () => void;
  onEditUsage: (i: number) => void;
}) {
  return (
    <div className="page-body">
      <div className="page-head">
        <h2>设置</h2>
      </div>

      <section className="listen-row">
        <label>
          监听地址
          <input value={base.listen} onChange={(e) => setBase((b) => ({ ...b, listen: e.target.value }))} />
        </label>
      </section>

      <div className="routes-head">
        <h3>路由映射</h3>
        <button
          onClick={() =>
            setRows((rs) => [...rs, { name: "", upstream: "", aliases: "", web_search: "note", balance: null }])
          }
        >
          + 添加路由
        </button>
      </div>

      {rows.length === 0 && <p className="hint">还没有路由。添加一条，例如 kimi → https://api.kimi.com/coding/v1</p>}

      {rows.map((row, i) => (
        <div className="card" key={i}>
          <div className="field-row">
            <label>
              名称
              <input value={row.name} placeholder="kimi" onChange={(e) => updateRow(i, { name: e.target.value })} />
            </label>
            <label className="grow">
              上游地址
              <input
                value={row.upstream}
                placeholder="https://api.kimi.com/coding/v1"
                onChange={(e) => updateRow(i, { upstream: e.target.value })}
              />
            </label>
            <button className="danger" title="删除这条路由" onClick={() => setRows((rs) => rs.filter((_, idx) => idx !== i))}>
              删除
            </button>
          </div>
          <div className="field-row">
            <label className="grow">
              别名（逗号分隔，可选）
              <input
                value={row.aliases}
                placeholder="/kimi/v1"
                onChange={(e) => updateRow(i, { aliases: e.target.value })}
              />
            </label>
            <label>
              web_search
              <select value={row.web_search} onChange={(e) => updateRow(i, { web_search: e.target.value })}>
                <option value="note">note</option>
                <option value="drop">drop</option>
                <option value="normalize">normalize</option>
              </select>
            </label>
          </div>
          <div className="field-row usage-cfg-row">
            <button onClick={() => onEditUsage(i)}>用量查询</button>
            <span className="hint">{balanceSummary(row.balance)}</span>
          </div>
          {addr && row.name.trim() && <div className="local-addr">本地地址：http://{addr}/{row.name.trim()}</div>}
        </div>
      ))}

      <div className="settings-actions">
        {message && <div className={`msg ${message.kind}`}>{message.text}</div>}
        <button className="primary" disabled={saving || rows.length === 0} onClick={onSave}>
          {saving ? "保存中…" : "保存并重启代理"}
        </button>
      </div>

      <section className="card runtime-card">
        <div className="card-title">运行环境</div>
        {runtimeError !== null ? (
          <div className="msg err">无法读取运行环境：{runtimeError}</div>
        ) : runtimeInfo ? (
          <>
            <div className="runtime-row">
              <span className="runtime-label">运行形态</span>
              <span className="runtime-value">{runtimeInfo.mode === "portable" ? "便携版" : "安装版"}</span>
            </div>
            <div className="runtime-row">
              <span className="runtime-label">数据目录</span>
              <span className="runtime-path">{runtimeInfo.data_dir || "-"}</span>
            </div>
            <div className="runtime-row">
              <span className="runtime-label">配置文件</span>
              <span className="runtime-path">{runtimeInfo.config_path || "-"}</span>
            </div>
            {runtimeInfo.mode === "portable" && (
              <p className="hint">data\ 文件夹是便携模式标记，请勿删除——删除后会切换到安装版数据目录。</p>
            )}
          </>
        ) : (
          <div className="hint">读取中…</div>
        )}
      </section>
    </div>
  );
}
