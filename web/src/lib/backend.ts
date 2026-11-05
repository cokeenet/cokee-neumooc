/**
 * 服务端访问后端。
 *
 * ⚠️ **SSR 页面在服务端 fetch 不会自动带浏览器 Cookie**，必须手动转发。
 *    （同类平台 踩过这个坑；本平台加鉴权后也踩了一次 —— 表现为
 *     页面渲染出来但数据全是空的，因为后端一律返回 401。）
 *
 * 转发方式用 `AsyncLocalStorage` 而非逐处传参：
 *   中间件在 `als.run({ cookie })` 里调用 `next()`，`serverFetch` 直接读。
 *   这样新增页面**不可能忘记**转发 —— 逐处传参迟早会漏。
 *   ALS 按异步上下文隔离，并发请求之间不会串 cookie。
 */

import { AsyncLocalStorage } from 'node:async_hooks';

const BACKEND =
  import.meta.env.NEUMOOC_BACKEND ?? process.env.NEUMOOC_BACKEND ?? 'http://127.0.0.1:8788';

export interface ApiResult<T = unknown> {
  ok: boolean;
  data?: T;
  error?: string;
}

/** 当前请求的上下文（中间件写入）。 */
export const requestContext = new AsyncLocalStorage<{ cookie: string }>();

/**
 * 取数结果。**区分「没有数据」和「取不到数据」**。
 *
 * 这个区分是必要的：早先 `serverFetch` 失败时统一返回 `null`，各页面再 `?? []`，
 * 于是后端一挂，概览页就对有账号的用户显示「还没有接入账号」并引导他去添加账号 ——
 * 界面在**陈述一件假事**，比直接报错更糟。
 */
export type FetchOutcome<T> =
  | { ok: true; data: T }
  | { ok: false; error: string };

/** 服务端取数，失败时**说明原因**。新代码请用这个。 */
export async function serverFetchResult<T = unknown>(
  path: string,
  init: RequestInit = {},
): Promise<FetchOutcome<T>> {
  const cookie = requestContext.getStore()?.cookie ?? '';
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    ...((init.headers as Record<string, string>) ?? {}),
  };
  // 转发登录态：否则后端鉴权中间件一律 401，页面会渲染成空数据
  if (cookie) headers.cookie = cookie;

  try {
    const r = await fetch(`${BACKEND}${path}`, { ...init, headers });
    if (!r.ok) {
      // 401 单独说，因为它的处置方式和别的失败完全不同
      if (r.status === 401) return { ok: false, error: '登录状态已失效，请重新登录' };
      return { ok: false, error: `后端返回 ${r.status}` };
    }
    return { ok: true, data: (await r.json()) as T };
  } catch (e) {
    return { ok: false, error: `无法连接后端服务（${(e as Error).message}）` };
  }
}

/**
 * 兼容旧写法的取数：失败返回 `null`。
 *
 * ⚠️ 只在**确实不需要区分**失败与空数据时使用；否则界面会把故障说成「暂无数据」。
 */
export async function serverFetch<T = unknown>(
  path: string,
  init: RequestInit = {},
): Promise<T | null> {
  const r = await serverFetchResult<T>(path, init);
  return r.ok ? r.data : null;
}

export { BACKEND };
