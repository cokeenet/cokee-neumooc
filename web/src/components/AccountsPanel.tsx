/**
 * 账号面板：列表、添加、删除、改密、重新登录。
 *
 * 对齐 同类平台 约定（UI-SPEC.md）：
 *   · **不用原生 confirm/prompt/alert** —— 删除用 InlineConfirm、
 *     改密用 InlinePrompt，两者内联在操作列里
 *   · 变更请求走 useCall（永不抛、优先服务端文案、成功后刷新、finally 归零 busy）
 *   · 状态只用四种徽标；幂等命中是 warn 不是 err
 */
import { Fragment, useCallback, useState } from 'react';
import { MSG_CLASS } from '../lib/msg';
import { api } from '../lib/api';
import { useResource } from '../lib/useResource';
import ErrorState from 'korikura-webui/components/ui/ErrorState';
import { InlineConfirm, InlinePrompt } from 'korikura-webui/components/InlineDialog';
import { useCall, type Msg } from '../lib/useCall';
import { accountState } from '../lib/labels';



export default function AccountsPanel() {

  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [label, setLabel] = useState('');

  // 一个 useResource 取代 items/loading/load/useEffect 四件套
  const load = useCallback(() => api.accounts(), []);
  // ⚠️ `error` 必须解构出来 —— 原先是 `{ data, loading, reload }`，
  // 后端一挂 `data` 保持 null，页面就走进「还没有账号」分支，
  // 引导用户去添加账号。**界面在陈述一件假事。**
  // `lib/backend.ts` 的注释专门批评过这个行为，但只修在 SSR 层。
  const { data, loading, error, reload } = useResource(load);
  const items = data ?? [];
  // ⚠️ 这一行被删过一次 —— 重构时把 `useCall(load)` 那段整块替换掉了，
  // 而 `msg` / `busy` / `call` 的声明就在里面。**构建照样通过**，
  // 直到 SSR 渲染才抛 `ReferenceError: msg is not defined`。
  const { call, busy, msg, setMsg } = useCall(reload);
  /**
   * **消息落在哪一行**。
   *
   * 原先是一个全局 `busy` + 页面顶部一条 `msg`：20 个账号时，
   * 第 15 行操作失败的提示出现在页面顶部 —— 用户看不到，
   * 而且下一次任何操作都会把它覆盖掉。
   *
   * 现在记住是哪一行发起的，消息就渲染在那一行；按钮也只禁用那一行。
   */
  const [rowId, setRowId] = useState<number | null>(null);
  async function rowCall(id: number, url: string, init: RequestInit, okText: string) {
    setRowId(id);
    return call(url, init, okText);
  }

  async function onAdd(e: React.FormEvent) {
    e.preventDefault();
    // 原先直接 return —— 三个框全空时点「添加并验证」**什么都不会发生**：
    // 没有请求、没有提示、界面一个字都不变。必须说出来。
    if (!username || !password) {
      setMsg({ kind: 'err', text: '请先填写学号和密码' });
      return;
    }
    const r = await call('/api/accounts', {
      method: 'POST',
      body: JSON.stringify({ username, password, label: label || undefined }),
    }, `已添加账号 ${username}`);
    if (r.ok) { setUsername(''); setPassword(''); setLabel(''); }
  }

  return (
    <>
      {msg && <div className={`msg-bar ${MSG_CLASS[msg.kind]}`}>{msg.text}</div>}

      <div className="card">
        <h2 className="card-title">添加账号</h2>
        <form onSubmit={onAdd}>
          <div className="form-row">
            <div className="field">
              <label htmlFor="u">学号</label>
              <input id="u" className="input" value={username} autoComplete="off"
                placeholder="如 00000000000"
                onChange={(e) => setUsername(e.target.value)} />
            </div>
            <div className="field">
              <label htmlFor="p">密码</label>
              <input id="p" className="input" type="password" value={password}
                autoComplete="new-password" placeholder="登录密码"
                onChange={(e) => setPassword(e.target.value)} />
            </div>
            <div className="field">
              <label htmlFor="l">备注名（可选）</label>
              <input id="l" className="input" value={label} placeholder="留空用学号"
                onChange={(e) => setLabel(e.target.value)} />
            </div>
            <button className="btn btn-primary" type="submit" disabled={busy}>
              {busy ? '处理中…' : '添加并验证'}
            </button>
          </div>
        </form>
        <p className="muted small">
          租户自动解析，无需手动填写。
        </p>
      </div>

      <div className="card mt-24">
        <h2 className="card-title">
          已接入账号
          <span className="card-meta">共 {items.length} 个</span>
        </h2>
        {loading ? (
          // **保留表头** —— 原先是整块替换成一行「加载中…」，
          // 标题、计数、表头全消失，数据回来后布局跳一下。
          // 加载态的目的是「让用户知道稍等」，不是「让页面消失」。
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>#</th><th>备注</th><th>学号</th><th>状态</th>
                  <th>课程</th><th>设备 ID</th><th>操作</th>
                </tr>
              </thead>
              <tbody>
                <tr><td colSpan={6} className="muted" style={{ padding: '18px 8px' }}>加载中…</td></tr>
              </tbody>
            </table>
          </div>
        ) : error ? (
          // 「取不到数据」与「没有数据」必须分开 —— 用户下一步动作完全不同
          <div>
            <ErrorState reason={`取不到账号列表：${error}`} />
            <p className="muted small" style={{ textAlign: 'center', marginTop: 8 }}>
              这是<strong>取不到数据</strong>，不是「没有账号」。
            </p>
            <div style={{ textAlign: 'center', marginTop: 8 }}>
              <button className="btn btn-sm" onClick={() => void reload()}>重试</button>
            </div>
          </div>
        ) : items.length === 0 ? (
          <p className="muted">还没有账号。</p>
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>#</th><th>备注</th><th>学号</th><th>状态</th>
                  <th>课程</th><th>设备 ID</th><th>操作</th>
                </tr>
              </thead>
              <tbody>
                {items.map((a) => {
                  const ast = accountState(a.state);
                  return (
                    <Fragment key={a.id}>
                    <tr>
                      <td>{a.id}</td>
                      <td>{a.label}</td>
                      <td className="mono">{a.username}</td>
                      <td><span className={`badge badge-${ast.variant}`}>{ast.text}</span></td>
                      <td>{a.courses}</td>
                      <td className="small">
                        {a.device_id_enabled
                          ? <span className="badge badge-ok" title="签到会携带 deviceId">开</span>
                          : <span className="badge badge-muted" title="签到不携带 deviceId">关</span>}
                        {a.device_id ? (
                          <span className="mono" title={a.device_id}
                            style={{ fontSize: 11, color: 'var(--muted)', marginLeft: 6 }}>
                            {a.device_id.slice(0, 8)}…
                          </span>
                        ) : (
                          <span className="muted" style={{ fontSize: 11, marginLeft: 6 }}>无 ID</span>
                        )}
                      </td>
                      <td>
                        <div className="row" style={{ gap: 6 }}>
                          <button className="btn btn-sm" disabled={busy && rowId === a.id}
                            onClick={() => void rowCall(a.id,
                              `/api/accounts/${a.id}/exec`,
                              { method: 'POST', body: JSON.stringify({ cmd: 'login' }) },
                              '已重新登录',
                            )}>
                            重新登录
                          </button>

                          <InlinePrompt
                            label="备注"
                            minLength={1}
                            placeholder={a.label}
                            confirmLabel="保存"
                            disabled={busy && rowId === a.id}
                            onSubmit={(v: string) => rowCall(a.id,
                              `/api/accounts/${a.id}`,
                              { method: 'PATCH', body: JSON.stringify({ label: v }) },
                              '备注已更新',
                            )}
                          />

                          <InlinePrompt
                            label="改密"
                            type="password"
                            minLength={8}
                            placeholder="至少 8 位"
                            confirmLabel="保存"
                            disabled={busy && rowId === a.id}
                            onSubmit={(pw: string) => rowCall(a.id, 
                              `/api/accounts/${a.id}`,
                              { method: 'PATCH', body: JSON.stringify({ password: pw }) },
                              '密码已更新',
                            )}
                          />

                          <button
                            className={`btn btn-sm${a.device_id_enabled ? '' : ' btn-primary'}`}
                            disabled={busy && rowId === a.id}
                            title={a.device_id_enabled
                              ? '点击关闭：签到不再携带 deviceId（ID 保留）'
                              : '点击开启：会生成/复用一个 UUID 并随签到提交'}
                            onClick={() => void rowCall(a.id,
                              `/api/accounts/${a.id}/device-id`,
                              { method: 'PUT', body: JSON.stringify({ enabled: !a.device_id_enabled }) },
                              !a.device_id_enabled ? '已开启设备 ID' : '已关闭设备 ID',
                            )}>
                            设备 ID {a.device_id_enabled ? '关' : '开'}
                          </button>

                          <InlinePrompt
                            label="设 ID"
                            placeholder={a.device_id || '填一个 UUID，或留空自动生成'}
                            confirmLabel="保存"
                            disabled={busy && rowId === a.id}
                            onSubmit={(v: string) => rowCall(a.id,
                              `/api/accounts/${a.id}`,
                              { method: 'PATCH', body: JSON.stringify(
                                v.trim() ? { device_id: v.trim() } : { regenerate_device_id: true }) },
                              v.trim() ? '设备 ID 已保存' : '已生成新设备 ID',
                            )}
                          />

                          <InlineConfirm
                            label="重生成"
                            danger
                            question={`重新生成「${a.label}」的设备 ID？历史签到记录里的 deviceId 会与新的不一致 —— 真机的 ID 装一次就固定，频繁变化本身就是异常信号。`}
                            confirmLabel="确认重生成"
                            disabled={busy && rowId === a.id}
                            onConfirm={() => rowCall(a.id,
                              `/api/accounts/${a.id}`,
                              { method: 'PATCH', body: JSON.stringify({ regenerate_device_id: true }) },
                              '已重新生成设备 ID',
                            )}
                          />

                          <InlineConfirm
                            label="删除"
                            danger
                            question={`删除「${a.label}」？该账号的课程与考勤缓存也会一并删除，且无法恢复。`}
                            // 不可逆操作要求把账号打出来 —— 「手滑」和「确认」要分开。
                            // 包里的 InlineDialog 支持 requireText，但一直没人用。
                            requireText={a.username}
                            confirmLabel="确认删除"
                            disabled={busy && rowId === a.id}
                            onConfirm={() => rowCall(a.id, 
                              `/api/accounts/${a.id}`, { method: 'DELETE' }, `已删除 ${a.label}`,
                            )}
                          />
                        </div>
                      </td>
                    </tr>
                    {/* 操作反馈落在**发起操作的那一行**，不是页面顶部 */}
                    {rowId === a.id && msg && (
                      <tr>
                        <td colSpan={6} style={{ paddingTop: 0 }}>
                          <div className={`msg-bar ${MSG_CLASS[msg.kind]}`} style={{ marginBottom: 0 }}>
                            {msg.text}
                          </div>
                        </td>
                      </tr>
                    )}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
            <p className="muted small" style={{ marginTop: 12 }}>
              令牌过期后平台会自动重新登录，不影响使用，无需手动处理。
            </p>
          </div>
        )}
      </div>
    </>
  );
}
