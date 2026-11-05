/**
 * 展示层文案映射。
 *
 * 后端返回的是内部键名（`job_key`、`state`），直接摆给用户看就是术语泄漏 ——
 * 概览页一度显示「任务：plan:study」这种只有开发者看得懂的东西。
 *
 * 统一在这里翻译。**不要**在各个页面里另写一份 Record，那样必然漏改
 * （`badge-warn` 被 6 处引用却从未定义，就是这么来的）。
 */

export type Variant = 'ok' | 'warn' | 'fail' | 'run' | 'muted';

/** worker 命令名 → 人话 */
export const JOB_TEXT: Record<string, string> = {
  study: '刷课',
  'plan:study': '定时刷课',
  // 每日检查（发现新资源/新作业）。漏了它，界面上会原样显示 job_key `check`
  check: '每日检查',
  'plan:check': '每日检查',
  attendance: '同步考勤',
  'plan:attendance': '定时同步考勤',
  sign: '提交签到',
  auto_checkin: '自动签到',
  teacher_makeup: '教师补签',
  makeup: '补签',
  login: '登录',
  scan: '扫描课程',
  refresh: '刷新令牌',
  sms_login: '短信登录',
};
export const jobLabel = (key: string): string => JOB_TEXT[key] ?? key;

/** 执行状态 */
export const EXEC_STATE: Record<string, { text: string; variant: Variant }> = {
  done: { text: '成功', variant: 'ok' },
  failed: { text: '失败', variant: 'fail' },
  running: { text: '进行中', variant: 'run' },
  pending: { text: '等待', variant: 'muted' },
  skipped: { text: '跳过', variant: 'muted' },
  // 取消不是失败，是一种预期结果；单独一档，别混进「失败」里吓人
  cancelled: { text: '已取消', variant: 'warn' },
};
export const execState = (s: string) =>
  EXEC_STATE[s] ?? { text: s, variant: 'muted' as Variant };

/**
 * 账号状态。
 *
 * ⚠️ 这里**刻意不暴露令牌是否过期**：过期是常态，平台会自动重新登录，
 * 显示「已过期」只会让人以为系统坏了。只有真的需要人工介入（need_login）
 * 才算异常。
 */
export const ACCOUNT_STATE: Record<string, { text: string; variant: Variant }> = {
  active: { text: '正常', variant: 'ok' },
  need_login: { text: '需重新登录', variant: 'fail' },
};
export const accountState = (s: string) =>
  ACCOUNT_STATE[s] ?? { text: '未验证', variant: 'muted' as Variant };

/** 北京时间业务日（YYYY-MM-DD），与后端 db.biz_date_now() 对齐 */
export function bizDateNow(): string {
  return new Date(Date.now() + 8 * 3600 * 1000).toISOString().slice(0, 10);
}

/**
 * 时间格式化 —— **一律显式指定 `Asia/Shanghai`**。
 *
 * SSR 由 Node 渲染，而容器时区是 UTC（`TZ` 未设置，systemd 也没配）。
 * 不写 timeZone 就会按 UTC 格式化，同一时刻在概览页和运行日志页**差 8 小时**。
 * 服务端渲染和浏览器渲染还可能得到不同结果，所以这里不依赖运行环境。
 */
export function fmtTs(ms: number | null | undefined): string {
  if (!ms) return '—';
  return new Date(ms).toLocaleString('zh-CN', {
    hour12: false,
    timeZone: 'Asia/Shanghai',
  });
}

/**
 * 「MM-DD HH:MM」—— 给**截止时间**用。
 *
 * 不能直接用 `fmtTime`：它只给 `时:分:秒`，跨天的截止日期会显示成
 * `23:59:59`，看不出是今天还是三天后。也不能用 `fmtTs`：那个带年份和秒，
 * 在表格里太长。
 */
export function fmtDeadline(ms: number | null | undefined): string {
  if (!ms) return '—';
  return new Date(ms).toLocaleString('zh-CN', {
    timeZone: 'Asia/Shanghai', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', hour12: false,
  });
}

/** 只要 时:分:秒 */
export function fmtTime(ms: number | null | undefined): string {
  if (!ms) return '—';
  return new Date(ms).toLocaleTimeString('zh-CN', {
    hour12: false,
    timeZone: 'Asia/Shanghai',
  });
}

/**
 * 资源类型 → 人话。**唯一来源。**
 *
 * 原先在 `StudyPanel.tsx` 与 `courses.astro` 各写一份 `TYPE_TEXT` ——
 * 加一个类型就要改两处，漏一处那一类资源会显示成「其他」。
 */
export const RES_TYPE_TEXT: Record<number, string> = {
  1: '视频',
  3: '文档',
  4: '图片',
};
