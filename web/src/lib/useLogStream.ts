/**
 * 订阅**全局**日志流。
 *
 * ## 为什么用全局流而不是「单次执行的流」
 *
 * 单执行的流（`/api/executions/:id/stream`）要求调用方**先知道 execution_id**。
 * 但任务有三个来源 —— 手动点、调度器、考勤监控 —— 后两个的 id 前端根本拿不到。
 * 结果就是「自动跑的任务没有任何进度可见」。
 *
 * 全局流（`/api/logs/stream`）没有这个问题：**订阅了就什么都能看到**，
 * 前端按 `account_id` 过滤即可。这是 同类平台 的做法（`RunPage.tsx:140-149`），
 * 也是它比我们强的最主要一条。
 *
 * ## ⚠️ 必须用 `addEventListener('log', …)` 而不是 `onmessage`
 *
 * 后端发的是**具名事件**（`event: log`）。`onmessage` 只接**不带事件名**的消息，
 * 所以用 `onmessage` 会一个字节都收不到，而且**不报错** ——
 * 表现为「连上了但永远没日志」。
 */
import { useCallback, useEffect, useRef, useState } from 'react';

export interface StreamLine {
  id: number;
  execution_id: number;
  account_id: number;
  job_key: string;
  level: string;
  message: string;
  ts: number;
}

/** 默认保留的行数。日志是流水，留太多会拖慢渲染。 */
const DEFAULT_MAX = 500;

export interface LogStream {
  lines: StreamLine[];
  /** SSE 连接是否活着。断了要告诉用户「任务可能仍在后台跑」。 */
  connected: boolean;
  clear: () => void;
}

/**
 * `accountId` 传 `null` 表示不过滤（看全部账号）。
 *
 * 断线由浏览器自动重连，并带上 `Last-Event-ID` —— 后端从那条之后继续推，
 * 不会重放也不会漏。
 */
export function useLogStream(accountId: number | null, max = DEFAULT_MAX): LogStream {
  const [lines, setLines] = useState<StreamLine[]>([]);
  const [connected, setConnected] = useState(false);
  // 用 ref 存过滤条件，避免它变化时重建连接（切账号不该断流）
  const filter = useRef(accountId);
  filter.current = accountId;

  useEffect(() => {
    const es = new EventSource('/api/logs/stream');
    es.onopen = () => setConnected(true);
    es.onerror = () => setConnected(false);

    const onLog = (ev: MessageEvent) => {
      let d: StreamLine;
      try {
        d = JSON.parse(ev.data) as StreamLine;
      } catch {
        return; // 非 JSON 忽略，不要把解析失败当日志显示
      }
      if (filter.current != null && d.account_id !== filter.current) return;
      setLines((p) => (p.length >= max ? [...p.slice(-(max - 1)), d] : [...p, d]));
    };

    // 具名事件（后端发的是 `event: log`）
    es.addEventListener('log', onLog as EventListener);
    return () => {
      es.removeEventListener('log', onLog as EventListener);
      es.close();
    };
  }, [max]);

  const clear = useCallback(() => setLines([]), []);
  return { lines, connected, clear };
}
