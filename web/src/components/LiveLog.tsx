/**
 * 实时日志（全局流的视图）。
 *
 * 放在「运行日志」页顶部 —— 那一页本来就是看日志的地方，
 * 而它现在只显示**已经结束**的执行，正在跑的要靠手动刷新。
 *
 * 用全局流而不是单执行的流：任务有三个来源（手动/调度器/考勤监控），
 * 后两个的 execution_id 前端拿不到。
 */
import { useEffect, useRef, useState } from 'react';
import { useLogStream } from '../lib/useLogStream';

interface Props {
  /** 只看某个账号；不传则全部 */
  accountId?: number | null;
}

const LEVEL_CLASS: Record<string, string> = {
  ok: 'is-ok',
  error: 'is-err',
  warn: 'is-warn',
  info: 'is-info',
};

export default function LiveLog({ accountId = null }: Props) {
  const { lines, connected, clear } = useLogStream(accountId);
  const [paused, setPaused] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);
  // 暂停时**仍然接收**（不断流），只是不自动滚 —— 断流会丢日志
  const shown = lines;

  useEffect(() => {
    if (!paused) boxRef.current?.scrollTo({ top: boxRef.current.scrollHeight });
  }, [shown, paused]);

  return (
    <div className="card mt-16">
      <h2 className="card-title">
        实时日志
        <span className="card-meta">
          {connected ? '● 已连接' : '○ 连接中断（任务可能仍在后台继续）'} · {shown.length} 条
        </span>
      </h2>
      <div className="row" style={{ gap: 6, marginBottom: 8 }}>
        <button className="btn btn-sm" onClick={() => setPaused((p) => !p)}>
          {paused ? '继续滚动' : '暂停滚动'}
        </button>
        <button className="btn btn-sm" onClick={clear} disabled={shown.length === 0}>清空</button>
        <span className="muted small">
          显示所有来源的任务（手动 / 调度器 / 考勤监控）
        </span>
      </div>
      <div className="log-panel" ref={boxRef} style={{ height: 260 }}>
        {shown.length === 0 ? (
          <div className="muted small" style={{ padding: 8 }}>
            暂时没有新日志。触发一个任务就会出现在这里。
          </div>
        ) : (
          shown.map((l) => (
            <div key={l.id} className={`log-line ${LEVEL_CLASS[l.level] ?? 'is-info'}`}>
              <span className="log-time">
                {new Date(l.ts).toLocaleTimeString('zh-CN', { hour12: false })}
              </span>
              <span className="muted small" style={{ minWidth: 52 }}>#{l.account_id}</span>
              <span>{l.message}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
