/**
 * 订阅一次执行的实时日志（SSE）。
 *
 * ## 为什么需要它
 *
 * AI 代答是**长任务**（30~60 秒），期间后端把思考/答案增量写进 logs 表
 * （见 `tasks.rs::LogSink`）。没有这个 hook，界面只能干等，用户不知道
 * 它是在思考、还是在卡死。
 *
 * ## 为什么按执行 id 订阅，而不是全局日志流
 *
 * 全局流是「所有账号的所有日志」，混着刷课/考勤的输出。这里只关心
 * **本次 AI 作答**，按 `execution_id` 过滤才不会串台。
 *
 * ## 结束条件
 *
 * 收到 `state` 事件里的终态（done/failed/cancelled）或 `end` 就关闭连接
 * —— 否则 EventSource 会自动重连，把已结束的执行反复拉一遍。
 */
import { useEffect, useRef, useState } from 'react';

export interface ExecLine {
  id: number;
  level: string;
  message: string;
  ts: number;
}

export interface ExecStream {
  lines: ExecLine[];
  /** 执行是否已结束 */
  done: boolean;
  /** 实时累计的思考与答案（按 level 分的两个缓冲） */
  thinking: string;
  answer: string;
}

const MAX = 300;

export function useExecStream(execId: number | null): ExecStream {
  const [lines, setLines] = useState<ExecLine[]>([]);
  const [done, setDone] = useState(false);
  const esRef = useRef<EventSource | null>(null);

  useEffect(() => {
    setLines([]);
    setDone(false);
    if (!execId) return;

    const es = new EventSource(`/api/executions/${execId}/stream`);
    esRef.current = es;

    const close = () => {
      es.close();
      esRef.current = null;
    };

    es.addEventListener('log', (ev) => {
      try {
        const d = JSON.parse((ev as MessageEvent).data) as ExecLine;
        setLines((prev) => {
          const next = [...prev, d];
          return next.length > MAX ? next.slice(-MAX) : next;
        });
      } catch { /* 忽略坏行，不要因为一条日志把整条流搞死 */ }
    });

    es.addEventListener('state', (ev) => {
      try {
        const d = JSON.parse((ev as MessageEvent).data) as { state?: string };
        if (d.state && ['done', 'failed', 'cancelled'].includes(d.state)) {
          setDone(true);
          close();
        }
      } catch { /* 同上 */ }
    });

    es.addEventListener('end', () => {
      setDone(true);
      close();
    });

    es.onerror = () => {
      // 服务端关流也会触发 error —— 执行已结束时不必重连
      if (es.readyState === EventSource.CLOSED) close();
    };

    return close;
  }, [execId]);

  // 思考与答案分开聚合：界面上要能折叠思考（它很长，且不是最终结果）
  const thinking = lines.filter((l) => l.level === 'think').map((l) => l.message).join('\n');
  const answer = lines.filter((l) => l.level === 'answer').map((l) => l.message).join('\n');

  return { lines, done, thinking, answer };
}
