/**
 * 页面层鉴权 + CSRF 防护。
 *
 * 与本仓库的姊妹项目 同类平台 共用同一套实现（那边是 `web/src/middleware.ts`），
 * 换过来之后两边行为一致，且**都不再依赖 Astro 内置的 `security.checkOrigin`**。
 *
 * ## 为什么不能直接用 Astro 内置的同源校验
 *
 * Astro 比较 `Origin` 头与 `new URL(request.url).origin`，而后者由 Node 适配器的
 * `createRequestFromNodeRequest` 构造 —— 它**只看 `req.socket.encrypted` 和 `Host`**，
 * 完全不读 `X-Forwarded-Proto`。而我们的部署是 nginx 终止 TLS 后转发明文 http：
 *
 * ```
 * 浏览器 Origin        https://app.example.com
 * Astro 算出的 origin   http://app.example.com    ← 协议丢了
 *                      → 不相等 → 403
 * ```
 *
 * 所以 `astro.config` 里设了 `security.checkOrigin: false`，
 * 然后在**这里**补一份等价（且更严）的校验 —— 而不是干脆不要防护。
 *
 * ## 为什么偏偏是「退出登录」会报错
 *
 * Astro 原逻辑分两种（`astro/dist/core/app/origin-check.js`）：
 *
 * | 请求 | 判定 |
 * | --- | --- |
 * | 带 `application/json` 之类**非表单** content-type | json 不算表单 → **放行** |
 * | **没有 content-type** | 直接看同源 → 公网下**必然 403** |
 *
 * 登出是 `fetch('/api/auth/logout', {method:'POST'})`，没 body 就没 content-type，
 * 正好落在第二条。本地 http 因 origin 恰好一致而被掩盖，只有公网才暴露 ——
 * 用户看到「退出失败，未能连接服务器」。
 *
 * ## 这里的实现
 *
 * 用**可信来源**重建外部 origin：
 *   1. `X-Forwarded-Proto`（nginx 已设，取第一个值）
 *   2. `X-Forwarded-Host` / `Host`
 * 然后要求 `Origin` 与之**完全相等**。浏览器对所有跨站非简单请求都会带 `Origin`，
 * 所以这比 Astro 原版更严：**不再放行 content-type 不匹配的跨站请求**。
 *
 * ## 为什么仅有 Host 白名单是不够的（实测过）
 *
 * 只挡 Host 挡不住 CSRF：攻击者用**合法 Host** 发请求、伪造的只是 `Origin`。
 * 实测 `POST /api/auth/logout` 带 `Host: app.example.com` + `Origin: http://evil.com`
 * 返回 **200** —— 所以 Host 白名单与 Origin 校验缺一不可。
 *
 * 没有 `Origin` 头的请求（老浏览器、curl、服务端调用）：只有携带会话 Cookie 的
 * **写请求**才需要 `Origin`；缺失时按 `sameSite=Lax` 的语义处理 —— 跨站表单提交
 * 浏览器一定会带 Origin，不带 Origin 而带 Cookie 的跨站写请求在现代浏览器里
 * 构造不出来，因此这里放行（否则 curl 调试全废），但会要求 Host 来自可信集合。
 *
 * ## 页面守卫（本项目特有）
 *
 * 姊妹项目是单用户，本项目有多用户，所以还要判定登录态并做角色分流。
 * 守卫必须在中间件里做 —— layout frontmatter 里 `Astro.redirect()` 会因
 * 流式渲染已开始而报 `ResponseSentError`，表现为页面 500 而 curl 看到 200，
 * **守卫静默失效**。
 *
 * API 的 `_auth_gate` 只管 `/api/*`；未登录仍能渲染页面壳子（同类平台 漏过这步）。
 */

import { defineMiddleware } from 'astro:middleware';
import { getSession } from './lib/auth';
import { requestContext } from './lib/backend';

const SAFE_METHODS = new Set(['GET', 'HEAD', 'OPTIONS']);

/**
 * 允许的 Host（部署域名 + 内网直连 + 本机调试）。
 *
 * 必须覆盖所有合法入口，否则会被自己挡掉。漏加域名的症状很迷惑：
 * **无 body 的 POST（如登出）失败，而带 JSON 的 POST 正常**。
 */
/**
 * **额外的**合法 Host 从环境变量读：`NEUMOOC_ALLOWED_HOSTS`（逗号分隔）。
 *
 * 真实部署域名与内网 IP 只出现在 systemd 的 `Environment=` 里，不进仓库。
 * 本机调试的 127.0.0.1 / localhost 保留为默认值 —— 它们不是敏感信息。
 */
const EXTRA_HOSTS = (process.env.NEUMOOC_ALLOWED_HOSTS ?? '')
  .split(',')
  .map((h) => h.trim())
  .filter(Boolean);
const ALLOWED_HOSTS = new Set([
  ...EXTRA_HOSTS,
  '127.0.0.1:3086',
  '127.0.0.1:4322',
  'localhost:3086',
  'localhost:4322',
]);

/** 无需登录即可访问的路径 */
const PUBLIC_PATHS = new Set(['/login', '/favicon.svg']);

function firstForwarded(v: string | null): string | null {
  if (!v) return null;
  const first = v.split(',')[0].trim();
  return first || null;
}

/** 反代会带 X-Forwarded-Host；没有（直连）时退回 Host */
function effectiveHost(request: Request): string | null {
  return (
    firstForwarded(request.headers.get('x-forwarded-host')) ??
    firstForwarded(request.headers.get('host'))
  );
}

function deny(message: string): Response {
  return new Response(JSON.stringify({ ok: false, error: message }), {
    status: 403,
    headers: { 'content-type': 'application/json' },
  });
}

export const onRequest = defineMiddleware(async (context, next) => {
  const req = context.request;
  const path = context.url.pathname;
  const cookie = req.headers.get('cookie') ?? '';

  // ── ① CSRF：只校验写请求 ──
  if (!SAFE_METHODS.has(req.method.toUpperCase())) {
    const host = effectiveHost(req);
    const proto = firstForwarded(req.headers.get('x-forwarded-proto')) ?? 'http';
    const origin = req.headers.get('origin');

    // Host 白名单：挡住 Host 头投毒。
    // 注意它**挡不住 CSRF**（攻击者可用合法 Host + 伪造 Origin），
    // 真正起作用的是下面那条。
    if (host && !ALLOWED_HOSTS.has(host) && !ALLOWED_HOSTS.has(host.split(':')[0])) {
      return deny('Host 不被允许');
    }

    // 带了 Origin 就必须与本服务的外部 origin 完全一致
    if (origin) {
      const expected = `${proto}://${host}`;
      if (origin !== expected) {
        return deny(`跨站请求被拒绝（Origin ${origin} ≠ ${expected}）`);
      }
    }
  }

  // 用 ALS 把 Cookie 带进本次请求的所有服务端 fetch。
  // 必须在最外层包住 next()，页面里的 serverFetch 才读得到。
  const withCtx = (fn: () => Promise<Response> | Response) => requestContext.run({ cookie }, fn);

  // 静态资源与公开路径直接放行
  if (PUBLIC_PATHS.has(path) || path.startsWith('/assets/') || path.startsWith('/fonts/')) {
    return withCtx(next);
  }
  // API 走后端自己的鉴权（由 Astro 代理转发）
  if (path.startsWith('/api/')) {
    return withCtx(next);
  }

  const session = await getSession(req);

  // 后端不可用 ≠ 未登录！此时把用户弹去登录页是错的：他根本登不进去
  // （登录也要打后端），而且会以为自己被登出了。放行让页面渲染，
  // 由页面的取数失败态如实说明情况。
  if (session.status === 'unavailable') {
    context.locals.user = null;
    context.locals.backendError = session.error;
    return withCtx(next);
  }

  if (session.status === 'anonymous') {
    const target =
      path === '/' ? '/login' : `/login?redirect=${encodeURIComponent(path + context.url.search)}`;
    return context.redirect(target, 302);
  }

  // 把用户挂到 locals，页面与 layout 可直接读取
  const user = session.user;
  context.locals.user = user;

  // 用户管理仅 root 可进（admin 也不能管平台用户）
  if (path.startsWith('/users') && !user.is_root) {
    return context.redirect('/', 302);
  }

  return withCtx(next);
});
