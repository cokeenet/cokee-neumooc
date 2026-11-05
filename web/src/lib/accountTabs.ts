/**
 * 账号切换链接的构造。**两份 `AccountTabs` 共用。**
 *
 * 为什么有两份：SSR 页面要求零客户端 JS（用 `.astro`），
 * 而刷课面板需要在**任务运行中禁用切换**（切页会丢实时日志）——
 * 那要求它是 React 组件、能读 `running` 状态。
 *
 * **不能合并，但可以共享这段逻辑** —— 原先两边各写一份 `hrefFor`，
 * 而「切账号要保留其它查询参数」这个修复只做在了其中一边过。
 */
export function accountHref(
  path: string,
  id: number,
  params: URLSearchParams,
): string {
  // ⚠️ 必须**基于现有参数**改 account，而不是死拼 `?account=` ——
  // 后者会让 `/study?account=1&term=X&course=Y` 一切换账号就丢掉 term/course，
  // 用户以为「这门课没了」。
  const p = new URLSearchParams(params);
  p.set('account', String(id));
  return `${path}?${p.toString()}`;
}

/**
 * 按**班级**分组账号。没有班级的归到「未分组」，排在最后。
 *
 * 为什么按班级：6 个账号里 4 个同班，平铺一行认不出谁是谁；
 * 而代课/批量操作基本都是**按班**发生的。
 */
export function groupByDept<T extends { dept?: string }>(
  accounts: T[],
): { dept: string; items: T[] }[] {
  const m = new Map<string, T[]>();
  for (const a of accounts) {
    const k = (a.dept || '').trim() || '未分组';
    const list = m.get(k) ?? [];
    list.push(a);
    m.set(k, list);
  }
  return [...m.entries()]
    .map(([dept, items]) => ({ dept, items }))
    // 「未分组」永远排最后，其余按班级名升序
    .sort((a, b) => {
      if (a.dept === '未分组') return 1;
      if (b.dept === '未分组') return -1;
      return a.dept.localeCompare(b.dept);
    });
}
