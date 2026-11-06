/**
 * 执行详情（按需加载）。
 *
 * 补的是一个**诊断死路**：`GET /api/executions/{id}/logs` 早就实现好了，
 * 但全站没有任何页面调用它 —— 于是任务跑完（或刷新页面）之后，
 * 那次执行留下的日志在界面上**永久不可达**，失败时只能看到被截断的
 * `{"error": …}`，根本没法排查。
 *
 * 按需拉取：日志可能几百条，给表格里 40 行都预先取一遍太浪费，
 * 所以点开才请求。
 */
import { useState } from 'react';
import { api } from '../lib/api';
import { fmtTime } from '../lib/labels';

interface LogRow { id: number; level: string; message: string; ts: number }

interface Props {
  execId: number;
  /** executions.detail 原文（可能很长，表格里只显示摘要） */
  detail: string;
  /** 默认是否展开 */
  defaultOpen?: boolean;
}

export default function LogDetail({ execId, detail, defaultOpen = false }: Props) {
  const [open, setOpen] = useState(defaultOpen);
  const [logs, setLogs] = useState<LogRow[] | null>(null);
  const [err, setErr] = useState('');
  const [loading, setLoading] = useState(false);

  async function toggle() {
    const next = !open;
    setOpen(next);
    if (!next || logs !== null || loading) return;
    setLoading(true);
    try {
      // 走 api.ts 而不是直接 fetch —— 统一的错误处理在那里
      // （尤其是「HTTP 200 + body 里 ok:false」这种后端失败）
      setLogs(await api.logs(execId));
    } catch (e) {
      setErr((e as Error).message);
      setLogs([]);
    } finally {
      setLoading(false);
    }
  }

  const hasDetail = detail && detail !== '{}';

  return (
    <div style={{ marginTop: 4 }}>
      <button type="button" className="btn btn-sm" onClick={() => void toggle()}>
        {open ? '收起' : '详情'}
      </button>

      {open && (
        <div style={{ marginTop: 8 }}>
          {hasDetail && (
            <>
              <div className="muted small">原始返回</div>
              <pre className="log-detail">{detail}</pre>
            </>
          )}
          <div className="muted small" style={{ marginTop: 8 }}>
            执行日志{logs ? ` · ${logs.length} 条` : ''}
          </div>
          {loading && <p className="muted small">加载中…</p>}
          {err && (
            <div className="msg-bar msg-err small">
              日志加载失败：{err}
              <button
                className="btn btn-sm"
                style={{ marginLeft: 8 }}
                onClick={() => { setErr(''); setLogs(null); void toggle(); }}
              >
                重试
              </button>
            </div>
          )}
          {logs && logs.length === 0 && !err && (
            <p className="muted small">这次执行没有留下日志。</p>
          )}
          {logs && logs.length > 0 && (
            <div className="log-panel" style={{ height: 260, marginTop: 6 }}>
              {logs.map((l) => (
                <div
                  key={l.id}
                  // 用包里的 `log-line is-*` —— `log-${level}` 这组 class
                  // 包里根本没有，只有颜色没有行内布局
                  className={`log-line ${
                    l.level === 'ok' ? 'is-ok' : l.level === 'error' ? 'is-err'
                    : l.level === 'warn' ? 'is-warn'
                    : l.level === 'think' ? 'is-think'
                    : l.level === 'answer' ? 'is-answer' : 'is-info'
                  }`}
                >
                  <span className="log-time">{l.ts ? fmtTime(l.ts) : ''}</span>
                  <span>{l.message}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
