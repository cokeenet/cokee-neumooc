/**
 * 作业面板：**左侧作业列表 + 右侧试卷详情**，结构对齐「考试」页。
 *
 * ## 为什么要改
 *
 * 原先只有一张表：课程 / 作业 / 截止 / 状态，**看不到分数，也看不到题目**。
 * 而平台（`smartexam` 子系统）其实给了完整数据：
 *
 * ```
 * GET /web-api/smartexam/test/student/paper?testId=<ctlId>
 *   → paperScore（满分）、paperQuestionQty、paperParts[]
 *     → paperQuestionList[] → paperQuestion{stem, choices, candidateScore,
 *                                          candidateAnswer, standardAnswer, analysis}
 * ```
 *
 * ⚠️ 这条链路以前在本文件里被写成「一律 403，后端拿不到」——**是错的**。
 * 实测（2026-10）：未交卷作业的题目能正常读，`enter` 才是会因「作答时间已截止」
 * 失败的接口。见 `docs/PENDING.md` 的更正说明。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  api, type HomeworkDetail, type HomeworkDetailQuestion, type HomeworkItem,
} from '../lib/api';
import { useAction, useResource } from '../lib/useResource';
import { fmtDeadline, fmtTs } from '../lib/labels';
import EmptyState from 'korikura-webui/components/ui/EmptyState';
import ErrorState from 'korikura-webui/components/ui/ErrorState';
import { InlineConfirm } from 'korikura-webui/components/InlineDialog';

interface LlmStatus {
  available: boolean;
  model: string;
  base_url: string;
}

interface Props {
  accounts: { id: number; label: string; username?: string }[];
  accountId: number;
}

const TYPE_LABEL: Record<string, string> = {
  '1': '单选题', '2': '多选题', '3': '判断题', '4': '填空题',
  '5': '简答题', '6': '论述题', '7': '计算题', '8': '名词解释', '40': '文件上传',
};
const typeLabel = (t: string) => TYPE_LABEL[t] ?? '题目';
const LETTERS = 'ABCDEFGHIJKLMNOP';

function fmtNum(n: number | null | undefined): string {
  if (n === null || n === undefined) return '—';
  return Number.isInteger(n) ? String(n) : n.toFixed(1);
}
function stripTags(html: string): string {
  return html.replace(/<[^>]*>/g, '').replace(/&nbsp;/g, ' ').trim();
}
/** `candidateAnswer` 是字符串：单选一个 id，多选 `id1,id2`，主观题是文本 */
function answerIds(a: string | null | undefined): string[] {
  if (!a) return [];
  return a.split(',').map((x) => x.trim()).filter(Boolean);
}

/** 一道题（含已交时的「我的答案」与得分）。 */
function QuestionCard({
  q, canShowScore, canShowAnswer, editable, picked, onPick, text, onText,
}: {
  q: HomeworkDetailQuestion;
  canShowScore: boolean;
  canShowAnswer: boolean;
  /** 作答模式：可以点选/输入 */
  editable?: boolean;
  /** 已选的 choiceId 集合 */
  picked?: Set<string>;
  onPick?: (choiceId: string) => void;
  /** 填空/主观题的文本答案 */
  text?: string;
  onText?: (v: string) => void;
}) {
  const mineIds = new Set((q.myAnswerText || '').split(',').map((x) => x.trim()).filter(Boolean));
  const rightRaw = q.standardAnswer;
  const rightIds = new Set(
    Array.isArray(rightRaw)
      ? rightRaw.map(String)
      : typeof rightRaw === 'string' && rightRaw
        ? rightRaw.split(',')
        : [],
  );
  const isChoice = (q.choices?.length ?? 0) > 0;
  const got = typeof q.candidateScore === 'number' ? q.candidateScore : null;
  const full = typeof q.questionScore === 'number' ? q.questionScore : null;
  const tone = !canShowScore || got === null
    ? 'var(--muted)'
    : full !== null && got >= full ? 'var(--ok)' : got > 0 ? 'var(--warn)' : 'var(--danger)';

  return (
    <div className="exam-q">
      <div className="exam-q-head">
        <span className="exam-q-no">第 {q.questionNo} 题</span>
        <span className="badge badge-muted">{typeLabel(q.questionType)}</span>
        <span className="exam-q-score" style={{ color: tone }}>
          {canShowScore && got !== null
            ? `${fmtNum(got)} / ${fmtNum(full)} 分`
            : `${fmtNum(full)} 分`}
        </span>
      </div>
      <p className="exam-q-stem">{q.stemText || stripTags(q.stem)}</p>

      {isChoice ? (
        <ul className="exam-choices">
          {q.choices.map((c, i) => {
            // 作答态：用本地 picked（draft）；只读态：用平台返回的我的答案
            const sel = editable ? (picked?.has(c.choiceId) ?? false) : mineIds.has(c.choiceId);
            const correct = !editable && canShowAnswer && rightIds.has(c.choiceId);
            const cls = correct ? (sel ? 'is-right' : 'is-correct') : sel ? 'is-picked' : '';
            return (
              <li key={c.choiceId}
                className={`exam-choice ${cls}`}
                style={editable ? { cursor: 'pointer' } : undefined}
                onClick={editable ? () => onPick?.(c.choiceId) : undefined}>
                <span className="exam-choice-letter">{LETTERS[i] ?? '?'}</span>
                <span className="exam-choice-text">{stripTags(c.choiceContent)}</span>
                <span className="exam-choice-flags">
                  {sel && <span className="badge badge-run">{editable ? '已选' : '我的选择'}</span>}
                  {correct && <span className="badge badge-ok">正确答案</span>}
                </span>
              </li>
            );
          })}
        </ul>
      ) : editable ? (
        // 填空/主观/文件题：给一个输入框（多空用 | 分隔）
        <div className="field" style={{ marginTop: 8 }}>
          <input className="input"
            placeholder="填写答案（多个空用 | 分隔）"
            value={text ?? ''}
            onChange={(e) => onText?.(e.target.value)} />
          {q.questionType === '40' && (
            <span className="muted small">
              文件上传题：这里填的是「说明文字」，附件仍需到平台上传。
            </span>
          )}
        </div>
      ) : (
        <p className="exam-answer">
          <span className="muted">我的答案：</span>
          {q.myAnswerText ? stripTags(q.myAnswerText) : '（未作答）'}
        </p>
      )}

      {(q.attachments?.length ?? 0) > 0 && (
        <p className="exam-answer">
          <span className="muted">附件：</span>
          {q.attachments.map((f) => (
            <span key={f} className="badge badge-muted" style={{ marginRight: 6 }}>{f}</span>
          ))}
        </p>
      )}

      {canShowAnswer && (
        <>
          {rightIds.size > 0 && !isChoice && (
            <p className="exam-answer"><span className="muted">参考答案：</span>{[...rightIds].join(' ')}</p>
          )}
          {q.analysis && (
            <p className="exam-analysis"><span className="muted">解析：</span>{stripTags(q.analysis)}</p>
          )}
        </>
      )}
    </div>
  );
}

/** 右栏：一份作业（未交＝题目预览，已交＝我的答案与得分）。 */
function PaperView({
  detail, loading, error, onRetry, answering, draft, onPick, onText,
}: {
  detail: HomeworkDetail | null;
  loading: boolean;
  error: string;
  onRetry: () => void;
  /** 作答模式（点「开始作答」后为 true） */
  answering?: boolean;
  /** 题号 → 答案（选择题存 choiceId 逗号串；填空存文本） */
  draft?: Map<string, string>;
  onPick?: (questionNo: string, choiceId: string) => void;
  onText?: (questionNo: string, v: string) => void;
}) {
  if (loading) return <p className="muted">正在拉取作业详情…</p>;
  if (error) {
    return (
      <div>
        <ErrorState reason={`取不到作业详情：${error}`} />
        <div style={{ marginTop: 8 }}><button className="btn btn-sm" onClick={onRetry}>重试</button></div>
      </div>
    );
  }
  if (!detail) return <p className="muted" style={{ margin: 0 }}>从左侧选一份作业，这里显示题目与得分。</p>;

  const parts = detail.parts ?? [];
  const qty = parts.reduce((n, p) => n + (p.questions?.length ?? 0), 0);
  const max = Number(detail.paper_score) || 0;
  const finalScore = typeof detail.final_score === 'number' ? detail.final_score : null;
  const calc = Number(detail.calculate_score) || null;
  const showScore = detail.can_show_score && (finalScore ?? calc) !== null;

  return (
    <div>
      <div className="exam-paper-head">
        <h3 className="exam-paper-title">{detail.name}</h3>
        <div className="exam-paper-stats">
          <div>
            <span className="stat-value" style={{ color: showScore ? 'var(--ok)' : 'var(--muted)' }}>
              {detail.submitted
                ? showScore ? fmtNum(finalScore ?? calc) : '未公布'
                : '未交'}
            </span>
            <span className="stat-label"> / {fmtNum(max)} 分</span>
          </div>
          <div className="muted small">
            共 {qty} 题 · {parts.length} 个大题
            {detail.submitted && (
              <span className={`badge ${detail.mark_status === 1 ? 'badge-ok' : 'badge-warn'}`} style={{ marginLeft: 6 }}>
                {detail.mark_status === 1 ? '已批阅' : '待批阅'}
              </span>
            )}
            {!detail.submitted && <span className="badge badge-muted" style={{ marginLeft: 6 }}>未提交</span>}
          </div>
          <div className="muted small">{detail.open_time_str || '—'} ~ {detail.close_time_str || '—'}</div>
          {detail.submitted && detail.submit_time && (
            <div className="muted small">提交 {fmtTs(detail.submit_time)}</div>
          )}
        </div>
      </div>

      {detail.note && <p className="msg-bar msg-info small" style={{ margin: '8px 0' }}>{detail.note}</p>}
      {detail.submitted && !detail.can_show_score && (
        <p className="muted small" style={{ margin: '4px 0 8px' }}>
          平台把这场作业的<strong>分数</strong>设为不公开，所以这里显示「未公布」。
        </p>
      )}
      {detail.submitted && !detail.can_show_answer && (
        <p className="muted small" style={{ margin: '4px 0 8px' }}>
          平台把<strong>标准答案与解析</strong>设为不公开（只显示你自己的答案）。
        </p>
      )}

      {parts.map((part, pi) => (
        <section key={`${part.partName}-${pi}`} className="exam-part">
          <h4 className="exam-part-title">
            {part.partName || `第 ${pi + 1} 大题`}
            <span className="muted small">
              {part.partDesc ? ` · ${part.partDesc}` : ''} · {part.questions?.length ?? 0} 题
            </span>
          </h4>
          {(part.questions ?? []).map((q) => {
            const cur = draft?.get(q.questionNo) ?? '';
            return (
              <QuestionCard
                key={`${q.questionNo}-${q.stem.slice(0, 8)}`}
                q={q}
                canShowScore={detail.can_show_score}
                canShowAnswer={detail.can_show_answer}
                editable={answering}
                picked={new Set(cur.split(',').map((x) => x.trim()).filter(Boolean))}
                onPick={(cid) => onPick?.(q.questionNo, cid)}
                text={cur}
                onText={(v) => onText?.(q.questionNo, v)}
              />
            );
          })}
        </section>
      ))}
    </div>
  );
}

export default function TasksPanel({ accountId }: Props) {
  const load = useCallback(async () => {
    const [tasks, llm] = await Promise.all([
      // ⚠️ 用 `homeworks`（按课程遍历 test/list，**含已交**），
      // 不能用 `tasks`（study-task/page，只返回待完成）——
      // 一个学期作业全交完之后那个接口返回 0 项，页面就成了「没有作业」。
      api.homeworks(accountId),
      api.llmStatus().catch(() => null),
    ]);
    return { tasks, llm };
  }, [accountId]);
  const { data, loading, error: err, reload } = useResource(load);

  const tasks = data?.tasks;
  const items = useMemo(() => (tasks?.items ?? []).filter((t) => !!t.test_id), [tasks]);

  // ── 分数：每份作业拉一次试卷（串行，最多几份），缓存住 ──
  const [scores, setScores] = useState<Map<string, { max: number; submitted: boolean; score: number | null; canShow: boolean } | null>>(new Map());
  const detailCache = useRef(new Map<string, HomeworkDetail>());
  const aliveRef = useRef(true);

  const [openId, setOpenId] = useState<string | null>(null);
  const [paper, setPaper] = useState<HomeworkDetail | null>(null);
  const [paperLoading, setPaperLoading] = useState(false);
  const [paperErr, setPaperErr] = useState('');

  // ── 手动作答 ──
  /** 是否处于作答模式（点「开始作答」进入） */
  const [answering, setAnswering] = useState(false);
  /** 题号 → 答案（选择题存 choiceId 逗号串；填空存文本） */
  const [draft, setDraft] = useState<Map<string, string>>(new Map());

  /** 点选项：多选切换、单选替换。判断题按题型只有 1 个答案。 */
  const onPick = useCallback((qNo: string, cid: string) => {
    setDraft((prev) => {
      const next = new Map(prev);
      const cur = (next.get(qNo) ?? '').split(',').map((x) => x.trim()).filter(Boolean);
      const qt = (paper?.parts ?? [])
        .flatMap((p) => p.questions ?? [])
        .find((q) => q.questionNo === qNo)?.questionType;
      // 多选题（type=2）才累计，其它覆盖
      if (qt === '2') {
        const set = new Set(cur);
        if (set.has(cid)) set.delete(cid); else set.add(cid);
        next.set(qNo, [...set].join(','));
      } else {
        next.set(qNo, cid);
      }
      return next;
    });
  }, [paper]);

  const onText = useCallback((qNo: string, v: string) => {
    setDraft((prev) => new Map(prev).set(qNo, v));
  }, []);

  const answerAction = useAction(
    async (submit: boolean) => {
      if (!openId) return;
      const answers = [...draft.entries()]
        .filter(([, v]) => v.trim() !== '')
        .map(([questionNo, answer]) => ({ questionNo, answer }));
      if (answers.length === 0) throw new Error('还没有填写任何答案');
      const r = await api.answerTask(accountId, openId, answers, submit);
      // 保存成功后刷新详情（让"我的答案"跟上）+ 退出作答态
      detailCache.current.delete(openId);
      const t = items.find((x) => x.test_id === openId);
      if (t) await selectTask(t);
      return r;
    },
    // 交卷与存草稿的成功文案不同 —— 交卷要明确告知"不可逆已完成"
    { okText: '已保存' },
  );

  /** 手动操作的结果文案（区分草稿/交卷） */
  const [answerNote, setAnswerNote] = useState('');
  /** 提交动作（带"存草稿/交卷"的区分文案） */
  const doAnswer = useCallback(async (submit: boolean) => {
    setAnswerNote('');
    const r = await answerAction.run(submit);
    if (r) setAnswerNote(submit ? `已交卷（${r.saved ?? 0} 题）` : `已保存草稿（${r.saved ?? 0} 题）`);
  }, [answerAction]);

  const aiAction = useAction(async (submit: boolean) => {
    if (!openId) return;
    const r = await api.aiAnswerTask(accountId, openId, { submit });
    // AI 答完后刷新详情，让用户看到保存的答案
    detailCache.current.delete(openId);
    const t = items.find((x) => x.test_id === openId);
    if (t) await selectTask(t);
    return r;
  });

  const scoreOf = (d: HomeworkDetail) => {
    const finalScore = typeof d.final_score === 'number' ? d.final_score : null;
    const calc = Number(d.calculate_score);
    return {
      max: Number(d.paper_score) || 0,
      submitted: !!d.submitted,
      score: Number.isFinite(calc) ? (finalScore ?? calc) : finalScore,
      canShow: !!d.can_show_score,
    };
  };

  useEffect(() => {
    aliveRef.current = true;
    const todo = items.filter((t) => t.test_id && !detailCache.current.has(t.test_id));
    if (todo.length === 0) return;
    void (async () => {
      // 串行：这个平台 token 会被提前吊销，并发容易互相顶掉
      for (const t of todo) {
        if (!aliveRef.current) return;
        try {
          const d = await api.homeworkDetail(accountId, t.test_id, t.candidate_id);
          detailCache.current.set(t.test_id, d);
          if (aliveRef.current) {
            setScores((prev) => new Map(prev).set(t.test_id, scoreOf(d)));
          }
        } catch {
          if (aliveRef.current) setScores((prev) => new Map(prev).set(t.test_id, null));
        }
      }
    })();
    return () => { aliveRef.current = false; };
  }, [items, accountId]);

  const selectTask = useCallback(async (t: HomeworkItem) => {
    const id = t.test_id;
    setOpenId(id); setPaperErr('');
    const hit = detailCache.current.get(id);
    if (hit) { setPaper(hit); return; }
    setPaper(null); setPaperLoading(true);
    try {
      const d = await api.homeworkDetail(accountId, id, t.candidate_id);
      detailCache.current.set(id, d);
      setScores((prev) => new Map(prev).set(id, scoreOf(d)));
      setPaper(d);
    } catch (e) {
      setPaperErr((e as Error).message);
    } finally {
      setPaperLoading(false);
    }
  }, [accountId]);

  // ---- AI 辅助（保持原样）----
  const [question, setQuestion] = useState('');
  const [context, setContext] = useState('');
  const [answer, setAnswer] = useState('');
  const askFn = useCallback(async () => {
    const r = await api.llmAsk(question, context);
    setAnswer(r.answer || '');
  }, [question, context]);
  const ask = useAction(askFn);

  const renderScore = (t: HomeworkItem) => {
    const s = scores.get(t.test_id);
    if (s === undefined) return <span className="muted" title="正在拉取">…</span>;
    if (s === null) return <span className="muted" title="取不到详情">—</span>;
    if (!s.submitted) return <span className="muted" title={`尚未提交（满分 ${fmtNum(s.max)}）`}>未交</span>;
    if (!s.canShow || s.score === null) {
      return <span className="muted" title="平台未公布分数">已交</span>;
    }
    return (
      <>
        <b style={{ color: 'var(--ok)' }}>{fmtNum(s.score)}</b>
        <span className="muted"> / {fmtNum(s.max)}</span>
      </>
    );
  };

  return (
    <>
      <div className="exams-split mt-16">
        {/* ── 左栏：作业列表 ── */}
        <div className="card exam-list-card">
          <h2 className="card-title">
            作业与测验
            <span className="card-meta">
              {loading ? '加载中…' : err ? '取不到' : `${items.length} 项`}
            </span>
            <button className="btn btn-sm" style={{ marginLeft: 'auto' }} onClick={() => void reload()} disabled={loading}>
              {loading ? '查询中…' : '重新查询'}
            </button>
          </h2>

          {loading ? (
            <p className="muted small">正在查询…（要逐门课问一次，稍等几秒）</p>
          ) : err ? (
            <ErrorState reason={err} onRetry={() => void reload()} />
          ) : items.length === 0 ? (
            <EmptyState
              title="平台侧没有作业"
              desc={
                tasks?.diagnosis
                  || '已遍历该账号当前学期的课程，没有找到作业类任务。'
              }
            />
          ) : (
            <ul className="exam-list">
              {items.map((t) => {
                const active = openId === t.test_id;
                const overdue = !t.done && t.close_time ? t.close_time < Date.now() : false;
                return (
                  <li key={`${t.course_name}-${t.test_id}`}>
                    <button
                      className={`exam-list-item ${active ? 'is-active' : ''}`}
                      onClick={() => void selectTask(t)}
                      title={t.name}
                    >
                      <span className="exam-item-name">{t.name}</span>
                      <span className="exam-item-meta">
                        {t.course_name}
                        {t.done ? ' · 已提交' : t.close_time ? ` · 截止 ${fmtDeadline(t.close_time)}` : ''}
                        {overdue ? '（已过期）' : ''}
                      </span>
                      <span className="exam-item-score">{renderScore(t)}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}

          {tasks?.error && (
            <p className="msg-bar msg-warn small" style={{ marginTop: 10 }}>
              部分课程查询失败：{tasks.error}
            </p>
          )}
        </div>

        {/* ── 右栏：试卷 ── */}
        <div className="card exam-paper-card">
          <h2 className="card-title">
            作业详情
            <span className="card-meta">{openId ? '' : '从左侧选择'}</span>
          </h2>
          {/* ── 作答操作条 ── */}
          {paper && !answering && (
            <div className="row" style={{ gap: 8, marginBottom: 12, flexWrap: 'wrap' }}>
              <button className="btn btn-primary btn-sm" onClick={() => setAnswering(true)}>
                开始作答
              </button>
              <button className="btn btn-sm" disabled={aiAction.busy}
                title="后端取题 → 调 AI → 一次算完整份答案，保存为草稿（不交卷）"
                onClick={() => void aiAction.run(false)}>
                {aiAction.busy ? 'AI 作答中…' : 'AI 代答'}
              </button>
            </div>
          )}
          {answering && (
            <div className="row" style={{ gap: 8, marginBottom: 12, flexWrap: 'wrap' }}>
              <span className="muted small" style={{ alignSelf: 'center' }}>
                已填 {[...draft.values()].filter((v) => v.trim()).length} 题
              </span>
              <button className="btn btn-primary btn-sm" disabled={answerAction.busy}
                onClick={() => void doAnswer(false)}>
                {answerAction.busy ? '保存中…' : '保存草稿'}
              </button>
              <InlineConfirm
                label="交卷"
                danger
                question="交卷**不可逆**。确认所有答案都填好了？"
                confirmLabel="确认交卷"
                disabled={answerAction.busy}
                onConfirm={() => void doAnswer(true)}
              />
              <button className="btn btn-sm" onClick={() => { setAnswering(false); setDraft(new Map()); }}>
                退出作答
              </button>
            </div>
          )}

          {(answerNote || answerAction.msg || aiAction.msg) && (
            <p
              className={`msg-bar ${(
                answerAction.msg?.kind === 'err' || aiAction.msg?.kind === 'err'
              ) ? 'msg-err' : 'msg-ok'} small`}
              style={{ marginBottom: 10 }}>
              {answerAction.msg?.kind === 'err' || aiAction.msg?.kind === 'err'
                ? (answerAction.msg?.text || aiAction.msg?.text)
                : (answerNote || aiAction.msg?.text || answerAction.msg?.text)}
            </p>
          )}

          <PaperView
            detail={paper}
            loading={paperLoading}
            error={paperErr}
            onRetry={() => {
              const t = items.find((x) => x.test_id === openId);
              if (t) void selectTask(t);
            }}
            answering={answering}
            draft={draft}
            onPick={onPick}
            onText={onText}
          />
        </div>
      </div>

      {/* ── AI 辅助作答 ── */}
      <div className="card mt-24">
        <h2 className="card-title">
          AI 辅助作答
          <span className="card-meta">
            {data?.llm?.available ? data.llm.model : '未配置'}
          </span>
        </h2>
        <p className="muted small" style={{ marginTop: -4 }}>
          把题目粘进来，模型给出<b>参考答案草稿</b>。
          {' '}它不会替你提交 —— 主观题猜错会留痕，比不交更糟。
        </p>

        {!data?.llm?.available && (
          <p className="msg-bar msg-warn small">
            未启用：需要在后端设置 <code>NEUMOOC_LLM_API_KEY</code> 后重启。
            {data?.llm?.base_url ? `（端点 ${data.llm.base_url}）` : ''}
          </p>
        )}

        <div className="field" style={{ marginTop: 10 }}>
          <label htmlFor="ai-q">题目</label>
          <textarea id="ai-q" className="input" rows={5} value={question}
            placeholder="把题干和选项整段粘进来。选择题请带上选项，判断题说明 A=正确/B=错误。"
            onChange={(e) => setQuestion(e.target.value)} disabled={ask.busy} />
        </div>
        <div className="field">
          <label htmlFor="ai-c">背景材料（可选）</label>
          <textarea id="ai-c" className="input" rows={2} value={context}
            placeholder="阅读材料、课程名之类，帮助模型判断。"
            onChange={(e) => setContext(e.target.value)} disabled={ask.busy} />
        </div>

        <div className="row" style={{ gap: 8, marginTop: 10 }}>
          <button className="btn btn-primary" disabled={ask.busy || !data?.llm?.available || !question.trim()}
            onClick={() => void ask.run()}>
            {ask.busy ? '正在生成…' : '生成参考答案'}
          </button>
          {answer && (
            <button className="btn" onClick={() => { setQuestion(''); setContext(''); setAnswer(''); }}>
              清空
            </button>
          )}
        </div>

        {ask.msg && <p className="msg-bar msg-err small" style={{ marginTop: 10 }}>{ask.msg.text}</p>}

        {answer && (
          <>
            <p className="msg-bar msg-warn small" style={{ marginTop: 12 }}>
              ⚠️ 以下是 AI 生成的草稿，<b>请自行核对后再使用</b>。
            </p>
            <pre className="log-detail" style={{ maxWidth: '100%', maxHeight: 420 }}>{answer}</pre>
          </>
        )}
      </div>
    </>
  );
}
