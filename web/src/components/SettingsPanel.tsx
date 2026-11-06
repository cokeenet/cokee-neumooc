/**
 * 系统设置面板。
 *
 * 目前只有 AI（LLM）配置 —— 用于「作业 → AI 代答」。
 *
 * 几个刻意的设计：
 *
 * * **key 永不回明文** —— 后端只回 `has_key` 和前 4/后 4 位。
 *   输入框留空 = 不修改，避免把打码值当真 key 存回去。
 * * **环境变量优先** —— 被 env 占用的项会标「被环境变量覆盖」，免得
 *   用户在界面上改半天发现不生效。
 * * **保存后真发一次请求** 才算配置成功，不靠"保存成功"骗自己。
 */
import { useCallback, useEffect, useState } from 'react';
import { api, type LlmConfig } from '../lib/api';
import { useAction } from '../lib/useResource';

export default function SettingsPanel({ me }: { me: string }) {
  const [cfg, setCfg] = useState<LlmConfig | null>(null);
  const [err, setErr] = useState('');
  const [loading, setLoading] = useState(true);

  // 表单本地状态
  const [apiKey, setApiKey] = useState('');
  const [model, setModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');

  const load = useCallback(async () => {
    setLoading(true);
    setErr('');
    try {
      const r = await api.llmConfig();
      setCfg(r);
      setModel(r.model);
      setBaseUrl(r.base_url);
      setApiKey(''); // 永远不回填 key
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  const save = useAction(
    async () => {
      await api.llmSetConfig({
        api_key: apiKey.trim() || undefined, // 空 = 不改
        model: model.trim(),
        base_url: baseUrl.trim(),
      });
      await load();
    },
    { okText: '已保存（立即生效，无需重启）' },
  );

  const test = useAction(async () => {
    const r = await api.llmTest();
    if (!r.ok) throw new Error(r.error || '测试失败');
  });

  if (loading) return <p className="muted">正在读取配置…</p>;

  return (
    <div className="row" style={{ gap: 16, alignItems: 'flex-start', flexWrap: 'wrap' }}>
      <div className="card" style={{ flex: '1 1 420px', minWidth: 340 }}>
        <div className="card-head">
          <h2 className="card-title">AI 配置</h2>
          <span className="card-meta">
            {cfg?.has_key
              ? <span className="badge badge-ok">已配置</span>
              : <span className="badge badge-muted">未配置</span>}
          </span>
        </div>

        {err && <p className="msg-bar msg-err small">{err}</p>}

        <p className="muted small" style={{ marginTop: 0 }}>
          用于「作业 → AI 代答」。配置好后不用重启。
        </p>

        <div className="field">
          <label htmlFor="llm-key">
            API Key
            {cfg?.key_from_env && <span className="badge badge-muted" style={{ marginLeft: 6 }}>被环境变量覆盖</span>}
          </label>
          <input id="llm-key" className="input" type="password"
            placeholder={cfg?.has_key ? `已配置 ${cfg.key_hint}（留空不修改）` : 'sk-...'}
            value={apiKey} onChange={(e) => setApiKey(e.target.value)}
            autoComplete="off" />
          <span className="muted small">
            留空 = 保留现有 key。要清除请点下面的「清除 Key」。
          </span>
        </div>

        <div className="field">
          <label htmlFor="llm-model">
            模型
            {cfg?.model_from_env && <span className="badge badge-muted" style={{ marginLeft: 6 }}>被环境变量覆盖</span>}
          </label>
          <input id="llm-model" className="input" placeholder="glm-4.5-flash"
            value={model} onChange={(e) => setModel(e.target.value)} />
        </div>

        <div className="field">
          <label htmlFor="llm-base">
            API 地址
            {cfg?.base_url_from_env && <span className="badge badge-muted" style={{ marginLeft: 6 }}>被环境变量覆盖</span>}
          </label>
          <input id="llm-base" className="input" placeholder="https://api.z.ai/api/paas/v4"
            value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
          <span className="muted small">
            任何 OpenAI 兼容端点都行（如 DeepSeek：<code>https://api.deepseek.com/v1</code>）
          </span>
        </div>

        <div className="row" style={{ gap: 8, marginTop: 14, flexWrap: 'wrap' }}>
          <button className="btn btn-primary" disabled={save.busy} onClick={() => void save.run()}>
            {save.busy ? '保存中…' : '保存'}
          </button>
          <button className="btn" disabled={test.busy || !cfg?.has_key} onClick={() => void test.run()}
            title={cfg?.has_key ? '用当前配置真发一次请求' : '还没配置 key'}>
            {test.busy ? '测试中…' : '测试连接'}
          </button>
          <button className="btn btn-danger" disabled={!cfg?.has_key}
            onClick={async () => {
              await api.llmSetConfig({ clear_key: true });
              await load();
            }}>
            清除 Key
          </button>
        </div>

        {save.msg && <p className={`msg-bar ${save.msg.kind === 'ok' ? 'msg-ok' : 'msg-err'} small`}>{save.msg.text}</p>}
        {test.msg && <p className={`msg-bar ${test.msg.kind === 'ok' ? 'msg-ok' : 'msg-err'} small`}>{test.msg.text}</p>}
      </div>

      <div className="card" style={{ flex: '1 1 300px', minWidth: 280 }}>
        <div className="card-head"><h2 className="card-title">说明</h2></div>
        <ul className="muted small" style={{ margin: 0, paddingLeft: 18, lineHeight: 1.9 }}>
          <li>配置只影响 <strong>AI 代答</strong>，与刷课/考勤无关。</li>
          <li>AI 只<strong>存草稿</strong>，交卷仍需手动确认。</li>
          <li>环境变量 <code>NEUMOOC_LLM_*</code> 优先于本页设置。</li>
          <li>保存后立即生效，<strong>无需重启</strong>。</li>
        </ul>
      </div>
    </div>
  );
}
