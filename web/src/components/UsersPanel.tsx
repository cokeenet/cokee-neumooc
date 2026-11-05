/**
 * 用户管理面板（仅 root 可见）。
 *
 * 三层角色，对齐 同类平台：
 *   root  超级管理员 —— 全部，含用户管理与绑定
 *   admin 管理员     —— 所有账号 + 调度；**不能**管平台用户
 *   user  普通用户   —— **只能**访问绑定给它的账号
 */
import { Fragment, useCallback, useState } from 'react';
import { MSG_CLASS } from '../lib/msg';
import { api } from '../lib/api';
import { useResource } from '../lib/useResource';
import ErrorState from 'korikura-webui/components/ui/ErrorState';
import { InlineConfirm, InlinePrompt } from 'korikura-webui/components/InlineDialog';
import { useCall, type Msg } from '../lib/useCall';

interface User {
  username: string;
  role: string;
  role_label?: string;
  created_at: number;
  account_ids?: number[];
}

const ROLES = [
  { value: 'user', label: '普通用户', hint: '只能访问绑定给它的账号' },
  { value: 'admin', label: '管理员', hint: '所有账号 + 调度；不能管平台用户' },
  { value: 'root', label: '超级管理员', hint: '全部权限，含用户管理' },
];


function roleBadge(role: string): string {
  return role === 'root' ? 'badge-ok' : role === 'admin' ? 'badge-run' : 'badge-muted';
}

export default function UsersPanel({ me }: { me: string }) {
  const [bindUser, setBindUser] = useState<string | null>(null);
  const [bound, setBound] = useState<number[]>([]);

  const [nName, setNName] = useState('');
  const [nPass, setNPass] = useState('');
  const [nRole, setNRole] = useState('user');

  // 一个 useResource 取代 users/accounts/loading/load/useEffect。
  const load = useCallback(async () => {
    const [users, accounts] = await Promise.all([api.users(), api.accounts()]);
    return { users, accounts };
  }, []);
  // error 必须解构 —— 丢弃它会把「后端挂了」显示成「还没有用户」
  const { data, loading, error, reload } = useResource(load);
  const users = data?.users ?? [];
  const accounts = data?.accounts ?? [];

  const { call, busy, msg } = useCall(reload);

  async function onAdd(e: React.FormEvent) {
    e.preventDefault();
    if (!nName || nPass.length < 8) return;
    const r = await call('/api/users', {
      method: 'POST',
      body: JSON.stringify({ username: nName, password: nPass, role: nRole }),
    }, `已创建用户 ${nName}`);
    if (r.ok) { setNName(''); setNPass(''); setNRole('user'); }
  }

  function openBind(u: User) {
    if (bindUser === u.username) { setBindUser(null); return; }
    setBindUser(u.username);
    setBound(u.account_ids ?? []);
  }

  function toggle(id: number) {
    setBound((p) => (p.includes(id) ? p.filter((x) => x !== id) : [...p, id]));
  }

  async function saveBind() {
    if (!bindUser) return;
    await call(`/api/users/${encodeURIComponent(bindUser)}`, {
      method: 'PATCH',
      body: JSON.stringify({ account_ids: bound }),
    }, `已更新 ${bindUser} 的账号绑定`);
    setBindUser(null);
  }

  const rootCount = users.filter((u) => u.role === 'root').length;

  return (
    <>
      {msg && <div className={`msg-bar ${MSG_CLASS[msg.kind]}`}>{msg.text}</div>}

      <div className="card">
        <h2 className="card-title">新建用户</h2>
        <form onSubmit={onAdd}>
          <div className="form-row">
            <div className="field">
              <label htmlFor="un">用户名</label>
              <input id="un" className="input" value={nName} autoComplete="off"
                onChange={(e) => setNName(e.target.value)} />
            </div>
            <div className="field">
              <label htmlFor="up">密码（至少 8 位）</label>
              <input id="up" className="input" type="password" value={nPass}
                autoComplete="new-password"
                onChange={(e) => setNPass(e.target.value)} />
            </div>
            <div className="field">
              <label htmlFor="ur">角色</label>
              <select id="ur" className="input" value={nRole}
                onChange={(e) => setNRole(e.target.value)}>
                {ROLES.map((r) => <option key={r.value} value={r.value}>{r.label}</option>)}
              </select>
            </div>
            <button className="btn btn-primary" type="submit" disabled={busy || nPass.length < 8}>
              {busy ? '处理中…' : '创建'}
            </button>
          </div>
        </form>
        <p className="muted small">
          {ROLES.map((r) => `${r.label}：${r.hint}`).join('　·　')}
        </p>
      </div>

      <div className="card mt-24">
        <h2 className="card-title">
          用户列表
          <span className="card-meta">共 {users.length} 个</span>
        </h2>
        {loading ? (
          // 保留表头（同 AccountsPanel：加载态不该让页面结构消失）
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>用户名</th><th>角色</th><th>可见账号</th><th>创建时间</th><th>操作</th>
                </tr>
              </thead>
              <tbody>
                <tr><td colSpan={5} className="muted" style={{ padding: '18px 8px' }}>加载中…</td></tr>
              </tbody>
            </table>
          </div>
        ) : error ? (
          // 「取不到」与「没有」分开 —— 后端挂了不该显示成「还没有用户」
          <div>
            <ErrorState reason={`取不到用户列表：${error}`} />
            <p className="muted small" style={{ textAlign: 'center', marginTop: 8 }}>
              这是<strong>取不到数据</strong>，不是「没有用户」。
            </p>
            <div style={{ textAlign: 'center', marginTop: 8 }}>
              <button className="btn btn-sm" onClick={() => void reload()}>重试</button>
            </div>
          </div>
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>用户名</th><th>角色</th><th>可见账号</th><th>创建时间</th><th>操作</th>
                </tr>
              </thead>
              <tbody>
                {users.map((u) => {
                  const isLastRoot = u.role === 'root' && rootCount <= 1;
                  return (
                    // key 必须在 map 返回的那个元素上。原先挂在里面的 <tr> 上，
                    // Fragment 本身没有 key —— React 会报 "each child in a list…" 警告，
                    // 列表复用时也更容易出错。
                    <Fragment key={u.username}>
                      <tr>
                        <td>
                          {u.username}
                          {u.username === me && <span className="badge badge-muted" style={{ marginLeft: 6 }}>我</span>}
                        </td>
                        <td>
                          <span className={`badge ${roleBadge(u.role)}`}>
                            {u.role_label ?? u.role}
                          </span>
                        </td>
                        <td>
                          {u.role === 'user'
                            ? `${(u.account_ids ?? []).length} 个`
                            : <span className="muted small">全部</span>}
                        </td>
                        <td className="mono small">
                          {u.created_at ? new Date(u.created_at).toLocaleDateString('zh-CN') : '-'}
                        </td>
                        <td>
                          <div className="row" style={{ gap: 6 }}>
                            <button className="btn btn-sm" disabled={busy} onClick={() => openBind(u)}>
                              {bindUser === u.username ? '收起绑定' : '绑定账号'}
                            </button>

                            <InlinePrompt
                              label="改密"
                              type="password"
                              minLength={8}
                              placeholder="至少 8 位"
                              confirmLabel="保存"
                              disabled={busy}
                              onSubmit={(pw: string) => call(
                                `/api/users/${encodeURIComponent(u.username)}`,
                                { method: 'PATCH', body: JSON.stringify({ password: pw }) },
                                '密码已更新',
                              )}
                            />

                            <InlineConfirm
                              label="删除"
                              danger
                              question={`删除用户「${u.username}」？其会话与账号绑定会一并清除，且无法恢复。`}
                              requireText={u.username}
                              confirmLabel="确认删除"
                              disabled={busy || u.username === me || isLastRoot}
                              onConfirm={() => call(
                                `/api/users/${encodeURIComponent(u.username)}`,
                                { method: 'DELETE' },
                                `已删除 ${u.username}`,
                              )}
                            />
                          </div>
                          {isLastRoot && <span className="muted small">最后一个超管，不可删除</span>}
                        </td>
                      </tr>

                      {bindUser === u.username && (
                        <tr key={`${u.username}-bind`}>
                          <td colSpan={5} style={{ background: 'var(--bg)' }}>
                            <div className="row" style={{ gap: 12, alignItems: 'flex-start' }}>
                              <span className="small" style={{ minWidth: 90, paddingTop: 6 }}>
                                可见账号：
                              </span>
                              <div className="row" style={{ gap: 8, flex: 1 }}>
                                {accounts.length === 0 ? (
                                  <span className="muted small">还没有账号。先到「账号」页添加。</span>
                                ) : accounts.map((a) => (
                                  <label key={a.id} className="row" style={{ gap: 4 }}>
                                    <input
                                      type="checkbox"
                                      checked={bound.includes(a.id)}
                                      onChange={() => toggle(a.id)}
                                    />
                                    <span className="small">{a.label}（{a.username}）</span>
                                  </label>
                                ))}
                              </div>
                              <button className="btn btn-sm btn-primary" onClick={() => void saveBind()} disabled={busy}>
                                保存
                              </button>
                            </div>
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </>
  );
}
