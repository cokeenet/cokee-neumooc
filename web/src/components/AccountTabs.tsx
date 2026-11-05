/**
 * 账号切换条（React 版）。
 *
 * ⚠️ 与 `AccountTabs.astro` 是同一套标记，改一个必须改另一个。
 * 之所以存在两份：SSR 页面要求零客户端 JS，用 .astro 版；
 * 而刷课面板需要在**任务运行中禁用切换**（切页会丢实时日志），
 * 这要求它是 React 组件、能读 running 状态。
 */

import type { Account } from '../lib/api';
import { accountHref, groupByDept } from '../lib/accountTabs';

interface Props {
  accounts: Account[];
  current: number;
  path: string;
  /** 任务运行中时禁止切换 */
  disabled?: boolean;
}

export default function AccountTabs({ accounts, current, path, disabled }: Props) {

  if (accounts.length <= 1) return null;
  // 按**班级**分组（与 `.astro` 版同一套逻辑，见 lib/accountTabs.ts）
  const groups = groupByDept(accounts);
  const btn = (a: Account) =>
          disabled ? (
            <button
              key={a.id}
              className={`btn btn-sm ${a.id === current ? 'btn-primary' : ''}`}
              disabled
              title="任务运行中，结束后可切换"
            >
              {a.label}
            </button>
          ) : (
            <a
              key={a.id}
              className={`btn btn-sm ${a.id === current ? 'btn-primary' : ''}`}
              href={accountHref(path, a.id, new URLSearchParams(typeof window !== 'undefined' ? window.location.search : ''))}
            >
              {a.label}
              {a.username && a.username !== a.label ? (
                <span className="muted" style={{ fontWeight: 400 }}> · {a.username}</span>
              ) : null}
            </a>
          );
  ;

  return (
    <div className="card mt-16">
      {groups.map((g) => (
        <div key={g.dept} className="row" style={{ gap: 8, marginBottom: 6, alignItems: 'center' }}>
          <span className="muted small" style={{ minWidth: 84 }} title={g.dept}>{g.dept}</span>
          {g.items.map(btn)}
        </div>
      ))}
    </div>
  );
}
