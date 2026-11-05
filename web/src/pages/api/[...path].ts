/**
 * 后端代理（方案 1）。
 *
 * 浏览器始终只访问前端同源地址 `/api/*`，由此处转发到后端，
 * 因此：不需要给后端开 CORS，也不把后端端口暴露给浏览器。
 *
 * 注意 SSE（`/api/logs/stream`）：必须**流式透传**，不能等 body 读完再返回，
 * 否则实时日志会退化成"任务结束后一次性吐出"。
 */
import type { APIRoute } from 'astro';

const BACKEND = import.meta.env.NEUMOOC_BACKEND ?? process.env.NEUMOOC_BACKEND ?? 'http://127.0.0.1:8788';

// 这些头由 fetch 自行管理，转发会导致协议错误
const STRIP_REQ_HEADERS = new Set(['host', 'connection', 'content-length', 'accept-encoding']);
// 这些头若原样回传，会与实际 body 不符（尤其是压缩与长度）
const STRIP_RES_HEADERS = new Set(['content-encoding', 'content-length', 'transfer-encoding']);

async function proxy({ request, params }: { request: Request; params: Record<string, string | undefined> }) {
  const path = params.path ?? '';
  const url = new URL(request.url);
  const target = `${BACKEND}/api/${path}${url.search}`;

  const headers = new Headers();
  request.headers.forEach((v, k) => {
    if (!STRIP_REQ_HEADERS.has(k.toLowerCase())) headers.set(k, v);
  });

  const hasBody = !['GET', 'HEAD'].includes(request.method);

  let upstream: Response;
  try {
    upstream = await fetch(target, {
      method: request.method,
      headers,
      body: hasBody ? await request.arrayBuffer() : undefined,
      // SSE 长连接不能被中途掐断
      signal: request.signal,
    });
  } catch (e) {
    return new Response(
      JSON.stringify({ ok: false, error: `无法连接后端 ${BACKEND}：${(e as Error).message}` }),
      { status: 502, headers: { 'content-type': 'application/json' } },
    );
  }

  const resHeaders = new Headers();
  upstream.headers.forEach((v, k) => {
    if (!STRIP_RES_HEADERS.has(k.toLowerCase())) resHeaders.set(k, v);
  });
  // Set-Cookie 可能有多条，forEach 会把它们合并成一条导致失效，需单独追加
  const setCookies = upstream.headers.getSetCookie?.() ?? [];
  if (setCookies.length > 0) {
    resHeaders.delete('set-cookie');
    for (const c of setCookies) resHeaders.append('set-cookie', c);
  }
  // 代理链路上禁止任何缓冲，SSE 才能即时到达
  resHeaders.set('cache-control', 'no-cache, no-transform');
  resHeaders.set('x-accel-buffering', 'no');

  // 直接透传 body 流：SSE 可以边产生边送达浏览器
  return new Response(upstream.body, {
    status: upstream.status,
    headers: resHeaders,
  });
}

export const ALL: APIRoute = proxy;
export const GET: APIRoute = proxy;
export const POST: APIRoute = proxy;
export const PUT: APIRoute = proxy;
export const DELETE: APIRoute = proxy;
export const PATCH: APIRoute = proxy;
