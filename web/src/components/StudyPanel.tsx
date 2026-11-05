/**
 * 刷课面板：选课程 → 执行 → SSE 实时日志。
 *
 * 重做要点（见 docs/UI-REDESIGN.md）：
 *   · 账号切换交给 AccountTabs（React 版），不再自造下拉框；运行中禁止切换
 *   · 去掉 `见 PENDING G1` / `数据库幂等约束` 这类内部术语
 *   · 未执行时不再留一个 340px 的黑色空框，改为一句说明
 *   · 补整体进度条 —— 之前只有滚动的日志，看不出「还剩多少」
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useLogStream } from '../lib/useLogStream';
import { useResource } from '../lib/useResource';
import { api, type Account, type Course, type Resource } from '../lib/api';
import { fmtTime } from '../lib/labels';
import { RES_TYPE_TEXT } from '../lib/labels';
import AccountTabs from './AccountTabs';


interface LogLine { id?: number; level: string; message: string; ts?: number }

interface Props {
  accounts: Account[];
  accountId: number;
  /** 深链带来的初始学期 / 课程（来自课程页的「刷这门课」） */
  initialTermId?: string;
  initialCoursePk?: number;
}

export default function StudyPanel({
  accounts, accountId, initialTermId = '', initialCoursePk,
}: Props) {
  const [termId, setTermId] = useState<string>(initialTermId);
  const [coursePk, setCoursePk] = useState<number | 'all'>(initialCoursePk ?? 'all');
  /** 不在当前学期里的待完成数（往期遗留） */
  const [onlyVideo, setOnlyVideo] = useState(false);
  // 只看计入考核的。考核分只由这部分产生 —— 默认**开**，因为这才是用户要的。
  const [onlyAssessed, setOnlyAssessed] = useState(true);
  const [delay, setDelay] = useState(0.35);
  const [limit, setLimit] = useState<number | ''>('');

  // 日志来自**全局流**，不是单执行的流 —— 这样调度器/考勤监控触发的任务
  // 也能在这里看到进度（它们没有前端能拿到的 execution_id）。
  const { lines, connected } = useLogStream(accountId);
  const [running, setRunning] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ kind: 'ok' | 'err' | 'warn'; text: string } | null>(null);
  const logRef = useRef<HTMLDivElement>(null);


  // 一个 useResource 取代 courses/resources/leftover/assessStat 四个 state
  // + reloadData + useEffect 五件套。
  //
  // 原先的 `catch (e) { setMsg(...) }` 只把错误丢进提示条，**数据仍是空数组** ——
  // 于是页面同时显示「取不到数据」的提示和「待处理 0」，自相矛盾。
  // 现在错误是独立状态，渲染层能明确分支。
  const load = useCallback(async () => {
    const [cs, rs, all, assessed] = await Promise.all([
      api.courses(accountId),
      api.resources(accountId, {
        only_todo: true,
        ...(termId ? { term_id: termId } : {}),
        ...(onlyVideo ? { res_type: 1 } : {}),
      }),
      // 另外取一次**不限学期**的待完成，用来算「往期遗留」。
      // 只报当前学期的数会误导：概览写「往期遗留 148」而这里写「待处理 0」，
      // 用户会以为全刷完了。
      api.resources(accountId, { only_todo: true, ...(onlyVideo ? { res_type: 1 } : {}) }),
      // 计入考核的**全部**（含已完成）—— 只取待完成算不出「已完成多少」。
      api.resources(accountId, {
        only_assessed: true,
        ...(termId ? { term_id: termId } : {}),
      }),
    ]);
    return {
      courses: cs,
      resources: rs,
      leftover: Math.max(0, all.length - rs.length),
      assess: { total: assessed.length, done: assessed.filter((r) => r.study_status === 2).length },
    };
  }, [accountId, onlyVideo, termId]);
  const { data, loading, error, reload: reloadData } = useResource(load);
  const courses: Course[] = data?.courses ?? [];
  const resources: Resource[] = data?.resources ?? [];
  const leftover = data?.leftover ?? 0;
  const assessStat = data?.assess ?? { total: 0, done: 0 };

  /** 学期列表：从已扫到的课程里推导。term_id 是递增的数字串，倒序即最新在前。 */
  const terms = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of courses) if (c.term_id) m.set(c.term_id, c.term_name);
    return [...m.entries()]
      .map(([id, name]) => ({ id, name }))
      .sort((a, b) => (a.id < b.id ? 1 : -1));
  }, [courses]);

  // 首次拿到课程后，默认选中最新学期 —— 刷课默认不该碰到已结束的学期
  useEffect(() => {
    // 深链指定的学期优先；没有才回落到最新学期
    if (!termId && terms.length) setTermId(terms[0].id);
  }, [terms, termId]);

  const scopedCourses = useMemo(
    () => (termId ? courses.filter((c) => c.term_id === termId) : courses),
    [courses, termId],
  );

  // 日志自动滚到底
  // 有新日志就滚到底（lines 来自全局流）
  useEffect(() => { logRef.current?.scrollTo({ top: logRef.current.scrollHeight }); }, [lines]);

  // 离开时关掉 SSE

  // ---------------- 操作

  async function onScan() {
    setBusy(true); setMsg(null);
    try {
      const r = await api.scan(accountId, termId || undefined);
      const d = r.data;
      setMsg({ kind: 'ok', text: d ? `扫描完成：${d.term_name}，${d.courses} 门课 / ${d.resources} 个资源` : '扫描完成' });
      await reloadData();
    } catch (e) { setMsg({ kind: 'err', text: (e as Error).message }); }
    finally { setBusy(false); }
  }

  /**
   * 任务结束了吗？—— 从**日志内容**判断，而不是从 execution_id。
   *
   * 后端收尾时写的是 `任务 <key> <state>：<detail>`，所以匹配到这条
   * 就说明这次执行结束了。这样不依赖「前端知道 id」，
   * 自动触发的任务也能正确解除「执行中」。
   */
  useEffect(() => {
    if (!running) return;
    const last = lines[lines.length - 1];
    if (last && /^任务 \S+ (done|failed|cancelled)/.test(last.message)) {
      setRunning(false);
      void reloadData();
    }
  }, [lines, running, reloadData]);

  async function onStudy() {
    setBusy(true); setMsg(null);
    try {
      const body = {
        // 学期必须显式传：不传的话后端会挑**全部学期**的未完成资源，
        // 把已结束学期的遗留也刷掉（扫过多个学期后「待处理」会莫名变大）
        term_id: termId || undefined,
        course_pk: coursePk === 'all' ? undefined : (coursePk as number),
        // 图片(4)不带上：客户端 complete_resource 没有图片分支，带上只会刷出一堆失败
        res_types: onlyVideo ? [1] : [1, 3],
        only_assessed: onlyAssessed,
        delay,
        limit: limit === '' ? undefined : Number(limit),
        // 先扫再刷。否则老师新上传的资源在本地库里根本不存在，
        // 「开始刷课」看起来跑了却什么都没刷到 —— 不能指望用户每次记得先手动扫描。
        rescan: true,
      };
      // 提交成功就进入「执行中」—— 进度由全局日志流驱动，见上面的 useEffect。
      // 原先后端不返回 execution_id，前端也没接返回值，于是点完**界面毫无变化**。
      const r = await api.study(accountId, body);
      setRunning(true);
    } catch (e) { setMsg({ kind: 'err', text: (e as Error).message }); }
    finally { setBusy(false); }
  }

  async function onCancel() {
    // execution_id 从**流里**取 —— 它是当前账号最近一条日志所属的执行。
    // 这样自动触发的任务也能中断（原先依赖前端自己记的 id，那个永远是 null）。
    const execId = lines[lines.length - 1]?.execution_id;
    if (execId == null) {
      setMsg({ kind: 'warn', text: '还没看到这个账号的日志，无法定位要中断的任务。' });
      return;
    }
    try { await api.cancel(execId); setMsg({ kind: 'warn', text: '已请求中断，当前资源完成后停止。' }); }
    catch (e) { setMsg({ kind: 'err', text: (e as Error).message }); }
  }

  const todoCount = resources.length;

  // 进度按当前选择范围统计
  const scope = coursePk === 'all' ? scopedCourses : scopedCourses.filter((c) => c.id === coursePk);
  const resTotal = scope.reduce((s, c) => s + (c.res_total || 0), 0);
  const resDone = scope.reduce((s, c) => s + (c.res_done || 0), 0);
  const pct = resTotal ? Math.round((resDone / resTotal) * 100) : 0;
  const allDone = resTotal > 0 && resDone >= resTotal;

  return (
    <>
      {/*
        「取不到数据」必须说出来 —— 原先是 `catch (e) { setMsg(...) }`，
        数据仍是空数组，于是页面同时显示错误提示和「待处理 0」，自相矛盾。
      */}
      {error && (
        <div className="msg-bar msg-err" style={{ marginBottom: 12 }}>
          取不到课程与资源数据：{error}
          <button className="btn btn-sm" style={{ marginLeft: 8 }} onClick={() => void reloadData()}>
            重试
          </button>
          <div className="small" style={{ marginTop: 4 }}>
            这是<strong>取不到数据</strong>，不是「没有待处理资源」。
          </div>
        </div>
      )}
      {loading && !data && (
        <p className="muted small" style={{ marginBottom: 12 }}>正在加载课程与资源…</p>
      )}

      <AccountTabs accounts={accounts} current={accountId} path="/study" disabled={running} />

      {msg && <div className={`msg-bar msg-${msg.kind}`}>{msg.text}</div>}

      <div className="card mt-16">
        <h2 className="card-title">选择目标</h2>
        <div className="form-row">
          <div className="field">
            <label htmlFor="sp-term">学期</label>
            <select id="sp-term" className="input" value={termId} disabled={running}
              onChange={(e) => { setTermId(e.target.value); setCoursePk('all'); }}>
              <option value="">全部学期</option>
              {terms.map((t) => (
                <option key={t.id} value={t.id}>{t.name}</option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor="sp-course">课程</label>
            <select id="sp-course" className="input" value={String(coursePk)} disabled={running}
              onChange={(e) => setCoursePk(e.target.value === 'all' ? 'all' : Number(e.target.value))}>
              <option value="all">全部课程</option>
              {scopedCourses.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.course_name}（{c.res_done}/{c.res_total}）
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor="sp-delay">间隔（秒）</label>
            <input id="sp-delay" className="input" type="number" step="0.1" min="0" value={delay}
              onChange={(e) => setDelay(Number(e.target.value))} disabled={running} />
          </div>
          <div className="field">
            <label htmlFor="sp-limit">最多处理（留空=全部）</label>
            <input id="sp-limit" className="input" type="number" min="1" value={limit}
              onChange={(e) => setLimit(e.target.value === '' ? '' : Number(e.target.value))} disabled={running} />
          </div>
          <button className="btn" onClick={() => void onScan()} disabled={busy || running}>
            重新扫描课程
          </button>
        </div>
        <label className="row" style={{ marginTop: 10 }}>
          <input type="checkbox" checked={onlyVideo} disabled={running}
            onChange={(e) => setOnlyVideo(e.target.checked)} />
          <span className="small">
            只刷视频（不勾则视频和文档都刷 —— 文档占待完成量多数，建议保持不勾）
          </span>
        </label>
        {/* 计入考核的才影响分数。默认勾上 —— 不勾的话会把大量
            不计入考核的资源也刷一遍，白花时间还看不出区别。 */}
        <label className="row" style={{ marginTop: 6 }}>
          <input type="checkbox" checked={onlyAssessed} disabled={running}
            onChange={(e) => setOnlyAssessed(e.target.checked)} />
          <span className="small">
            <b>只刷计入考核的</b>（考核分只由这部分产生；
            {assessStat.total > 0 && ` 本账号计入考核 ${assessStat.done}/${assessStat.total}`}
            {assessStat.total === 0 && ' 还没扫描过，先点「重新扫描课程」'}）
          </span>
        </label>
      </div>

      <div className="card mt-24">
        <h2 className="card-title">
          执行
          <span className="badge badge-muted" style={{ marginLeft: 10 }}>
            待处理 {todoCount} 个
          </span>
          {/* 只报当前学期的数会误导：概览写「往期遗留 148」、这里写「待处理 0」，
              用户会以为全刷完了。把两者摆在一起说清楚。 */}
          {leftover > 0 && (
            <span className="card-meta">
              另有往期遗留 {leftover} 个（在「课程」页切换学期后逐门刷）
            </span>
          )}
        </h2>

        {resTotal > 0 && (
          <div className="pbar" style={{ marginBottom: 14 }}>
            <div className="pbar-track">
              <div className={`pbar-fill${allDone ? ' is-done' : ''}`} style={{ width: `${pct}%` }} />
            </div>
            <span className="pbar-text">
              {resDone}/{resTotal}（{pct}%）
            </span>
          </div>
        )}

        <div className="row">
          <button className="btn btn-primary" onClick={() => void onStudy()} disabled={busy || running}>
            {running ? '执行中…' : '开始刷课'}
          </button>
          <button className="btn btn-danger" onClick={() => void onCancel()} disabled={!running || busy}>
            {busy ? '处理中…' : '中断'}
          </button>
        </div>
        <p className="muted small" style={{ marginTop: 10 }}>
          开始刷课前会先扫描一次课程，确保老师新上传的资源也被刷到。
          <strong>随时可以再刷</strong>：已完成的资源会被自动跳过，不会重复提交；
          同一时间只会跑一份。
        </p>
      </div>

      <div className="card mt-24">
        <h2 className="card-title">实时日志</h2>
        {lines.length === 0 ? (
          <div className="empty-state">
            <p className="empty-title">尚未开始</p>
            <p className="empty-desc">点击「开始刷课」后，这里会实时显示每个资源的处理结果。</p>
          </div>
        ) : (
          <div className="log-panel" ref={logRef}>
            {lines.map((l, i) => (
              <div key={i} className={`log-${l.level}`}>
                {l.ts ? `[${fmtTime(l.ts)}] ` : ''}
                {l.message}
              </div>
            ))}
          </div>
        )}
      </div>
    </>
  );
}
