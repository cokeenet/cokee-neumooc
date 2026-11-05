/**
 * 调度面板：为每个账号配置**每日检查**时间。
 *
 * 调度只负责「发现」——今天有没有新资源要刷、有没有新作业要写；
 * 要不要刷由人手动触发（刷课随时可以再跑，钉死在某个时刻没有意义）。
 *
 * 服务内调度器（非 cron）：计划落库 + 乐观锁抢占 + 崩溃恢复。
 * 时区统一按北京时间（UTC+8）—— 容器是 UTC，直接用本地时间会错 8 小时。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import { MSG_CLASS } from '../lib/msg';
import { api } from '../lib/api';
import { useResource } from '../lib/useResource';
import { useCall, type Msg } from '../lib/useCall';
import EmptyState from 'korikura-webui/components/ui/EmptyState';
import ErrorState from 'korikura-webui/components/ui/ErrorState';

interface Plan {
  id: number; biz_date: string; fire_at: number; state: string; detail: string;
}
interface SchedRow {
  account_id: number;
  label: string;
  enabled: boolean;
  time: string;
  args?: Record<string, unknown>;
  today_plan: Plan | null;
  /** 最近一次成功的「每日检查」结果 —— 调度页原先看不到自己配的东西查出什么 */
  last_check: {
    at: number; biz_date: string;
    new_resources: number | null; todo_resources: number | null;
    pending_tasks: number | null; task_error: string | null;
  } | null;
}

const PLAN_STATE: Record<string, string> = {
  pending: '等待', running: '进行中', done: '已完成', failed: '失败', skipped: '跳过',
};

/** epoch ms → 北京时间 HH:MM */
function beijingTime(ms: number): string {
  return new Date(ms).toLocaleTimeString('zh-CN', {
    timeZone: 'Asia/Shanghai', hour: '2-digit', minute: '2-digit', hour12: false,
  });
}

interface NotifyStatus {
  enabled: boolean; mode: string; configured: boolean;
  pending: number; summary_at: string;
}

export default function SchedulerPanel() {
  const [draft, setDraft] = useState<Record<number, { time: string; enabled: boolean }>>({});

  // 一个 useResource 取代 loading/rows/nt/load/useEffect 五件套。
  // draft 是**编辑中的副本**（用户改了还没保存），所以留在本地。
  const load = useCallback(async () => {
    const [rows, nt] = await Promise.all([
      api.schedule(),
      api.notifyStatus().catch(() => null),
    ]);
    return { rows, nt };
  }, []);
  // error 必须解构 —— 丢弃它会把「后端挂了」显示成「还没检查过」
  const { data, loading, error, reload } = useResource(load);
  // ⚠️ 重构时漏了这一行 —— `msg` / `busy` / `call` 都没声明，
  // 而构建**照样通过**（TS 在这个文件里没开严格检查），
  // 直到 SSR 渲染时抛 `ReferenceError: msg is not defined`。
  //
  // 更阴的是：错误发生在**流的中途**，页面已经吐了一半 HTML，
  // 所以浏览器看到的是 `ERR_INCOMPLETE_CHUNKED_ENCODING` 而不是干净的 500，
  // curl 甚至返回 200。日志在 /var/log/neumooc-frontend.log，不在 journal 里。
  const { call, busy, msg } = useCall(reload);
  // ⚠️ `?? []` **每次渲染都造一个新数组** —— 拿它当 useEffect 的依赖会变成
  // 死循环：setDraft → 重渲染 → 新数组 → effect 再跑 → …
  // SSR 下表现为整个页面 `Internal server error`。
  // 所以用 useMemo 锁住引用（依赖是 data，它只在加载完成时变一次）。
  const rows = useMemo(() => data?.rows ?? [], [data]);
  const nt = data?.nt ?? null;

  // 数据回来后初始化草稿（只在还没有草稿时）
  useEffect(() => {
    setDraft((prev) =>
      Object.keys(prev).length
        ? prev
        : Object.fromEntries(rows.map((x) => [x.account_id, {
            time: x.time || '08:00', enabled: !!x.enabled,
          }])),
    );
  }, [rows]);

  function patch(id: number, k: 'time' | 'enabled', v: unknown) {
    setDraft((p) => ({ ...p, [id]: { ...p[id], [k]: v } }));
  }

  async function save(id: number) {
    const d = draft[id];
    await call(`/api/schedule/${id}`, {
      method: 'PUT',
      body: JSON.stringify({ enabled: d.enabled, time: d.time }),
    }, '调度已保存');
  }

  async function runNow(id: number) {
    await call(`/api/schedule/${id}/run-now`, { method: 'POST' }, '已触发一次检查');
  }

  return (
    <>
      {msg && <div className={`msg-bar ${MSG_CLASS[msg.kind]}`}>{msg.text}</div>}

      <div className="card">
        <h2 className="card-title">
          每日检查
          <span className="card-meta">按北京时间（UTC+8）</span>
        </h2>
        <p className="muted small" style={{ marginTop: -4 }}>
          到点自动检查一次：有没有新上传的课程资源要刷、有没有新作业要写。
          <strong>只检查，不自动刷</strong> —— 要不要刷你来点「刷课」。
          多个账号会依次错开几秒，避免同时请求服务端。
        </p>

        {/* 通知通道：配错了会**静默不工作**，所以必须能自证状态 */}
        {nt && (
          <div className="row" style={{ gap: 8, alignItems: 'center', marginBottom: 12 }}>
            <span className="muted small">考勤提醒：</span>
            {nt.enabled ? (
              <>
                <span className="badge badge-ok">已开启</span>
                <span className="muted small">
                  {nt.mode === 'each' ? '逐条推送' : '失败立即推、成功每日汇总'}
                  {(nt.pending ?? 0) > 0 ? ` · 待汇总 ${nt.pending} 条` : ''}
                  {` · 汇总 ${nt.summary_at}`}
                </span>
                <button className="btn btn-sm" disabled={busy}
                  onClick={async () => {
                    const r = await call('/api/notify/test', { method: 'POST' }, '测试消息已发送，去群里看看');
                    // 原来是 `setNt({...nt, pending: nt.pending})` —— 值没变，
                    // 是个**空操作**。改成真的刷新（发送后 pending 可能变了）。
                    if (r.ok) void reload();
                  }}>
                  发测试消息
                </button>
              </>
            ) : (
              <>
                <span className="badge badge-muted">未开启</span>
                <span className="muted small">
                  {nt.configured
                    ? '已配置凭据但模式为 off'
                    : '需要设置 NEUMOOC_QQ_BOT_ID / NEUMOOC_QQ_TARGET_ID'}
                  ，签到窗口开启时不会有任何提醒
                </span>
              </>
            )}
          </div>
        )}

        {loading ? (
          <p className="muted">加载中…</p>
        ) : error ? (
          <div>
            <ErrorState reason={`取不到调度数据：${error}`} />
            <p className="muted small" style={{ textAlign: 'center', marginTop: 8 }}>
              这是<strong>取不到数据</strong>，不是「没有账号」。
            </p>
            <div style={{ textAlign: 'center', marginTop: 8 }}>
              <button className="btn btn-sm" onClick={() => void reload()}>重试</button>
            </div>
          </div>
        ) : rows.length === 0 ? (
          <EmptyState
            title="还没有账号"
            desc="添加账号后，就能在这里设置每天自动检查的时间。"
            href="/accounts"
            action="前往「账号」添加"
          />
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>账号</th><th>启用</th><th>检查时间</th>
                  <th>今日检查</th><th>上次结果</th><th>操作</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => {
                  const d = draft[r.account_id] ?? { time: r.time, enabled: r.enabled };
                  const plan = r.today_plan;
                  return (
                    <tr key={r.account_id}>
                      <td>{r.label}</td>
                      <td>
                        <input type="checkbox" checked={d.enabled}
                          onChange={(e) => patch(r.account_id, 'enabled', e.target.checked)} />
                      </td>
                      <td>
                        <input className="input" style={{ width: 92 }} type="time" value={d.time}
                          onChange={(e) => patch(r.account_id, 'time', e.target.value)} />
                      </td>
                      <td>
                        {plan ? (
                          <>
                            <span className={`badge ${plan.state === 'done' ? 'badge-ok'
                              : plan.state === 'failed' ? 'badge-fail'
                              : plan.state === 'running' ? 'badge-run' : 'badge-muted'}`}>
                              {PLAN_STATE[plan.state] ?? plan.state}
                            </span>
                            <span className="muted small" style={{ marginLeft: 6 }}>
                              {beijingTime(plan.fire_at)}
                            </span>
                          </>
                        ) : (
                          <span className="muted small">未生成</span>
                        )}
                      </td>
                      <td>
                        {(() => {
                          const lc = r.last_check;
                          if (!lc) return <span className="muted small">还没检查过</span>;
                          const nw = lc.new_resources ?? 0;
                          const todo = lc.todo_resources ?? 0;
                          const tasks = lc.pending_tasks ?? 0;
                          return (
                            <>
                              {nw > 0 ? (
                                <span className="badge badge-warn">新资源 {nw}</span>
                              ) : todo === 0 && tasks === 0 ? (
                                <span className="badge badge-ok">无新增</span>
                              ) : (
                                <span className="badge badge-muted">待完成 {todo}</span>
                              )}
                              {tasks > 0 && (
                                <span className="badge badge-warn" style={{ marginLeft: 4 }}>
                                  作业 {tasks}
                                </span>
                              )}
                              <div className="sub">
                                {lc.biz_date} · 待完成 {todo}
                                {lc.task_error ? ' · 作业情况查不到' : ''}
                              </div>
                            </>
                          );
                        })()}
                      </td>
                      <td>
                        <div className="row" style={{ gap: 6 }}>
                          <button className="btn btn-sm btn-primary" disabled={busy}
                            onClick={() => void save(r.account_id)}>保存</button>
                          <button className="btn btn-sm" disabled={busy}
                            onClick={() => void runNow(r.account_id)}>立即检查</button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </>
  );
}
