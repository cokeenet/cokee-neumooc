/**
 * 统一的「发起变更请求」助手。
 *
 * 规范来自 同类平台（见 /opt/同类平台/UI-SPEC.md）：
 *   1. **永不抛出** —— 网络异常也转成 `{kind:'err'}`，让调用方只关心结果
 *   2. **优先用服务端 message** —— 后端文案通常比前端准确
 *   3. **成功后刷新** —— 避免乐观更新与服务端真值不一致
 *   4. **`finally` 归零 busy** —— 异常路径也不能卡住按钮
 *
 * 原来的实现散落在各组件里各写一遍，这里收敛成一处。
 */
import { useCallback, useState } from 'react';

export type MsgKind = 'ok' | 'err' | 'warn';
export interface Msg {
  kind: MsgKind;
  text: string;
}

export interface CallResult<T = unknown> {
  ok: boolean;
  data?: T;
  error?: string;
  /** 幂等命中不是失败，UI 应显示为中性提示 */
  idempotent?: boolean;
  /** 同步调用的日志（不落库，随响应回传） */
  logs?: { level: string; message: string }[];
}

export function isIdempotentHit(text: string | undefined): boolean {
  if (!text) return false;
  return /幂等|已执行过|当日已|重复提交/.test(text);
}

export function useCall(onDone?: () => void | Promise<void>) {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<Msg | null>(null);

  const call = useCallback(
    async <T = unknown>(url: string, init: RequestInit, okText: string): Promise<CallResult<T>> => {
      setBusy(true);
      setMsg(null);
      try {
        const r = await fetch(url, {
          ...init,
          headers: { 'content-type': 'application/json', ...(init.headers ?? {}) },
        });
        const d = (await r.json().catch(() => ({}))) as CallResult<T> & {
          detail?: string;
          logs?: { level: string; message: string }[];
        };
        // ⚠️ **不能只看 HTTP 状态**：后端失败时也返回 200 + `{ok:false, error}`。
        // 只看 `r.ok` 会把「签到失败」显示成「签到提交成功」，
        // 而且**两个后端（Python / Rust）都是这个形状** —— 所以这是前端既有 bug。
        // 实测：签到对话框点了之后面板直接关掉，用户以为签上了。
        if (r.ok && d.ok !== false) {
          const text = (d.data as { message?: string } | undefined)?.message ?? okText;
          setMsg({ kind: 'ok', text });
          await onDone?.();
          // 连同 logs 一起透传：同步调用的日志不落库，只随响应回来，
          // 只返回 {ok,data} 会把它们丢掉（补签演练就看不到明细）。
          return { ok: true, data: d.data, logs: d.logs ?? [] };
        }
        // body 里的 error 优先 —— 后端失败也是 200，状态码说不出原因
        const err = d.error ?? d.detail ?? `操作失败（HTTP ${r.status}）`;
        // 幂等命中是**预期行为**，不是故障 —— 显成报错会让人以为系统坏了
        setMsg(isIdempotentHit(err) ? { kind: 'warn', text: err } : { kind: 'err', text: err });
        return { ok: false, error: err, idempotent: isIdempotentHit(err), logs: d.logs ?? [] };
      } catch (e) {
        const text = (e as Error).message;
        setMsg({ kind: 'err', text });
        return { ok: false, error: text, logs: [] };
      } finally {
        setBusy(false);
      }
    },
    [onDone],
  );

  return { call, busy, msg, setMsg, setBusy };
}
