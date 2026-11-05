/**
 * 统一的数据加载 / 动作执行。
 *
 * ## 为什么需要它
 *
 * 重构前，9 个组件各自实现同一套「加载 → 错误 → 刷新」：
 * **84 个 `useState` 散在各处**，同一个接口在不同组件里的失败表现都不一样。
 *
 * 抽出这两个 hook 之后，每个面板只需要说清楚**「取什么」**和**「做什么」**，
 * 不用再重复「怎么处理加载中 / 怎么显示错误 / 什么时候刷新」。
 *
 * ## 两个 hook 的分工
 *
 * | hook | 用途 | 典型场景 |
 * | --- | --- | --- |
 * | [`useResource`] | **读**：进页面就取、依赖变了重取 | 列表、详情 |
 * | [`useAction`] | **写**：用户点了才执行、执行中禁用按钮 | 保存、提交、触发任务 |
 *
 * 分开是因为它们的生命周期完全不同：读是「自动 + 幂等」，
 * 写是「手动 + 有副作用」。混在一起写出来的组件会又长又难改。
 */

import { useCallback, useEffect, useRef, useState } from 'react';

export interface Resource<T> {
  data: T | null;
  loading: boolean;
  error: string | null;
  /** 重新加载。**不会**把 loading 置回 true 造成闪烁，除非是首次加载。 */
  reload: () => Promise<void>;
  /** 本地直接改（乐观更新用）。 */
  setData: (v: T | null) => void;
}

/**
 * 读一个资源。
 *
 * `load` **必须是 useCallback 包好的**，否则每次渲染都会重新触发加载。
 * 依赖数组就是 `load` 的依赖 —— 不需要另外传。
 *
 * ```tsx
 * const tasks = useCallback(() => api.tasks(accountId), [accountId]);
 * const { data, loading, error, reload } = useResource(tasks);
 * ```
 */
export function useResource<T>(load: () => Promise<T>): Resource<T> {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // 用 ref 记住「是不是第一次」，避免每次 reload 都把整页闪成加载中
  const first = useRef(true);
  // 防止「后发的请求先回来」把新数据覆盖成旧的
  const seq = useRef(0);

  const reload = useCallback(async () => {
    const mine = ++seq.current;
    if (first.current) setLoading(true);
    try {
      const v = await load();
      // 只有最后一次发出的请求才允许写结果
      if (mine === seq.current) {
        setData(v);
        setError(null);
      }
    } catch (e) {
      if (mine === seq.current) setError((e as Error).message || '加载失败');
    } finally {
      if (mine === seq.current) {
        setLoading(false);
        first.current = false;
      }
    }
  }, [load]);

  useEffect(() => { void reload(); }, [reload]);

  return { data, loading, error, reload, setData };
}

export interface Msg {
  kind: 'ok' | 'err' | 'warn' | 'info';
  text: string;
}

export interface Action<A extends unknown[], R> {
  run: (...args: A) => Promise<R | undefined>;
  busy: boolean;
  msg: Msg | null;
  setMsg: (m: Msg | null) => void;
}

/**
 * 执行一个动作（写操作）。
 *
 * 自动处理：执行中 `busy`（用来禁用按钮）、失败时的错误信息、
 * **成功后自动刷新**（`onDone`）。
 *
 * ⚠️ `run` **不抛异常**，失败返回 `undefined` 并把错误放进 `msg`。
 * 组件里到处写 `try/catch` 是重复的，而且很容易漏掉一处。
 */
export function useAction<A extends unknown[], R>(
  fn: (...args: A) => Promise<R>,
  opts: { okText?: string; onDone?: () => void | Promise<void> } = {},
): Action<A, R> {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<Msg | null>(null);
  // 防止双击发两次
  const inFlight = useRef(false);

  const run = useCallback(
    async (...args: A): Promise<R | undefined> => {
      if (inFlight.current) return undefined;
      inFlight.current = true;
      setBusy(true);
      setMsg(null);
      try {
        const r = await fn(...args);
        if (opts.okText) setMsg({ kind: 'ok', text: opts.okText });
        await opts.onDone?.();
        return r;
      } catch (e) {
        setMsg({ kind: 'err', text: (e as Error).message || '操作失败' });
        return undefined;
      } finally {
        inFlight.current = false;
        setBusy(false);
      }
    },
    [fn, opts.okText, opts.onDone],
  );

  return { run, busy, msg, setMsg };
}
