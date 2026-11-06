/**
 * 导航结构。**唯一来源。**
 *
 * ## 为什么抽出来
 *
 * 原先它写在 `layouts/Base.astro` 的 frontmatter 里，是个**局部变量** ——
 * 任何别的文件都无法 import。于是「平台有哪些页面」这件事
 * 只存在于一个 AST 节点的渲染路径上：加页面要记得改它，
 * Ctrl+K 面板、页脚、面包屑想用同一份清单就得再抄一遍。
 *
 * 与 姊妹项目 的 `lib/nav.ts` 对齐（那份的注释写得更直白：
 * 「这是**唯一的一份**……各页各写一份必然漂移」）。
 *
 * ## tier 的作用
 *
 * 按**使用频率**分层，不按功能模块平铺：
 *
 * ```
 * 概览 │ 刷课 考勤 作业 │ 课程 账号 │ 运行日志 调度 │ 用户 设置
 *      └─── 日常 ───┘   └─ 查阅 ─┘   └── 运维 ──┘   └运维┘
 * ```
 *
 * 目前只用于文档与排序意图；`Navbar` 仍是扁平渲染。
 * 加下拉分组时不必再改这里。
 */
export interface NavItem {
  href: string;
  label: string;
  /** 使用频率分层（仅用于分组展示与可读性，不影响渲染） */
  tier: 'daily' | 'lookup' | 'ops';
  /** 后端接口是否已就绪；false = 占位页 */
  ready: boolean;
  /** 一句话说明，用于页头与将来的 Ctrl+K 面板 */
  desc: string;
  /** 仅 root 可见 */
  rootOnly?: boolean;
}

export const NAV_ITEMS: NavItem[] = [
  { href: '/', label: '概览', tier: 'daily', ready: true, desc: '需要注意的事与账号状态' },
  { href: '/study', label: '刷课', tier: 'daily', ready: true, desc: '选目标 → 执行 → 实时日志' },
  { href: '/attendance', label: '考勤', tier: 'daily', ready: true, desc: '签到、补签与自动签到开关' },
  { href: '/tasks', label: '作业', tier: 'daily', ready: true, desc: '待处理作业与测验' },
  { href: '/exams', label: '考试', tier: 'lookup', ready: true, desc: '已交卷考试 · 题目答案与得分' },
  { href: '/courses', label: '课程', tier: 'lookup', ready: true, desc: '课程 × 资源 × 进度' },
  { href: '/accounts', label: '账号', tier: 'lookup', ready: true, desc: '接入、重登、凭据状态' },
  { href: '/logs', label: '运行日志', tier: 'ops', ready: true, desc: '执行历史与实时日志' },
  { href: '/schedule', label: '调度', tier: 'ops', ready: true, desc: '每日检查与通知通道' },
  { href: '/users', label: '用户', tier: 'ops', ready: true, desc: '平台用户与角色', rootOnly: true },
  { href: '/settings', label: '设置', tier: 'ops', ready: true, desc: 'AI 与运行时配置', rootOnly: true },
];

/** 按角色过滤后的导航项。 */
export function navFor(isRoot: boolean): NavItem[] {
  return NAV_ITEMS.filter((l) => !l.rootOnly || isRoot);
}

/** 传给 `Base` / `Navbar` / `SearchPanel` 的形态（只要 href/label）。 */
export function navLinks(isRoot: boolean): { href: string; label: string }[] {
  return navFor(isRoot).map(({ href, label }) => ({ href, label }));
}

export const SITE_NAME = '东软学习自动化';
