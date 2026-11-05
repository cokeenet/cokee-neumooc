/**
 * 考试面板：左边考试列表，右边试卷详情（题目 / 我的答案 / 标准答案 / 得分）。
 *
 * ## 数据从哪来
 *
 * · 列表：`POST /web-api/smartexam/my-exam/finished/list`
 * · 试卷：`GET /web-api/smartexam/exam-mark/getPaperByExamIdAndPaperId/{examId}/{candidateId}`
 *
 * ⚠️ 试卷路径要 **examId + candidateId 两个 id**，只给 examId 会 403/404。
 *
 * ## 实际分数为什么要「后台补」
 *
 * 列表接口的 `score` 字段是**满分**（账号13 全部是 "100"，账号12 有一场是 "2"），
 * **不是实际得分**。实际得分只在试卷详情里（`calculateScore`），
 * 而平台没有更轻的分数接口（探过 `getScore` / `getPaperScore` / `my-exam/detail`
 * 等 7 个候选路径，全部 500）。
 *
 * 所以：列表先用满分渲染（立刻可见），后台**并发 3** 逐场拉试卷把实际分数填进去，
 * 填到哪场显示哪场（其余显示「…」）。分数缓存到 sessionStorage，
 * 同一会话刷新不再重复拉；试卷本体只留内存（一份 ~90KB，不塞存储）。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api, type ExamPaper, type ExamQuestion, type ExamSummary } from '../lib/api';
import { fmtTs } from '../lib/labels';
import ErrorState from 'korikura-webui/components/ui/ErrorState';

const TYPE_LABEL: Record<string, string> = {
  '1': '单选题', '2': '多选题', '3': '判断题', '4': '填空题',
  '5': '简答题', '6': '论述题', '7': '计算题', '8': '名词解释',
};
const typeLabel = (t: string) => TYPE_LABEL[t] ?? '题目';
const LETTERS = 'ABCDEFGHIJKLMNOP';
/**
 * 后台补分数时的并发数 —— **必须是 1（串行）**。
 *
 * 实测并发 3 时账号12 的 17 场有 7 场失败，错误是 `[401] 账号未登录`：
 * 这个平台会**提前吊销 token**，并发请求会同时触发重登 → 几个请求抢着重登、
 * 互相把对方的 token 作废，最终只有一部分成功。
 * 串行时 `with_reauth` 的重试是安全的（没有竞争），实测全部成功。
 */
const SCORE_CONCURRENCY = 1;
/** 单场失败后重试次数（偶发 401 重试一次通常就过） */
const SCORE_RETRY = 1;

function fmtNum(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1);
}
function stripTags(html: string): string {
  return html.replace(/<[^>]*>/g, '').replace(/&nbsp;/g, ' ').trim();
}
function fmtDur(mins: string): string {
  const n = Number(mins);
  if (!n || Number.isNaN(n)) return '—';
  return n >= 60 ? `${Math.floor(n / 60)} 小时${n % 60 ? ` ${n % 60} 分` : ''}` : `${n} 分钟`;
}
const examKey = (e: ExamSummary) => `${e.examId}:${e.candidateId}`;

/**
 * 分数 + 满分。
 *
 * · `score === undefined` → 还在后台拉
 * · `score === null`      → 拉过了但拿不到（试卷未公布等）
 * · 数字                  → 实际得分
 *
 * 满分取自**列表**的 `score` 字段（平台这个字段是满分，不是实际分）。
 */
interface ScoreInfo { score: number | null | undefined; max: number }

// 会话内缓存：分数落 sessionStorage，试卷只留内存
const scoreCache = new Map<string, number | null>();
const paperCache = new Map<string, ExamPaper>();

function loadStoredScores(accountId: number): Map<string, number | null> {
  const out = new Map<string, number | null>();
  try {
    const raw = sessionStorage.getItem(`exam-scores:${accountId}`);
    if (raw) for (const [k, v] of Object.entries(JSON.parse(raw) as Record<string, number | null>)) out.set(k, v);
  } catch { /* 存储不可用就当没有缓存 */ }
  return out;
}
function saveStoredScores(accountId: number) {
  try {
    const obj: Record<string, number | null> = {};
    for (const [k, v] of scoreCache) obj[k] = v;
    sessionStorage.setItem(`exam-scores:${accountId}`, JSON.stringify(obj));
  } catch { /* 配额满了就放弃持久化，不影响功能 */ }
}

/** 单道题：选项用 A/B/C… 标，正确项绿、选错红。 */
function QuestionCard({ q }: { q: ExamQuestion }) {
  const mine = new Set(q.candidateAnswer ?? []);
  const right = new Set(q.standardAnswer ?? []);
  const isChoice = (q.choices?.length ?? 0) > 0;
  const judged = right.size > 0;
  const full =
    judged && mine.size > 0 &&
    [...right].every((x) => mine.has(x)) && [...mine].every((x) => right.has(x));
  const got = q.candidateScore;
  const tone = !judged
    ? 'var(--muted)'
    : full ? 'var(--ok)' : got > 0 ? 'var(--warn)' : 'var(--danger)';

  return (
    <div className="exam-q">
      <div className="exam-q-head">
        <span className="exam-q-no">第 {q.questionNo} 题</span>
        <span className="badge badge-muted">{typeLabel(q.questionType)}</span>
        <span className="exam-q-score" style={{ color: tone }}>
          {fmtNum(got)} / {fmtNum(q.questionScore)} 分
        </span>
      </div>
      <p className="exam-q-stem">{q.stemText || stripTags(q.stem)}</p>

      {isChoice ? (
        <ul className="exam-choices">
          {q.choices.map((c, i) => {
            const picked = mine.has(c.choiceId);
            const correct = right.has(c.choiceId);
            const cls = !judged
              ? picked ? 'is-picked' : ''
              : correct
                ? picked ? 'is-right' : 'is-correct'
                : picked ? 'is-wrong' : '';
            return (
              <li key={c.choiceId} className={`exam-choice ${cls}`}>
                <span className="exam-choice-letter">{LETTERS[i] ?? '?'}</span>
                <span className="exam-choice-text">{c.choiceContent}</span>
                <span className="exam-choice-flags">
                  {picked && <span className="badge badge-run">我的选择</span>}
                  {judged && correct && <span className="badge badge-ok">正确答案</span>}
                </span>
              </li>
            );
          })}
        </ul>
      ) : (
        <>
          {mine.size > 0 && (
            <p className="exam-answer"><span className="muted">我的答案：</span>{q.candidateAnswer.join(' ')}</p>
          )}
          {right.size > 0 && (
            <p className="exam-answer"><span className="muted">参考答案：</span>{q.standardAnswer.join(' ')}</p>
          )}
        </>
      )}
      {q.analysis && (
        <p className="exam-analysis"><span className="muted">解析：</span>{q.analysis}</p>
      )}
    </div>
  );
}

/** 右栏：一份试卷。 */
function PaperView({
  paper, loading, error, onRetry,
}: {
  paper: ExamPaper | null;
  loading: boolean;
  error: string;
  onRetry: () => void;
}) {
  if (loading) return <p className="muted">正在拉取试卷…</p>;
  if (error) {
    return (
      <div>
        <ErrorState reason={`取不到试卷：${error}`} />
        <div style={{ marginTop: 8 }}><button className="btn btn-sm" onClick={onRetry}>重试</button></div>
      </div>
    );
  }
  if (!paper) return <p className="muted" style={{ margin: 0 }}>从左侧选一场考试，这里显示题目、答案与得分。</p>;

  const p = paper.resExamCandidatePaper;
  const parts = p.paperParts ?? [];
  const qty = parts.reduce((s, x) => s + (x.paperQuestionList?.length ?? 0), 0);

  return (
    <div>
      <div className="exam-paper-head">
        <h3 className="exam-paper-title">{p.examName}</h3>
        <div className="exam-paper-stats">
          <div><span className="stat-value" style={{ color: 'var(--ok)' }}>{fmtNum(paper.calculateScore)}</span>
            <span className="stat-label"> / {fmtNum(p.paperScore)} 分</span></div>
          <div className="muted small">客观 {fmtNum(p.objectiveScore)} · 主观 {fmtNum(p.subjectiveScore)} · 共 {qty} 题</div>
          <div className="muted small">
            交卷 {p.submitTime ? fmtTs(p.submitTime) : '—'}
            {p.markCompleteTime ? ` · 批阅完成 ${fmtTs(p.markCompleteTime)}` : ''}
          </div>
        </div>
      </div>

      {parts.map((part) => (
        <section key={part.partId} className="exam-part">
          <h4 className="exam-part-title">
            {part.partName}
            <span className="muted small"> · {part.partDesc} · {part.paperQuestionList?.length ?? 0} 题</span>
          </h4>
          {(part.paperQuestionList ?? []).map((row) => (
            <QuestionCard key={`${part.partId}-${row.paperQuestion.questionNo}`} q={row.paperQuestion} />
          ))}
        </section>
      ))}
    </div>
  );
}

export default function ExamsPanel({ accountId }: { accountId: number }) {
  const [items, setItems] = useState<ExamSummary[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [scores, setScores] = useState<Map<string, number | null>>(new Map());
  const [openKey, setOpenKey] = useState<string | null>(null);
  const [paper, setPaper] = useState<ExamPaper | null>(null);
  const [paperLoading, setPaperLoading] = useState(false);
  const [paperError, setPaperError] = useState('');
  /** 防止切账号后旧请求把状态写回来 */
  const aliveRef = useRef(true);

  // ── 列表 + 后台补分数 ──
  const loadList = useCallback(async () => {
    setLoading(true); setError('');
    try {
      const d = await api.exams(accountId);
      if (!aliveRef.current) return;
      setItems(d.items ?? []); setTotal(d.total ?? 0);
      // 先上会话缓存，再对缺的做后台并发补齐
      const cached = loadStoredScores(accountId);
      for (const [k, v] of cached) scoreCache.set(k, v);
      setScores(new Map(scoreCache));

      const missing = (d.items ?? []).filter((e) => !scoreCache.has(examKey(e)));
      void fillScores(accountId, missing);
    } catch (e) {
      if (aliveRef.current) setError((e as Error).message);
    } finally {
      if (aliveRef.current) setLoading(false);
    }
  }, [accountId]);

  /** 并发 3 逐场拉试卷，只取分数（顺手把试卷留在内存里，点开就不用再拉）。 */
  const fillScores = useCallback(async (accId: number, todo: ExamSummary[]) => {
    let i = 0;
    const worker = async () => {
      while (i < todo.length) {
        const e = todo[i++];
        const key = examKey(e);
        const fetchPaper = async () => {
          const p = await api.examPaper(accId, e.examId, e.candidateId);
          paperCache.set(key, p);
          scoreCache.set(key, p.calculateScore);
        };
        try {
          await fetchPaper();
        } catch {
          // 偶发的 401（token 刚被吊销）重试一次
          let ok = false;
          for (let n = 0; n < SCORE_RETRY && !ok; n += 1) {
            await new Promise((r) => setTimeout(r, 400));
            try { await fetchPaper(); ok = true; } catch { /* 继续下一次 */ }
          }
          // 真的拿不到就标 null（显示「—」），不让它一直在「…」转
          if (!ok) scoreCache.set(key, null);
        }
        if (!aliveRef.current) return;
        setScores(new Map(scoreCache));
      }
      if (aliveRef.current) saveStoredScores(accId);
    };
    await Promise.all(Array.from({ length: Math.min(SCORE_CONCURRENCY, todo.length) }, worker));
  }, []);

  /** 刷新：清掉本账号已缓存的分数，再重新拉列表 + 后台补分。 */
  const refreshAll = useCallback(async () => {
    for (const e of items) {
      const k = examKey(e);
      scoreCache.delete(k);
      paperCache.delete(k);
    }
    saveStoredScores(accountId);
    setScores(new Map(scoreCache));
    await loadList();
  }, [items, accountId, loadList]);

  // ── 选中某场：优先用后台已经拉过的试卷 ──
  const selectExam = useCallback(
    async (e: ExamSummary) => {
      const key = examKey(e);
      setOpenKey(key); setPaperError('');
      const hit = paperCache.get(key);
      if (hit) { setPaper(hit); return; }
      setPaper(null); setPaperLoading(true);
      try {
        const p = await api.examPaper(accountId, e.examId, e.candidateId);
        if (!aliveRef.current) return;
        paperCache.set(key, p);
        scoreCache.set(key, p.calculateScore);
        setScores(new Map(scoreCache));
        setPaper(p);
      } catch (err) {
        if (aliveRef.current) setPaperError((err as Error).message);
      } finally {
        if (aliveRef.current) setPaperLoading(false);
      }
    },
    [accountId],
  );

  useEffect(() => {
    aliveRef.current = true;
    void loadList();
    return () => { aliveRef.current = false; };
  }, [loadList]);

  /** 列表里显示「实际分 / 满分」。满分的来源：列表 `score` 字段。 */
  const scoreOf = (e: ExamSummary): ScoreInfo => {
    const key = examKey(e);
    const max = Number(e.score) || 0;
    // 后台补分是渐进式的：没进 Map 就是还在拉
    return { score: scores.has(key) ? (scores.get(key) ?? null) : undefined, max };
  };

  const sorted = useMemo(() => items, [items]);

  return (
    <div className="exams-split">
      {/* ── 左栏：考试列表 ── */}
      <div className="card exam-list-card">
        <h2 className="card-title">
          考试列表
          <span className="card-meta">{loading ? '加载中…' : error ? '取不到' : `${total} 场`}</span>
          <button className="btn btn-sm" style={{ marginLeft: 'auto' }} onClick={() => void refreshAll()} disabled={loading}
            title="重新拉取考试列表并重取每场的实际得分">
            {loading ? '刷新中…' : '刷新'}
          </button>
        </h2>

        {error ? (
          <div>
            <ErrorState reason={`取不到考试列表：${error}`} />
            <p className="muted small" style={{ textAlign: 'center', marginTop: 8 }}>
              这是<strong>取不到数据</strong>，不是「没有考试」。
            </p>
            <div style={{ textAlign: 'center', marginTop: 8 }}>
              <button className="btn btn-sm" onClick={() => void loadList()}>重试</button>
            </div>
          </div>
        ) : loading && sorted.length === 0 ? (
          <p className="muted small">正在从平台拉取考试列表…</p>
        ) : sorted.length === 0 ? (
          <p className="muted small" style={{ margin: 0 }}>这个账号还没有已交卷的考试。</p>
        ) : (
          <ul className="exam-list">
            {sorted.map((e) => {
              const key = examKey(e);
              const info = scoreOf(e);
              const active = openKey === key;
              return (
                <li key={key}>
                  <button className={`exam-list-item ${active ? 'is-active' : ''}`}
                    onClick={() => void selectExam(e)} title={e.examName}>
                    <span className="exam-item-name">{e.examName}</span>
                    <span className="exam-item-meta">
                      {fmtTs(e.startTime)} · {fmtDur(e.duration)}
                    </span>
                    <span className="exam-item-score">
                      {info.score === undefined
                        ? <span className="muted" title="正在拉取实际得分">…</span>
                        : info.score === null
                          ? <span className="muted" title="取不到这场试卷">—</span>
                          : <b>{fmtNum(info.score)}</b>}
                      <span className="muted"> / {info.max || '—'}</span>
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      {/* ── 右栏：试卷 ── */}
      <div className="card exam-paper-card">
        <h2 className="card-title">
          试卷详情
          <span className="card-meta">{openKey ? '' : '从左侧选择'}</span>
        </h2>
        <PaperView
          paper={paper}
          loading={paperLoading}
          error={paperError}
          onRetry={() => {
            const e = sorted.find((x) => examKey(x) === openKey);
            if (e) void selectExam(e);
          }}
        />
      </div>
    </div>
  );
}
