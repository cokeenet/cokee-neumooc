/**
 * 考勤面板。
 *
 * 对齐 同类平台 的约定（见 UI-SPEC.md）：
 *   · 不用原生 confirm/prompt/alert
 *   · 变更请求走 useCall（永不抛、优先服务端文案、成功后刷新）
 *   · 状态只用四种徽标；幂等命中是 warn 不是 err
 *   · `.badge` 做消息条时要块级化
 *
 * 进行中的场次可直接在列表里提交签到；定位场次需填经纬度
 * （或点「用当前位置」调浏览器 Geolocation）。
 */
import { useCallback, useEffect, useState } from 'react';
import { MSG_CLASS } from '../lib/msg';
import { useResource } from '../lib/useResource';
import { api, type Account, type AttendanceItem, type AttendanceResp } from '../lib/api';
import { useCall, type Msg } from '../lib/useCall';
import SignDialog from './SignDialog';
import ErrorState from 'korikura-webui/components/ui/ErrorState';
import { InlineConfirm } from 'korikura-webui/components/InlineDialog';
import { fmtTime } from '../lib/labels';


interface Summary {
  total: number; signed: number; missed: number;
  pending: number; upcoming: number; rate: number;
  by_type: Record<string, number>;
}

interface Payload {
  items: AttendanceItem[];
  summary: Summary;
  terms: string[];
  pending_items: AttendanceItem[];
}

const TYPE_LABEL: Record<number, string> = { 0: '签到', 1: '二维码', 2: '教师点名' };

function fmt(ms: number): string {
  if (!ms) return '-';
  return new Date(ms).toLocaleString('zh-CN', {
    timeZone: 'Asia/Shanghai',
    month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', hour12: false,
  });
}

function isSigned(a: AttendanceItem): boolean {
  return a.sign_role === 3 || a.sign_role === 4 || !!a.sign_time;
}

function isOpen(a: AttendanceItem): boolean {
  const now = Date.now();
  if (a.finish_time && now > a.finish_time) return false;
  if (a.open_time && now < a.open_time) return false;
  return a.status === null || a.status === 1;
}

/**
 * 后端返回的 `terms` 可能有两种形态：老版本给的是纯 term_id 字符串，
 * 新版本给 `{id, name}`。这里统一成 id + 可读标签，
 * 避免把雪花 ID 直接摆到用户面前。
 */
function termId(t: unknown): string {
  return typeof t === 'string' ? t : String((t as { id?: string })?.id ?? '');
}
function termLabel(t: unknown): string {
  if (typeof t === 'object' && t) {
    const o = t as { name?: string; id?: string };
    if (o.name) return o.name;
  }
  return '（未命名学期）';
}

/** 明细表筛选用：已签到 / 缺勤 / 未签到（未开始或进行中） */
type RowFilter = 'all' | 'pending' | 'signed' | 'missed';

const FILTERS: { key: RowFilter; label: string }[] = [
  { key: 'all', label: '全部' },
  { key: 'pending', label: '未签到' },
  { key: 'signed', label: '已签到' },
  { key: 'missed', label: '缺勤' },
];

function classify(a: AttendanceItem, now: number): Exclude<RowFilter, 'all'> {
  if (isSigned(a)) return 'signed';
  if (a.finish_time && now > a.finish_time) return 'missed';
  return 'pending';
}


export default function AttendancePanel({ accountId }: { accountId: number }) {
  const [term, setTerm] = useState('');
  const [filter, setFilter] = useState<RowFilter>('all');
  const [tick, setTick] = useState(0);
  /** 上次手动刷新的时刻 —— 让「刷新」有个可确认的反馈 */
  const [refreshedAt, setRefreshedAt] = useState(0);
  // 一个 useResource 取代 loading/data/autoSign/load/useEffect 五件套。
  // 原先这里是手写 loading + **两处裸 fetch**（绕过 api.ts）+
  // **空 catch**（`catch { setData(null) }`）—— 后端一挂就显示
  // 「还没有考勤数据。点「同步考勤」拉取。」，把故障说成用户没数据。
  const load = useCallback(async () => {
    const [att, auto, devCfg, accts] = await Promise.all([
      api.attendance(accountId, term),
      // 自动签到状态拿不到不影响考勤列表，不让它把整页拖成错误
      api.autoSign(accountId).catch(() => null),
      // 设备 ID 的全局急停开关
      api.deviceConfig().catch(() => null),
      // 本账号的 device_id_enabled —— 从账号列表里取（已有接口，不新增）
      api.accounts().catch(() => [] as Account[]),
    ]);
    const me = accts.find((a) => a.id === accountId) ?? null;
    return { att, auto, devCfg, me };
  }, [accountId, term]);
  const { data: loaded, loading, error, reload } = useResource(load);
  const data: AttendanceResp | null = loaded?.att ?? null;
  const autoSign = loaded?.auto ?? null;
  /** 签到是否带 deviceId（单账号开关） */
  const deviceOn = loaded?.me?.device_id_enabled ?? false;
  /** 平台全局急停（NEUMOOC_DEVICE_ID） */
  const deviceMasterOn = loaded?.devCfg?.master_on ?? true;
  // ⚠️ 这一行必须留着 —— 重构时把 `useCall(load)` 那段整块替换掉了，
  // `call`/`busy`/`msg` 全没了。**构建能过**（TS 在这个文件里没拦住），
  // 到运行时才抛 `busy is not defined`。
  const { call, busy, msg } = useCall(reload);

  // 秒级刷新「剩余时间」，让倒计时动起来
  useEffect(() => {
    const t = setInterval(() => setTick((x) => x + 1), 1000);
    return () => clearInterval(t);
  }, []);

  /**
   * 有「进行中」场次时定期回查。
   *
   * ⚠️ 原先只有上面那个 1 秒定时器，它**只改显示、不回查数据**。后果：
   *   · 签到窗口已经关闭，页面仍显示「进行中」并留着可点的「签到」按钮；
   *   · 新开的场次不会出现，用户得手动刷新才知道。
   * 签到是唯一时间敏感的功能，界面必须跟着服务端走。
   *
   * 只在进行中才轮询 —— 没有窗口时不必打扰。
   */
  useEffect(() => {
    const hasOpen = (data?.pending_items ?? []).some(isOpen);
    if (!hasOpen) return;
    const t = setInterval(() => { void load(); }, 20_000);
    return () => clearInterval(t);
  }, [data, load]);

  /** 已结束场次的补签：临时重开 → 补签 → 还原。 */
  async function onMakeup(a: AttendanceItem) {
    await call(
      `/api/accounts/${accountId}/force-checkin`,
      {
        method: 'POST',
        body: JSON.stringify({
          attendance_id: a.attendance_id,
          detail_id: a.detail_id,
          reopen_ended: true,
          reopen_seconds: 60,
        }),
      },
      `已补签：${a.title}`,
    );
    await load();
  }

  async function onSign(payload: Record<string, unknown>) {
    // 返回值交给 SignDialog：失败时它要保持面板打开并就地报错
    return call(
      `/api/accounts/${accountId}/attendance/sign`,
      { method: 'POST', body: JSON.stringify(payload) },
      '签到提交成功',
    );
  }

  async function onSync() {
    const r = await call(`/api/accounts/${accountId}/attendance/sync`, {
      method: 'POST',
      body: JSON.stringify({ term_id: term }),
    }, '考勤已同步');
    // 成功后记下时刻 —— 替代原来那个「刷新」按钮的反馈作用。
    // 注意 `useCall` 的 `onDone` 已经触发过 reload 了（后端现在**等结果**，
    // 所以那次 reload 读到的就是新数据）。
    if (r.ok) setRefreshedAt(Date.now());
  }

  const pending = (data?.pending_items ?? []).filter(isOpen);
  const s = data?.summary;
  const now = Date.now();
  const rows = (data?.items ?? []).filter((a) => filter === 'all' || classify(a, now) === filter);

  return (
    <>

    <>
      {msg && <div className={`msg-bar ${MSG_CLASS[msg.kind]}`}>{msg.text}</div>}


      {pending.length > 0 && (
        <div className="card" style={{ borderColor: 'var(--warn)' }}>
          <h2 className="card-title">
            ⚠️ 正在签到
            <span className="card-meta">共 {pending.length} 场</span>
          </h2>
          <p className="muted small" style={{ marginTop: -4 }}>
            下面这些场次<strong>窗口已开且你还没签</strong>，可直接在「操作」列签到。
          </p>
          {pending.map((a) => {
            const left = a.finish_time ? Math.max(0, Math.floor((a.finish_time - now) / 1000)) : 0;
            return (
              <div key={a.detail_id} className="row" style={{ marginTop: 8, gap: 12 }}>
                <span className="badge badge-run">{TYPE_LABEL[a.attendance_type] ?? '考勤'}</span>
                <strong>{a.title}</strong>
                <span className="muted small">{a.course_name}</span>
                {a.location_type === 1 && <span className="badge badge-warn">需现场定位</span>}
                <span className="card-meta" style={{ marginLeft: 'auto' }}>
                  剩余 <strong>{Math.floor(left / 60)}:{String(left % 60).padStart(2, '0')}</strong>
                </span>
                <SignDialog
                  disabled={busy}
                  target={{
                    detail_id: a.detail_id,
                    attendance_id: a.attendance_id ?? '',
                    title: a.title,
                    attendance_type: a.attendance_type,
                    location_type: a.location_type,
                    address_name: a.address_name ?? '',
                    longitude: a.longitude,
                    latitude: a.latitude,
                    radius: a.radius,
                  }}
                  onSubmit={onSign}
                />
              </div>
            );
          })}
        </div>
      )}

      {/* 统计卡直接铺在页面流里。
          ⚠️ 上一轮拍平只做了一半：外层 stat-grid 里还留着旧的内层 stat-grid，
          而 JSX fragment 不产生 DOM 节点 —— 外层网格的直接子元素变成了
          【内层网格 + 类型分布段落】两个，被排成两列，内层只有 ~680px 宽，
          4 张卡挤成 3+1 换行、类型分布悬在右边。现在条件分支放在网格**外面**，
          网格里永远只有 4 张卡。 */}
      {loading ? (
        <div className="stat-grid mt-24">
          {['总场次', '已签', '缺勤', '出勤率'].map((k) => (
            <div key={k} className="card stat"><div className="stat-value muted">…</div><div className="stat-label">{k}</div></div>
          ))}
        </div>
      ) : error ? (
        // 「取不到数据」与「没有数据」必须分开
        <div className="card mt-24">
          <ErrorState reason={`取不到考勤数据：${error}`} />
          <p className="muted small" style={{ textAlign: 'center', marginTop: 8 }}>
            这是<strong>取不到数据</strong>，不是「没有考勤」。
          </p>
          <div style={{ textAlign: 'center', marginTop: 8 }}>
            <button className="btn btn-sm" onClick={() => void reload()}>重试</button>
          </div>
        </div>
      ) : !s || s.total === 0 ? (
        <div className="card mt-24">
          <p className="muted" style={{ margin: 0 }}>
            还没有考勤数据。{data?.terms.length ? '' : '点「同步考勤」拉取。'}
          </p>
        </div>
      ) : (
        <>
          <div className="stat-grid mt-24" style={{ marginBottom: 0 }}>
            <div className="card stat"><div className="stat-value">{s.total}</div><div className="stat-label">总场次</div></div>
            <div className="card stat"><div className="stat-value">{s.signed}</div><div className="stat-label">已签</div></div>
            <div className="card stat"><div className="stat-value" style={{ color: s.missed ? 'var(--danger)' : undefined }}>{s.missed}</div><div className="stat-label">缺勤</div></div>
            <div className="card stat"><div className="stat-value">{s.rate}%</div><div className="stat-label">出勤率</div></div>
          </div>
          <p className="muted small" style={{ margin: '8px 0 0' }}>
            类型分布：{Object.entries(s.by_type).map(([k, v]) => `${k} ${v}`).join(' · ')}
          </p>
        </>
      )}

      <div className="card mt-24">
        <h2 className="card-title">
          场次明细
          <span className="card-meta">共 {data?.items.length ?? 0} 条</span>
          {/* 自动签到会**真的替学生提交考勤**，所以默认关。
              放在它作用的表上方（原先悬在卡片外，像一行孤儿文字）。 */}
          {/* 签到是否带 deviceId —— **显式开关**，不用「空值 = 不带」那种隐式约定。
              平台按「同一 deviceId 对应多个学号」判考勤异常；不带则不参与该判定。
              与 device_id 解耦：关掉不清空 ID，再打开还是原来那个。 */}
          <span className="row" style={{ gap: 6, marginLeft: 'auto', alignItems: 'center' }}>
            <span className="muted small" title="签到请求里是否携带 deviceId（模拟设备标识）">
              设备 ID
            </span>
            {deviceOn
              ? <span className="badge badge-ok">已开启</span>
              : <span className="badge badge-muted">已关闭</span>}
            <button
              className={`btn btn-sm${deviceOn ? '' : ' btn-primary'}`}
              disabled={busy || !deviceMasterOn}
              title={!deviceMasterOn
                ? '平台已全局关闭（NEUMOOC_DEVICE_ID=0）'
                : deviceOn
                  ? '点击关闭：签到不再携带 deviceId'
                  : '点击开启：会为本账号生成/复用一个 UUID 并随签到提交'}
              onClick={async () => {
                const r = await call(
                  `/api/accounts/${accountId}/device-id`,
                  { method: 'PUT', headers: { 'content-type': 'application/json' },
                    body: JSON.stringify({ enabled: !deviceOn }) },
                  !deviceOn ? '已开启设备 ID' : '已关闭设备 ID',
                );
                if (r.ok) await reload();
              }}>
              {deviceOn ? '关闭' : '开启'}
            </button>
          </span>

          {autoSign && (
            <span className="row" style={{ gap: 6, alignItems: 'center' }}>
              <span className="muted small">自动签到</span>
              {autoSign.enabled
                ? <span className="badge badge-ok">已开启</span>
                : <span className="badge badge-muted">已关闭</span>}
              <button
                className={`btn btn-sm${autoSign.enabled ? '' : ' btn-primary'}`}
                disabled={busy || !autoSign.available}
                title={autoSign.hint}
                onClick={async () => {
                  const next = !autoSign.enabled;
                  const r = await call(
                    `/api/accounts/${accountId}/auto-sign`,
                    { method: 'PUT', headers: { 'content-type': 'application/json' },
                      body: JSON.stringify({ enabled: next }) },
                    next ? '已开启自动签到' : '已关闭自动签到',
                  );
                  // 重新拉取而不是本地改字段：hint 与开关状态是绑定的
                  if (r.ok) await reload();
                }}>
                {autoSign.enabled ? '关闭' : '开启'}
              </button>
            </span>
          )}
        </h2>
        <div className="row" style={{ marginBottom: 12 }}>
          {/* 学期下拉原先直接把 term_id（雪花 ID）当标签显示。
              单个 term 是纯数字串，界面上一眼看不出那是哪个学期。 */}
          <select className="input" value={term} onChange={(e) => setTerm(e.target.value)}>
            <option value="">全部学期</option>
            {(data?.terms ?? []).map((t) => (
              <option key={termId(t)} value={termId(t)}>{termLabel(t)}</option>
            ))}
          </select>
          {/*
            原先这里是**两个按钮**：「同步考勤」（真去打上游）和「刷新」
            （只重读本地缓存）。

            用户报「两个按钮重复」—— 因为「刷新」读的缓存**不会因为点它而变化**，
            等于什么都没做。而「同步考勤」成功后本来就会重读一次。

            所以删掉「刷新」，只留一个按钮，并把它的语义说清楚。
          */}
          <button className="btn btn-primary" onClick={() => void onSync()} disabled={busy || loading}>
            {busy ? '同步中…' : '从平台同步考勤'}
          </button>
          {refreshedAt > 0 && (
            <span className="muted small">上次同步 {fmtTime(refreshedAt)}</span>
          )}
        </div>

        {loading ? (
          <p className="muted">加载中…</p>
          ) : error ? (
          // 「取不到数据」与「没有数据」必须分开
          <div>
            <ErrorState reason={`取不到考勤数据：${error}`} />
            <p className="muted small" style={{ textAlign: 'center', marginTop: 8 }}>
              这是<strong>取不到数据</strong>，不是「没有考勤」。
            </p>
            <div style={{ textAlign: 'center', marginTop: 8 }}>
              <button className="btn btn-sm" onClick={() => void reload()}>重试</button>
            </div>
          </div>
          ) : (data?.items.length ?? 0) === 0 ? (
          <p className="muted">暂无记录。点「同步考勤」拉取。</p>
        ) : (
          <>
            <div className="row" style={{ gap: 6, marginBottom: 12 }}>
              {FILTERS.map((f) => (
                <button
                  key={f.key}
                  className={`btn btn-sm${filter === f.key ? ' btn-primary' : ''}`}
                  onClick={() => setFilter(f.key)}
                >
                  {f.label}
                </button>
              ))}
            </div>

            {rows.length === 0 ? (
              <p className="muted">该筛选下暂无记录。</p>
            ) : (
              <div className="table-wrap" style={{ maxHeight: 420, overflowY: 'auto' }}>
                <table className="table">
                  <thead>
                    <tr>
                      <th>时间</th><th>标题</th><th>课程</th><th>类型</th>
                      <th>定位</th><th>状态</th><th>签到时间</th><th>操作</th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((a) => {
                      const signed = isSigned(a);
                      const open = isOpen(a);
                      const badge = signed ? 'badge-ok'
                        : open ? 'badge-run'
                        : a.open_time > now ? 'badge-muted' : 'badge-fail';
                      const text = signed ? '已签' : open ? '进行中' : a.open_time > now ? '未开始' : '缺勤';
                      return (
                        <tr key={a.detail_id}>
                          <td className="mono">{fmt(a.open_time)}</td>
                          <td>{a.title}</td>
                          <td className="muted small">{a.course_name}</td>
                          <td><span className="badge badge-muted">{TYPE_LABEL[a.attendance_type] ?? a.attendance_type}</span></td>
                          <td>{a.location_type === 1 ? <span className="badge badge-warn">需定位</span> : <span className="muted small">—</span>}</td>
                          <td><span className={`badge ${badge}`}>{text}</span></td>
                          <td className="mono small">{a.sign_time ? fmt(a.sign_time) : '-'}</td>
                          <td>
                            {signed ? (
                              <span className="muted small">—</span>
                            ) : open ? (
                              <SignDialog
                                disabled={busy}
                                target={{
                                  detail_id: a.detail_id,
                                  attendance_id: a.attendance_id ?? '',
                                  title: a.title,
                                  attendance_type: a.attendance_type,
                                  location_type: a.location_type,
                                  address_name: a.address_name ?? '',
                                  longitude: a.longitude,
                                  latitude: a.latitude,
                                  radius: a.radius,
                                }}
                                onSubmit={onSign}
                              />
                            ) : (
                              <InlineConfirm
                                label="补签"
                                question="该场已结束，补签需临时重开几十秒（此间对全班显示为进行中），补完立即还原。确定？"
                                confirmLabel="确认补签"
                                disabled={busy}
                                onConfirm={() => onMakeup(a)}
                              />
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </>
        )}
      </div>

    </>
    </>
  );
}
