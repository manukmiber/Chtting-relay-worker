/**
 * One row per relayed request. The shape is shared by both storage backends so
 * the dashboard sees identical data whether SQLite is available or not.
 */
export const REQUEST_FIELDS = [
  'id', 'ts', 'day', 'hour',
  'key_id', 'key_label', 'ip', 'user_agent',
  'public_model', 'backend_id', 'upstream_model', 'endpoint',
  'stream', 'status', 'error', 'finish_reason',
  'ttft_ms', 'total_ms', 'gen_ms',
  'prompt_tokens', 'completion_tokens', 'total_tokens',
  'cached_tokens', 'reasoning_tokens',
  'tokens_per_sec', 'usage_source', 'tokenizer', 'exact',
  'local_prompt', 'local_completion', 'drift_prompt', 'drift_completion',
  'req_preview', 'res_preview', 'retries',
];

export function emptyRecord() {
  return {
    id: '', ts: 0, day: '', hour: '',
    key_id: '', key_label: '', ip: '', user_agent: '',
    public_model: '', backend_id: '', upstream_model: '', endpoint: '',
    stream: 0, status: 0, error: '', finish_reason: '',
    ttft_ms: 0, total_ms: 0, gen_ms: 0,
    prompt_tokens: 0, completion_tokens: 0, total_tokens: 0,
    cached_tokens: 0, reasoning_tokens: 0,
    tokens_per_sec: 0, usage_source: '', tokenizer: '', exact: 1,
    local_prompt: 0, local_completion: 0, drift_prompt: 0, drift_completion: 0,
    req_preview: '', res_preview: '', retries: 0,
  };
}

export const CREATE_SQL = `
CREATE TABLE IF NOT EXISTS requests (
  id TEXT PRIMARY KEY,
  ts INTEGER NOT NULL,
  day TEXT NOT NULL,
  hour TEXT NOT NULL,
  key_id TEXT, key_label TEXT, ip TEXT, user_agent TEXT,
  public_model TEXT, backend_id TEXT, upstream_model TEXT, endpoint TEXT,
  stream INTEGER, status INTEGER, error TEXT, finish_reason TEXT,
  ttft_ms REAL, total_ms REAL, gen_ms REAL,
  prompt_tokens INTEGER, completion_tokens INTEGER, total_tokens INTEGER,
  cached_tokens INTEGER, reasoning_tokens INTEGER,
  tokens_per_sec REAL, usage_source TEXT, tokenizer TEXT, exact INTEGER,
  local_prompt INTEGER, local_completion INTEGER,
  drift_prompt INTEGER, drift_completion INTEGER,
  req_preview TEXT, res_preview TEXT, retries INTEGER
);
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts DESC);
CREATE INDEX IF NOT EXISTS idx_requests_day ON requests(day);
CREATE INDEX IF NOT EXISTS idx_requests_model ON requests(public_model);
CREATE INDEX IF NOT EXISTS idx_requests_key ON requests(key_id);
`;
