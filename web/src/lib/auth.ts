/**
 * 页面层鉴权守卫。
 *
 * ⚠️ 这是必须的：API 的中间件**不会**保护页面本身。
 *    同类平台 实测漏过这一步 —— 未登录仍能渲染 React 岛屿界面。
 */

const BACKEND =
  import.meta.env.NEUMOOC_BACKEND ?? process.env.NEUMOOC_BACKEND ?? 'http://127.0.0.1:8788';

export interface SessionUser {
  username: string;
  role: string;
  role_label?: string;
  is_admin?: boolean;
  is_root?: boolean;
  /** null = 不受限（admin/root）；数字 = 可见账号数 */
  account_scope?: number | null;
}

/**
 * 服务端校验登录态。
 *
 * 必须**手动转发 Cookie** —— SSR 里 fetch 不会自动带浏览器 Cookie。
 */
/**
 * 会话校验结果。
 *
 * 关键在于把 **`anonymous`（真的没登录）** 与 **`unavailable`（后端连不上，
 * 无从判断）** 分开。早先两者都返回 `null`，中间件于是把「后端挂了」也当成
 * 「未登录」把用户弹去登录页 —— 他登不进去，还会以为自己被登出了。
 */
export type SessionResult =
  | { status: 'ok'; user: SessionUser }
  | { status: 'anonymous' }
  | { status: 'unavailable'; error: string };

export async function getSession(request: Request): Promise<SessionResult> {
  const cookie = request.headers.get('cookie');
  if (!cookie) return { status: 'anonymous' };
  try {
    const r = await fetch(`${BACKEND}/api/auth/me`, {
      headers: { cookie, accept: 'application/json' },
    });
    // 只有明确表示「没登录/会话失效」才该跳登录页
    if (r.status === 401 || r.status === 403) return { status: 'anonymous' };
    if (!r.ok) return { status: 'unavailable', error: `后端返回 ${r.status}` };
    return { status: 'ok', user: (await r.json()) as SessionUser };
  } catch (e) {
    return { status: 'unavailable', error: `无法连接后端（${(e as Error).message}）` };
  }
}

/** 需要登录的页面调用；未登录返回 null，由页面决定跳转。 */
export async function requireSession(
  request: Request,
  redirect: (to: string) => Response,
): Promise<SessionUser | Response> {
  const s = await getSession(request);
  if (s.status !== 'ok') {
    const url = new URL(request.url);
    const to = url.pathname === '/' ? '/login' : `/login?redirect=${encodeURIComponent(url.pathname)}`;
    return redirect(to);
  }
  return s.user;
}
