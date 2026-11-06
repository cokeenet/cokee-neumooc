/** 浏览器侧 API 客户端。所有请求走同源 /api/*（由 Astro 代理到后端）。 */

/** 执行日志一行（后端 `await_execution` 会把 `data.logs` 提到顶层） */
export interface LogLine { level: string; message: string }

/**
 * AI 配置。
 *
 * `api_key` **永远不回来** —— 后端只回 `has_key` 和 `key_hint`（前 4 后 4 位）。
 * `*_from_env` 为 true 时说明该项被环境变量压住，界面改了不生效。
 */
export interface LlmConfig {
  has_key: boolean;
  key_hint: string;
  model: string;
  base_url: string;
  key_from_env: boolean;
  model_from_env: boolean;
  base_url_from_env: boolean;
  /** 思考强度：off / minimal / low / medium / high / max */
  effort: string;
  /** 是否请求流式（内部聚合，行为一致，只是能更早拿数据） */
  stream: boolean;
  effort_from_env: boolean;
  stream_from_env: boolean;
  /** 后端给的合法取值，避免两边写死不一致 */
  efforts: string[];
}

export interface ExecResult<T = unknown> {
  ok: boolean;
  data?: T;
  error?: string;
  /** `await_execution` 提升到顶层的日志 */
  logs?: LogLine[];
  execution_id?: number;
}

async function req<T>(path: string, init: RequestInit = {}): Promise<T> {
  const r = await fetch(`/api${path}`, {
    ...init,
    headers: { 'content-type': 'application/json', ...(init.headers ?? {}) },
  });
  const text = await r.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = { error: text.slice(0, 300) };
  }
  // ⚠️ **不能只看 HTTP 状态**：后端失败时也返回 200 + `{ok:false, error}`
  // （FastAPI 的默认行为，Python 版和 Rust 版都是这个形状）。
  // 只看 `r.ok` 会把「签到失败」显示成「签到提交成功」—— 这个 bug 修过一次，
  // 但当时只修在 `useCall` 里；**现在收拢到 `req()` 这唯一一处**，
  // 所有走 api.ts 的调用都自动带上这道判断。
  const b = body as { detail?: string; error?: string; ok?: boolean } | null;
  if (!r.ok || b?.ok === false) {
    const msg = b?.error ?? b?.detail ?? `HTTP ${r.status}`;
    throw new Error(msg);
  }
  return body as T;
}

export const api = {
  health: () => req<{ ok: boolean; accounts: number }>('/health'),
  overview: () => req<Record<string, unknown>>('/overview'),

  accounts: () => req<Account[]>('/accounts'),
  addAccount: (body: { username: string; password: string; label?: string; tenant_id?: string }) =>
    req<{ id: number }>('/accounts', { method: 'POST', body: JSON.stringify(body) }),
  patchAccount: (id: number, body: Record<string, unknown>) =>
    req<{ ok: boolean }>(`/accounts/${id}`, { method: 'PATCH', body: JSON.stringify(body) }),
  /** AI 配置（**key 打码**，只回 has_key 与前后 4 位） */
  llmConfig: () => req<LlmConfig>('/llm/config'),
  /** 保存 AI 配置。`api_key` 空 = 不修改；`clear_key` 显式清除 */
  llmSetConfig: (body: {
    api_key?: string; model?: string; base_url?: string;
    effort?: string; stream?: boolean; clear_key?: boolean;
  }) =>
    req<{ ok: boolean }>('/llm/config', { method: 'PUT', body: JSON.stringify(body) }),
  /** 用当前配置真发一次请求 */
  llmTest: () => req<{ ok: boolean; reply?: string; error?: string }>('/llm/test', { method: 'POST' }),
  /** 设备 ID 全局开关状态 */
  deviceConfig: () => req<{ master_on: boolean; env_var: string; note: string }>('/device-config'),
  /** 开关某账号签到是否带 deviceId（打开时若没有 ID 会自动生成） */
  setDeviceId: (id: number, enabled: boolean) =>
    req<{ ok: boolean; enabled: boolean; device_id: string; generated: boolean; master_on: boolean }>(
      `/accounts/${id}/device-id`,
      { method: 'PUT', body: JSON.stringify({ enabled }) },
    ),
  deleteAccount: (id: number) => req<{ ok: boolean }>(`/accounts/${id}`, { method: 'DELETE' }),

  courses: (id: number, termId = '') =>
    req<Course[]>(`/accounts/${id}/courses${termId ? `?term_id=${termId}` : ''}`),
  resources: (
    id: number,
    params: {
      course_pk?: number; only_todo?: boolean; res_type?: number; term_id?: string;
      /** 只看**计入考核**的 —— 考核分只由这部分产生 */
      only_assessed?: boolean;
    } = {},
  ) => {
    const q = new URLSearchParams();
    if (params.course_pk) q.set('course_pk', String(params.course_pk));
    if (params.only_todo) q.set('only_todo', 'true');
    if (params.res_type) q.set('res_type', String(params.res_type));
    if (params.term_id) q.set('term_id', params.term_id);
    if (params.only_assessed) q.set('only_assessed', 'true');
    const s = q.toString();
    return req<Resource[]>(`/accounts/${id}/resources${s ? `?${s}` : ''}`);
  },

  scan: (id: number, termId = '') =>
    req<ExecResult<ScanResult>>(`/accounts/${id}/scan`, {
      method: 'POST', body: JSON.stringify({ term_id: termId }),
    }),

  study: (id: number, body: StudyBody) =>
    req<{ execution_id?: number; detail?: string; dry_run?: boolean; count?: number; resources?: Resource[] }>(
      `/accounts/${id}/study`,
      { method: 'POST', body: JSON.stringify(body) },
    ),

  executions: (accountId?: number) =>
    req<Execution[]>(`/executions${accountId ? `?account_id=${accountId}` : ''}`),
  /**
   * 读一次执行。
   *
   * `detail` 可能是**对象**（新后端会解析）或**字符串**（老记录的原始 TEXT），
   * 调用方要按需 `typeof` 判断 —— 早期版本这里当成对象直接读字段，
   * 结果 `answered` 永远是 undefined。
   */
  execution: (id: number) => req<Execution>(`/executions/${id}`),

  /** 附件下载地址（直接给 `<a href>` 用，后端带上 token 转发） */
  fileUrl: (id: number, eid: string) => `/api/accounts/${id}/files/${encodeURIComponent(eid)}`,
  /**
   * 资源代理地址（题图、附件预览等需要 token 的静态资源）。
   *
   * 早先是 `/tasks/:test_id/questions/:no/image?idx=N` —— 后端每张图都要
   * `enter_task` 一次，两道图并发就会触发两次重新登录互相踢 token（第二张 400）。
   * 现在前端直接把题干里抽到的 src 传过来，后端只负责带 token 转发。
   */
  assetUrl: (id: number, src: string) =>
    `/api/accounts/${id}/asset?src=${encodeURIComponent(src)}`,
  /**
   * **上传文件**（附件 / 文件题作答）。返回平台 eid。
   *
   * ⚠️ 不要手写 `Content-Type` —— 浏览器要给 FormData 自己带 boundary。
   */
  uploadFile: async (id: number, file: File) => {
    const fd = new FormData();
    fd.append('file', file);
    const r = await fetch(`/api/accounts/${id}/files`, { method: 'POST', body: fd });
    const d = await r.json() as {
      ok?: boolean; eid?: string; name?: string; size?: string; md5?: string; detail?: string;
    };
    if (!r.ok || !d.ok) throw new Error(d.detail || '上传失败');
    return d as { eid: string; name: string; size: string; md5: string };
  },
  logs: (id: number) => req<LogRow[]>(`/executions/${id}/logs`),
  cancel: (id: number) => req<{ ok: boolean }>(`/executions/${id}/cancel`, { method: 'POST' }),
  terms: (id: number) =>
    req<{ ok: boolean; data?: { id: string; name: string }[]; error?: string }>(
      `/accounts/${id}/exec`,
      { method: 'POST', body: JSON.stringify({ cmd: 'terms' }) },
    ),

  // ---- 以下原先散在各组件里直接 fetch，现在统一收在这里 ----
  //
  // 收拢的理由：组件直接 fetch 会**各自处理错误、各自拼路径、各自解析 JSON**，
  // 结果是同一个接口在不同组件里的失败表现都不一样。
  // 统一走 `req()` 之后，「HTTP 非 2xx」和「body 里 ok:false」只有一处判断。

  attendance: (id: number, termId = '') =>
    req<AttendanceResp>(`/accounts/${id}/attendance${termId ? `?term_id=${termId}` : ''}`),

  autoSign: (id: number) => req<AutoSignResp>(`/accounts/${id}/auto-sign`),
  setAutoSign: (id: number, enabled: boolean) =>
    req<{ ok: boolean }>(`/accounts/${id}/auto-sign`, {
      method: 'PUT', body: JSON.stringify({ enabled }),
    }),

  tasks: (id: number) => req<TasksResp>(`/accounts/${id}/tasks`),

  // ── 考试（智慧考试子系统，实时拉取，不落库）──
  exams: (id: number) => req<ExamListResp>(`/accounts/${id}/exams`),
  /** 试卷详情要 **examId + candidateId** 两个 id（路径是 .../{examId}/{candidateId}） */
  /** 全部作业/测验（含已交）—— 按课程遍历 test/list 得到，不只是「待完成」 */
  homeworks: (id: number) => req<HomeworksResp>(`/accounts/${id}/homeworks`),
  /** 作业详情：未交=题目预览；已交=我的答案 + 逐题得分 + 平台允许时显示标准答案 */
  homeworkDetail: (id: number, testId: string, candidateId?: string) =>
    req<HomeworkDetail>(
      `/accounts/${id}/homeworks/${encodeURIComponent(testId)}/detail` +
        (candidateId ? `?candidate_id=${encodeURIComponent(candidateId)}` : ''),
    ),
  /**
   * **手动作答**：保存草稿（submit=false）或交卷（submit=true，需 admin）。
   *
   * ⚠️ 后端走 `await_execution`，返回 `{ok, data, logs}` —— **失败时 HTTP 仍是 200**，
   * 必须看 `ok` 字段。早先直接当扁平对象读，把「401 账号未登录」当成了「已保存」。
   */
  answerTask: async (id: number, testId: string, answers: { questionNo: string; answer: string }[], submit = false) => {
    const r = await req<ExecResult<{ saved?: number; submitted?: boolean; history_id?: string }>>(
      `/accounts/${id}/tasks/answer`,
      { method: 'POST', body: JSON.stringify({ test_id: testId, answers, submit }) },
    );
    if (!r.ok) throw new Error(r.error || '作答失败');
    return { ...(r.data ?? {}), logs: r.logs } as { saved?: number; submitted?: boolean; history_id?: string; logs?: LogLine[] };
  },
  /**
   * **AI 批量作答**：后端取题 → 调模型 → 算答案（一次调用答完整份）。
   * 同样要检查 `ok`（见 `answerTask` 的说明）。
   */
  /**
   * **AI 批量作答**。
   *
   * `no_wait: true` 时立刻返回 `execution_id`，前端据此订阅 SSE 看思考过程，
   * 算完再用 `execution(id)` 拉结果。默认等结果一起返回。
   */
  aiAnswerTask: async (id: number, testId: string, opts: {
    submit?: boolean; dry_run?: boolean; no_wait?: boolean;
  } = {}) => {
    const r = await req<ExecResult<{ answered?: number; total?: number; submitted?: boolean; unknown?: string[] }>>(
      `/accounts/${id}/tasks/ai-answer`,
      { method: 'POST', body: JSON.stringify({ test_id: testId, ...opts }) },
    );
    if (!r.ok) throw new Error(r.error || 'AI 作答失败');
    // ⚠️ `no_wait: true` 时后端返回的是 `{ok, execution_id, state}`，
    // **没有 `data`** —— 早先只展开 `r.data`，`execution_id` 就丢了，
    // 前端拿不到 id 也就订阅不了实时流（面板永远空白）。
    return {
      ...(r.data ?? {}),
      logs: r.logs,
      execution_id: r.execution_id,
      state: (r as unknown as { state?: string }).state,
    } as {
      answered?: number; total?: number; submitted?: boolean; unknown?: string[];
      logs?: LogLine[];
      execution_id?: number;
      state?: string;
    };
  },
  /** 作业试卷（题目/我的答案/标准答案/逐题得分），结构对齐考试试卷 */
  homeworkPaper: (id: number, testId: string) =>
    req<HomeworkPaper>(`/accounts/${id}/homeworks/${encodeURIComponent(testId)}/paper`),
  examPaper: (id: number, examId: string, candidateId: string) =>
    req<ExamPaper>(`/accounts/${id}/exams/${encodeURIComponent(examId)}/paper?candidate_id=${encodeURIComponent(candidateId)}`),
  taskDetail: (id: number, testId: string) =>
    req<{ ok: boolean; data?: TaskDetail; error?: string }>(`/accounts/${id}/task-detail`, {
      method: 'POST', body: JSON.stringify({ test_id: testId }),
    }),

  schedule: () => req<ScheduleRow[]>('/schedule'),
  setSchedule: (id: number, body: { enabled?: boolean; time?: string }) =>
    req<{ ok: boolean }>(`/schedule/${id}`, { method: 'PUT', body: JSON.stringify(body) }),
  runNow: (id: number) => req<{ ok: boolean; execution_id?: number }>(`/schedule/${id}/run-now`, { method: 'POST' }),

  notifyStatus: () => req<NotifyStatus>('/notify/status'),
  notifyTest: () => req<{ ok: boolean; sent?: boolean }>('/notify/test', { method: 'POST' }),

  users: () => req<UserRow[]>('/users'),
  createUser: (body: { username: string; password: string; role?: string }) =>
    req<{ ok: boolean }>('/users', { method: 'POST', body: JSON.stringify(body) }),
  deleteUser: (username: string) =>
    req<{ ok: boolean }>(`/users/${encodeURIComponent(username)}`, { method: 'DELETE' }),

  llmStatus: () => req<LlmStatus>('/llm/status'),
  llmAsk: (question: string, context = '') =>
    req<{ ok: boolean; answer?: string; error?: string; disclaimer?: string }>('/llm/ask', {
      method: 'POST', body: JSON.stringify({ question, context }),
    }),

  forceCheckin: (id: number, body: Record<string, unknown>) =>
    req<ExecResult<Record<string, unknown>>>(`/accounts/${id}/force-checkin`, {
      method: 'POST', body: JSON.stringify(body),
    }),
  teacherMakeup: (id: number, body: Record<string, unknown>) =>
    req<ExecResult<Record<string, unknown>>>(`/accounts/${id}/teacher-makeup`, {
      method: 'POST', body: JSON.stringify(body),
    }),
  sign: (id: number, body: Record<string, unknown>) =>
    req<ExecResult<Record<string, unknown>>>(`/accounts/${id}/attendance/sign`, {
      method: 'POST', body: JSON.stringify(body),
    }),
  syncAttendance: (id: number, termId = '') =>
    req<ExecResult<Record<string, unknown>>>(`/accounts/${id}/attendance/sync`, {
      method: 'POST', body: JSON.stringify({ term_id: termId }),
    }),
};

// ---- 这些接口的返回形状 ----

/** 一条考勤场次。**唯一来源** —— 组件从这里引，不再各写一份。 */
export interface AttendanceItem {
  id: number;
  detail_id: string;
  attendance_id: string;
  title: string;
  course_name: string;
  attendance_type: number;
  status: number | null;
  open_time: number;
  finish_time: number;
  sign_role: number | null;
  sign_time: number | null;
  location_type: number | null;
  /** 平台**要求**的坐标与半径 —— 用于自动定位签到 */
  longitude?: string;
  latitude?: string;
  radius?: number | null;
  address_name?: string;
}
export interface AttendanceResp {
  items: AttendanceItem[];
  terms: { id: string; name: string }[];
  summary: {
    total: number; signed: number; missed: number; pending: number;
    upcoming: number; rate: number;
    /** 类型分布。**后端少返回这个字段前端会直接崩**（对它做 Object.entries）。 */
    by_type: Record<string, number>;
  };
  pending_items: AttendanceItem[];
}
export interface AutoSignResp {
  enabled: boolean;
  /** 总开关。**少这个字段按钮会永久禁用** —— 前端写的是 disabled={!available}。 */
  available: boolean;
  interval_seconds: number;
  hint: string;
}
export interface TasksResp {
  items: TaskRow[];
  total: number;
  pending: number;
  error: string;
  /** 列表为空时后端给出的结论：区分「平台真没有作业」与「查询链路出错」 */
  diagnosis?: string;
  /** 查询用的是哪个学期（空结果时让用户知道查的是哪一学期） */
  term_name?: string;
}
export interface TaskRow {
  course_pk: number; course_name: string; term_name: string;
  res_id: string; ctl_id: string; name: string;
  study_status: number; done: boolean;
  open_time: number; close_time: number | null;
  task_type: number; kind: string;
}
export interface TaskDetail {
  test_id: string; count: number;
  questions: { id: string; question_no: string; question_type: string; stem: string; score: number | null }[];
}
export interface ScheduleRow {
  account_id: number; label: string; enabled: boolean; time: string;
  today_plan: { id: number; biz_date: string; fire_at: number; state: string; detail: string } | null;
  last_check: {
    at: number; biz_date: string; new_resources: number | null;
    todo_resources: number | null; pending_tasks: number | null; task_error: string | null;
  } | null;
}
export interface NotifyStatus {
  enabled: boolean; mode: string; configured: boolean;
  pending: number | null; summary_at: string;
}
export interface UserRow {
  username: string; role: string; role_label: string;
  account_ids: number[]; created_at: number; updated_at: number;
}
export interface LlmStatus { available: boolean; model: string; base_url: string }

export interface Account {
  id: number; label: string; username: string; user_id: string | null;
  tenant_id: string; state: string; token_alive: boolean; note: string;
  /** 班级（来自上游 deptName）。界面上按它分组。 */
  dept: string;
  /** 模拟设备 ID（UUID）。留空 = 还没生成过。 */
  device_id: string;
  /** **签到是否带 deviceId**（与 device_id 解耦，这是开关） */
  device_id_enabled: boolean;
  last_login_at: number; courses: number;
}
export interface Course {
  id: number; course_name: string; term_id: string; term_name: string;
  teach_course_id: string; teach_class_id: string;
  res_total: number; res_done: number; progress: string;
}
export interface Resource {
  id: number; res_id: string; name: string; res_type: number;
  /** 所属课程（本地 `courses.id`）。页面按课程分组要用。 */
  course_pk: number;
  res_ext: string; study_status: number; play_progress: number | null;
  /** **是否计入考核**（1=计入）。考核分只由这部分产生。 */
  is_assess: number;
  open_time: number; close_time: number | null;
  res_desc: string;
}
export interface Execution {
  id: number; account_id: number; job_key: string; biz_date: string;
  state: string; detail: string; started_at: number; finished_at: number;
}
export interface LogRow { id: number; level: string; message: string; ts: number }
export interface ScanResult {
  term_id: string; term_name: string; courses: number; resources: number;
}
export interface StudyBody {
  term_id?: string; course_pk?: number; res_ids?: string[];
  res_types?: number[]; limit?: number; delay?: number;
  /** 只刷**计入考核**的资源 */
  only_assessed?: boolean;
  rescan?: boolean;
}

// ───────────────────────── 考试 ─────────────────────────

/** 已交卷考试列表里的一条。 */
export interface ExamSummary {
  examId: string;
  /** 取试卷详情要它，不是 examId */
  candidateId: string;
  examName: string;
  startTime: number;
  endTime: number;
  /** 时长（分钟）。平台给的是字符串 */
  duration: string;
  /** **卷面分**，不是实际得分 —— 实际得分只有试卷详情里有 */
  score: string;
  status: string;
  statusValue: string;
  submitTime?: number | null;
}

export interface ExamListResp {
  total: number;
  items: ExamSummary[];
}

export interface ExamChoice {
  /** 平台键名就是 `choiceId` / `choiceContent`（不是 `id` / `content`） */
  choiceId: string;
  choiceContent: string;
}

/**
 * 一道题。
 *
 * ⚠️ 这里的键名是**平台原始名**（后端 serde 的 `rename` 两个方向都生效）：
 * `questionNo` / `questionScore` / `candidateScore` / `candidateAnswer`，
 * 不是 Rust 结构体里的 `no` / `score` / `my_score` / `my_answer`。
 */
export interface ExamQuestion {
  questionType: string;
  questionNo: string;
  stem: string;
  stemText: string;
  questionScore: number;
  candidateScore: number;
  /** 单选时是 1 个 id；多选时是多个 */
  candidateAnswer: string[];
  /** 平台给的是逗号串，后端已切成数组 */
  standardAnswer: string[];
  analysis: string;
  choices: ExamChoice[];
}

export interface ExamPart {
  partId: string;
  partName: string;
  partDesc: string;
  paperQuestionQty?: number | null;
  /** 题目包在 `paperQuestion` 里（平台的结构，不是我们的冗余） */
  paperQuestionList: { paperQuestion: ExamQuestion }[];
}

export interface ExamPaper {
  /** 平台算出来的最终得分 */
  calculateScore: number;
  publishPaper: boolean;
  publishScore: boolean;
  /** 试卷本体 —— 顶层字段名字是平台的原始名 */
  resExamCandidatePaper: {
    id: string;
    examId: string;
    candidateId: string;
    examName: string;
    nickName: string;
    number: string;
    name: string;
    paperScore: number;
    paperQuestionQty: number;
    objectiveScore: number;
    subjectiveScore: number;
    finalScore: number;
    submitTime?: number | null;
    markCompleteTime?: number | null;
    paperParts: ExamPart[];
  };
}

// ───────────────────────── 作业（智能考试子系统） ─────────────────────────

export interface HomeworkQuestion {
  questionNo: string;
  questionType: string;
  stem: string;
  stemText: string;
  /** 本题满分 */
  questionScore: number | null;
  /** 我在这道题上的得分（未作答/未批阅为 0） */
  candidateScore: number;
  /** 我的答案（单选是一个 id；填空/主观是文本；未作答为 null） */
  candidateAnswer: string | null;
  /** 标准答案（未交卷/未批阅时平台不下发，空数组） */
  standardAnswer: string[];
  analysis: string;
  choices: { choiceId: string; choiceContent: string }[];
}

export interface HomeworkPart {
  partName: string;
  partDesc: string;
  questions: HomeworkQuestion[];
}

export interface HomeworkPaper {
  name: string;
  /** 满分（平台给的是字符串） */
  paperScore: string;
  questionQty: number | null;
  openTimeStr: string;
  closeTimeStr: string;
  parts: HomeworkPart[];
}

// ───────────────────────── 作业（含已交，走 review 链路） ─────────────────────────

export interface HomeworkItem {
  test_id: string;
  name: string;
  course_name: string;
  /** 已交卷才需要它去取我的答案/得分 */
  candidate_id: string;
  res_id: string;
  data_id: string;
  /** 2=测试 3=作业 */
  kind: number;
  study_status: number;
  status: number;
  /** 1=已批阅 0=未批阅 */
  mark_status: number | null;
  is_assess: number;
  open_time: number;
  close_time: number;
  open_time_str: string;
  close_time_str: string;
  /** 已交 */
  done: boolean;
}

export interface HomeworksResp {
  items: HomeworkItem[];
  total: number;
  pending: number;
  term_name: string;
  error: string;
  diagnosis?: string;
}

export interface HomeworkDetailQuestion {
  questionNo: string;
  questionType: string;
  stem: string;
  stemText: string;
  questionScore: number | null;
  candidateScore: number | null;
  /** 我的答案（选择题是选项 id，多选用逗号连接；文件题是文字说明） */
  myAnswerText: string;
  /** 我的作答附件（带 eid，可下载） */
  attachments: { name: string; eid: string }[];
  /** 任务附件（老师给的指导书）的 eid 与文件名 */
  taskEid: string;
  taskResName: string;
  /** 我的作答附件的 eid（文件题交卷后才有） */
  answerEid: string;
  answerResName: string;
  standardAnswer: unknown;
  analysis: string;
  choices: { choiceId: string; choiceContent: string }[];
}

export interface HomeworkDetail {
  test_id: string;
  name: string;
  submitted: boolean;
  paper_score: number | string | null;
  question_qty: number | null;
  open_time_str: string;
  close_time_str: string;
  final_score: number | null;
  objective_score: number | null;
  subjective_score: number | null;
  calculate_score: number | string | null;
  mark_status: number | null;
  submit_time: number | null;
  mark_time: number | null;
  /** 平台允许显示分数 / 答案与解析 */
  can_show_score: boolean;
  can_show_answer: boolean;
  parts: { partName: string; partDesc: string; questions: HomeworkDetailQuestion[] }[];
  note?: string;
  history_id?: string;
}
