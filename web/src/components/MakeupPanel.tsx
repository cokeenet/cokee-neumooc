/**
 * 补签面板。
 *
 * 两条路径：
 *   A. 学生自助补签（force-checkin）—— 用学生账号走教师端接口发现场次
 *      · 进行中的场次直接补
 *      · 已结束的需「临时重开」（会短暂让该场对全班显示为进行中，随后自动还原）
 *   B. 教师补签（teacher-makeup）—— 用教师账号为指定学生补
 *
 * 对齐 同类平台 约定：不用原生对话框；变更走 useCall；日志随响应回传。
 */
import { useCallback, useState } from 'react';
import { MSG_CLASS } from '../lib/msg';
import { useResource } from '../lib/useResource';
import { InlineConfirm } from 'korikura-webui/components/InlineDialog';
import { api } from '../lib/api';
import { useCall, type Msg } from '../lib/useCall';

interface Props { accountId: number }

interface LogLine { level: string; message: string }

/** 已同步的考勤场次 —— 用来代替「让用户手填原始 ID」 */
interface AttRow {
  detail_id: string; attendance_id: string;
  title: string; course_name: string; open_time: number;
}


const STATUS_OPTIONS = [
  { value: 1, label: '出勤' }, { value: 2, label: '缺勤' },
  { value: 3, label: '事假' }, { value: 4, label: '病假' },
  { value: 5, label: '迟到' }, { value: 6, label: '早退' },
];

export default function MakeupPanel({ accountId }: Props) {
  const [includeEnded, setIncludeEnded] = useState(true);
  // 默认 **0 = 不限**。默认 120 分钟时，昨天缺的场次会被静默过滤掉，
  // 用户勾了「临时重开」也签不上，只会看到「没有需要补签的场次」。
  const [endedWithin, setEndedWithin] = useState(0);
  const [reopenEnded, setReopenEnded] = useState(false);
  const [reopenSeconds, setReopenSeconds] = useState(60);
  const [signStatus, setSignStatus] = useState(1);
  const [lon, setLon] = useState('');
  const [lat, setLat] = useState('');
  const [addr, setAddr] = useState('');
  const [logs, setLogs] = useState<LogLine[]>([]);

  const [tAtt, setTAtt] = useState('');
  const [tDetail, setTDetail] = useState('');
  const [tStudent, setTStudent] = useState('');
  const [tType, setTType] = useState('');
  // 教师补签要的 attendanceId / detailId，平台**本地就存着**（考勤同步时落库），
  // 没道理让用户去别处翻原始 ID 手抄一遍。这里拉出来做成选择器。
  /**
   * 可补签的场次。
   *
   * 用 `useResource` 而不是手写 `useEffect` + `alive` 标志 ——
   * 后者那套「防卸载后 setState」的样板在 `useResource` 里已经处理了
   * （它用序号 `seq` 防的还不止卸载，还有**后发请求先回来**）。
   *
   * `catch { /* 拿不到就退化成手填 *\/ }` 那个空 catch 也一并去掉：
   * 失败要能说出来，而不是让用户以为「这个账号没有可补签的场次」。
   */
  const loadSessions = useCallback(
    async () =>
      (await api.attendance(accountId)).items.filter((x) => x.detail_id && x.attendance_id),
    [accountId],
  );
  const { data: sessions, loading: sessionsLoading, error: sessionsError, reload: reloadSessions } =
    useResource(loadSessions);
  const rows = sessions ?? [];

  const { call, busy, msg } = useCall(reloadSessions);

  function body() {
    return {
      include_ended: includeEnded,
      ended_within: endedWithin,
      reopen_ended: reopenEnded,
      reopen_seconds: reopenSeconds,
      sign_status: signStatus,
      longitude: lon || undefined,
      latitude: lat || undefined,
      address: addr || undefined,
    };
  }

  async function run() {
    setLogs([]);
    const r = await call<{ ok: number; fail: number; skip: number; total: number }>(
      `/api/accounts/${accountId}/force-checkin`,
      { method: 'POST', body: JSON.stringify(body()) },
      '补签完成',
    );
    // 日志随响应回传（同步调用不落库）
    if (r.logs?.length) setLogs(r.logs as LogLine[]);
  }

  async function teacherMakeup() {
    setLogs([]);
    const r = await call(
      `/api/accounts/${accountId}/teacher-makeup`,
      {
        method: 'POST',
        body: JSON.stringify({
          attendance_id: tAtt, detail_id: tDetail, student_id: tStudent,
          type: tType === '' ? undefined : Number(tType),
          status: signStatus,
          longitude: lon || undefined, latitude: lat || undefined, address: addr || undefined,
        }),
      },
      '教师补签已提交',
    );
    if (r.logs?.length) setLogs(r.logs as LogLine[]);
  }

  return (
    <>
      {msg && <div className={`msg-bar ${MSG_CLASS[msg.kind]}`}>{msg.text}</div>}

      <div className="card mt-24">
        <h2 className="card-title">
          自助补签
          <span className="card-meta">学生账号 · 走教师端接口</span>
        </h2>
        <p className="muted small" style={{ marginTop: -4 }}>
          用学生账号调用教师端考勤列表发现场次，再提交补签。
          「不在该教学班」属正常跳过 —— 教师端分页会返回同课程<strong>所有教学班</strong>的场次。
        </p>

        <div className="form-row" style={{ marginTop: 12, alignItems: 'flex-start' }}>
          <div className="field">
            <label htmlFor="ew">已结束回看窗口（分钟，0=不限）</label>
            <input id="ew" className="input" type="number" min={0} value={endedWithin}
              onChange={(e) => setEndedWithin(Number(e.target.value))} />
          </div>
          <div className="field">
            <label htmlFor="ss">补签写入状态</label>
            <select id="ss" className="input" value={signStatus}
              onChange={(e) => setSignStatus(Number(e.target.value))}>
              {STATUS_OPTIONS.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
            </select>
          </div>
          <div className="field">
            <label htmlFor="rs">重开窗口（秒）</label>
            <input id="rs" className="input" type="number" min={10} value={reopenSeconds}
              onChange={(e) => setReopenSeconds(Number(e.target.value))} disabled={!reopenEnded} />
          </div>
        </div>

        <div className="row" style={{ gap: 14, marginTop: 10 }}>
          <label className="row" style={{ gap: 5 }}>
            <input type="checkbox" checked={includeEnded}
              onChange={(e) => setIncludeEnded(e.target.checked)} />
            <span className="small">包含已结束的场次</span>
          </label>
          <label className="row" style={{ gap: 5 }}>
            <input type="checkbox" checked={reopenEnded}
              onChange={(e) => setReopenEnded(e.target.checked)} />
            <span className="small">临时重开已结束场次（补完自动还原）</span>
          </label>
        </div>

        {reopenEnded && (
          <p className="muted small" style={{ color: 'var(--warn)' }}>
            ⚠️ 重开的那 {reopenSeconds} 秒内，该场对全班显示为「进行中」。
            补完立即还原；若进程被强杀，需手动检查该场状态。
          </p>
        )}

        <div className="row" style={{ gap: 8, marginTop: 12 }}>
          <input className="input" style={{ width: 110 }} placeholder="经度"
            value={lon} onChange={(e) => setLon(e.target.value)} />
          <input className="input" style={{ width: 110 }} placeholder="纬度"
            value={lat} onChange={(e) => setLat(e.target.value)} />
          <input className="input" style={{ width: 200 }} placeholder="地址（可选）"
            value={addr} onChange={(e) => setAddr(e.target.value)} />
        </div>

        <div className="row" style={{ gap: 8, marginTop: 12 }}>
          {/*
            补签**会临时重开已结束的场次** —— 重开的几十秒里该场对**全班**显示为
            「进行中」（本文件开头自己写着）。所以它不能是一次点击就提交：
            必须把代价摊开给用户看。

            原先这里是两个绑**同一个 handler** 的按钮（「提交补签」/「开始补签」），
            零确认。审计也点了这条。
          */}
          <InlineConfirm
            label={sessionsLoading ? '拉取中…' : '开始补签'}
            confirmLabel="确认补签"
            disabled={busy || sessionsLoading || rows.length === 0}
            question={
              sessionsError
                ? `取不到考勤场次：${sessionsError}（这是取不到数据，不是「没有场次」）`
                : sessionsLoading
                ? '正在拉取考勤场次…'
                : rows.length === 0
                ? '没有可补签的场次。'
                : '将扫描本账号全部考勤场次，对「进行中未签」和「已结束未签」提交补签。' +
                  '已结束的会临时重开（此间该场对全班显示为「进行中」，补完自动还原）。'
            }
            onConfirm={run}
          />
        </div>
      </div>

      <div className="card mt-24">
        <h2 className="card-title">
          教师补签
          <span className="card-meta">需教师权限</span>
        </h2>
        <p className="muted small" style={{ marginTop: -4 }}>
          用教师账号为指定学生补签。服务端会校验当前账号是否具备该场次的教师权限。
        </p>
        <div className="form-row" style={{ marginTop: 12, alignItems: 'flex-start' }}>
          {rows.length > 0 ? (
            <div className="field" style={{ flex: 2 }}>
              <label htmlFor="ta">考勤场次</label>
              {/* attendanceId / detailId 平台在同步考勤时就存下来了，
                  让用户去别处翻原始 ID 手抄一遍毫无道理（而且是两个 ID）。 */}
              <select
                id="ta"
                className="input"
                value={tAtt}
                onChange={(e) => {
                  const hit = rows.find((x) => x.attendance_id === e.target.value);
                  setTAtt(e.target.value);
                  setTDetail(hit?.detail_id ?? '');
                }}
              >
                <option value="">请选择…</option>
                {rows.map((s) => (
                  <option key={s.detail_id} value={s.attendance_id}>
                    {s.title}{s.course_name ? ` · ${s.course_name}` : ''}
                  </option>
                ))}
              </select>
            </div>
          ) : (
            <div className="field">
              <label htmlFor="ta">考勤场次 ID</label>
              <input id="ta" className="input" value={tAtt}
                onChange={(e) => setTAtt(e.target.value)} />
            </div>
          )}
          <div className="field">
            <label htmlFor="ts">学生 ID</label>
            <input id="ts" className="input" value={tStudent}
              onChange={(e) => setTStudent(e.target.value)}
              placeholder="从教师端的学号/ID 处复制" />
          </div>
          <div className="field">
            <label htmlFor="tt">类型（可留空）</label>
            <input id="tt" className="input" value={tType} onChange={(e) => setTType(e.target.value)} />
          </div>
        </div>
        {(!tAtt || !tStudent) && (
          <p className="muted small" style={{ marginTop: 8 }}>
            {!tAtt ? '请先选择考勤场次' : '请先填写学生 ID'}
            {!tAtt && rows.length === 0 ? '（本账号还没有同步到考勤场次，先去「同步考勤」）' : ''}
          </p>
        )}
        <div className="row" style={{ gap: 8, marginTop: 12 }}>
          {/* disabled 时必须给出原因，否则用户面对两个灰按钮无从下手 */}
          {/* 原先这里还留着一个**没有文字**的空按钮 —— 渲染成一个小圆圈，
              用户不知道那是什么。已删。 */}
          <button className="btn btn-primary" onClick={() => void teacherMakeup()}
            disabled={busy || !tAtt || !tDetail || !tStudent}
            title={!tAtt ? '请先选择考勤场次' : !tStudent ? '请先填写学生 ID' : undefined}>
            提交补签
          </button>
        </div>
      </div>


      {logs.length > 0 && (
        <div className="card mt-24">
          <h2 className="card-title">
            补签日志
            <span className="card-meta">共 {logs.length} 条</span>
          </h2>
          <div className="log-panel" style={{ height: 300 }}>
            {logs.map((l, i) => (
              // 包里的类名是 `log-line is-ok/is-err/is-warn`；
              // `log-${level}` 这组 class 根本不存在（只有颜色、没有行内布局）
              <div
                key={i}
                className={`log-line ${
                  l.level === 'ok' ? 'is-ok' : l.level === 'error' ? 'is-err'
                  : l.level === 'warn' ? 'is-warn'
                  : l.level === 'think' ? 'is-think'
                  : l.level === 'answer' ? 'is-answer' : 'is-info'
                }`}
              >
                <span>{l.message}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </>
  );
}
