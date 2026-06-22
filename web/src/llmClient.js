// ── LLM provider definitions ──────────────────────────────────────────────────
//
// authStyle:
//   'bearer'     → Authorization: Bearer <key>   (OpenAI, OpenRouter, Ollama, most others)
//   'anthropic'  → x-api-key: <key> + anthropic-version header + native message schema
//   'none'       → no auth header (Ollama local with no key set)

export const PROVIDERS = [
  {
    id: 'anthropic',
    label: 'Anthropic (direct)',
    baseUrl: 'https://api.anthropic.com',
    authStyle: 'anthropic',
    defaultModel: 'claude-sonnet-4-6',
    note: 'Requires anthropic-dangerous-direct-browser-access header (Anthropic\'s opt-in for browser use).',
  },
  {
    id: 'openrouter',
    label: 'OpenRouter',
    baseUrl: 'https://openrouter.ai/api/v1',
    authStyle: 'bearer',
    defaultModel: 'anthropic/claude-sonnet-4-6',
    note: 'One key for Claude, GPT, Gemini, and others. OpenAI-compatible API.',
  },
  {
    id: 'openai',
    label: 'OpenAI',
    baseUrl: 'https://api.openai.com/v1',
    authStyle: 'bearer',
    defaultModel: 'gpt-4o',
    note: null,
  },
  {
    id: 'ollama',
    label: 'Ollama (local)',
    baseUrl: 'http://localhost:11434/v1',
    authStyle: 'none',
    defaultModel: 'qwen2.5-coder:14b',
    note: 'Requires OLLAMA_ORIGINS=* if the app is not served from localhost.',
  },
  {
    id: 'custom',
    label: 'Custom…',
    baseUrl: '',
    authStyle: 'bearer',
    defaultModel: '',
    note: 'Any OpenAI-compatible endpoint.',
  },
];

// ── Storage ───────────────────────────────────────────────────────────────────
//
// The API key is stored in localStorage under a dedicated key, separate from
// the main settings store.  It is only ever sent to the configured provider URL.

const STORAGE_KEY = 'openlrlens.llm';

export function loadLlmConfig() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

export function saveLlmConfig(config) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(config));
}

export function clearLlmConfig() {
  localStorage.removeItem(STORAGE_KEY);
}

// ── API call ──────────────────────────────────────────────────────────────────
//
// config: { providerId, baseUrl, apiKey, model }
// messages: [{ role: 'system'|'user'|'assistant', content: string }]
// tools: OpenAI-format tool definitions (optional)
//
// Returns: { ok: bool, content: string|null, tool_calls: array|null, error: string|null }

export async function chatComplete(config, messages, tools) {
  const provider = PROVIDERS.find(p => p.id === config.providerId);
  const authStyle = provider?.authStyle ?? 'bearer';

  if (authStyle === 'anthropic') {
    return anthropicComplete(config, messages, tools);
  }
  return openaiComplete(config, messages, tools, authStyle);
}

async function openaiComplete({ baseUrl, apiKey, model }, messages, tools, authStyle) {
  const headers = { 'Content-Type': 'application/json' };
  if (authStyle === 'bearer' && apiKey) {
    headers['Authorization'] = `Bearer ${apiKey}`;
  }

  const body = { model, messages };
  if (tools?.length) body.tools = tools;

  try {
    const res = await fetch(`${baseUrl}/chat/completions`, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
    const data = await res.json();
    if (!res.ok) return { ok: false, content: null, tool_calls: null, error: data.error?.message ?? `HTTP ${res.status}` };
    const msg = data.choices?.[0]?.message;
    return { ok: true, content: msg?.content ?? null, tool_calls: msg?.tool_calls ?? null, error: null };
  } catch (e) {
    return { ok: false, content: null, tool_calls: null, error: e.message };
  }
}

async function anthropicComplete({ baseUrl, apiKey, model }, messages, tools) {
  // Anthropic native schema: system prompt is a top-level field; tools have input_schema.
  const systemMsg = messages.find(m => m.role === 'system');
  const nonSystem = messages.filter(m => m.role !== 'system');

  const headers = {
    'Content-Type': 'application/json',
    'x-api-key': apiKey,
    'anthropic-version': '2023-06-01',
    'anthropic-dangerous-direct-browser-access': 'true',
  };

  const body = { model, messages: nonSystem, max_tokens: 4096 };
  if (systemMsg) body.system = systemMsg.content;
  if (tools?.length) {
    // Convert OpenAI tool format → Anthropic tool format
    body.tools = tools.map(t => ({
      name: t.function.name,
      description: t.function.description,
      input_schema: t.function.parameters,
    }));
  }

  try {
    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
    const data = await res.json();
    if (!res.ok) return { ok: false, content: null, tool_calls: null, error: data.error?.message ?? `HTTP ${res.status}` };

    // Normalize response back to OpenAI-like shape
    const textBlock = data.content?.find(b => b.type === 'text');
    const toolBlocks = data.content?.filter(b => b.type === 'tool_use') ?? [];
    const tool_calls = toolBlocks.length
      ? toolBlocks.map(b => ({ id: b.id, type: 'function', function: { name: b.name, arguments: JSON.stringify(b.input) } }))
      : null;
    return { ok: true, content: textBlock?.text ?? null, tool_calls, error: null };
  } catch (e) {
    return { ok: false, content: null, tool_calls: null, error: e.message };
  }
}

// Convenience: send a minimal message to verify connectivity and auth.
export async function testConnection(config) {
  return chatComplete(
    config,
    [{ role: 'user', content: 'Reply with just the word OK.' }],
  );
}
