/**
 * 提示条样式映射。**唯一来源。**
 *
 * 原先这段在 5 个组件里各写一遍（`AccountsPanel` / `AttendancePanel` /
 * `MakeupPanel` / `SchedulerPanel` / `UsersPanel`）—— 加一档 `kind`
 * 就要改五处，漏一处那一档就没有样式（而且是静默的：类名拼出来是空的）。
 */
export type MsgKind = 'ok' | 'err' | 'warn' | 'info';

export const MSG_CLASS: Record<MsgKind, string> = {
  ok: 'msg-ok',
  err: 'msg-err',
  warn: 'msg-warn',
  info: 'msg-info',
};

/** 提示条组件用不到——但日志行的等级映射也需要同一份约定。 */
export const LEVEL_CLASS: Record<string, string> = {
  ok: 'is-ok',
  error: 'is-err',
  warn: 'is-warn',
  info: 'is-info',
};
