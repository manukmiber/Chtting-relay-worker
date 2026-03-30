// src/services/workerChat.js

/**
 * Kirim pesan ke worker-chat dan kumpulkan seluruh SSE stream
 * menjadi 1 string response.
 *
 * @param {object}   opts
 * @param {string}   opts.apiKey       - API key user
 * @param {string}   opts.model        - model name
 * @param {string}   opts.systemPrompt - system prompt
 * @param {Array}    opts.messages     - array { role, content }
 * @returns {Promise<string>}
 */
export async function callWorkerChat({ apiKey, model, systemPrompt, messages }) {
  const url = process.env.WORKER_CHAT_URL;
  if (!url) throw new Error('WORKER_CHAT_URL tidak di-set di .env');

  const body = {
    model,
    messages,
    ...(systemPrompt ? { system: systemPrompt } : {}),
  };

  const res = await fetch(url, {
    method: 'POST',
    headers: {
      'Authorization': `Bearer ${apiKey}`,
      'Content-Type' : 'application/json',
    },
    body: JSON.stringify(body),
    // Signal timeout 2 menit
    signal: AbortSignal.timeout(120_000),
  });

  if (!res.ok) {
    const errText = await res.text().catch(() => '');
    throw new WorkerChatError(res.status, errText);
  }

  // ── Consume SSE stream ──────────────────────────────────────
  const reader  = res.body.getReader();
  const decoder = new TextDecoder();
  let   fullText = '';
  let   buffer   = '';

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });

    // Proses per baris
    const lines = buffer.split('\n');
    buffer = lines.pop(); // sisa yang belum lengkap

    for (const line of lines) {
      const trimmed = line.trim();
      if (!trimmed.startsWith('data:')) continue;

      const data = trimmed.slice(5).trim();
      if (data === '[DONE]') break;

      try {
        const json  = JSON.parse(data);
        const delta = json.choices?.[0]?.delta?.content ?? '';
        fullText += delta;
      } catch {
        // skip malformed chunk
      }
    }
  }

  return fullText.trim();
}

/* ─── Custom Error ───────────────────────────────────────────── */

export class WorkerChatError extends Error {
  constructor(status, body) {
    super(`Worker-chat error ${status}: ${body.slice(0, 200)}`);
    this.status = status;
    this.body   = body;
  }
}

/* ─── Build system prompt dari settings ─────────────────────── */

export function buildSystemPrompt(settings) {
  // Jika user sudah set system prompt custom, pakai itu
  if (settings.systemPrompt?.trim()) return settings.systemPrompt.trim();

  const lang = settings.language === 'en' ? 'English' : 'Bahasa Indonesia';

  return [
    `Kamu adalah ${settings.characterName}.`,
    settings.characterDesc ? settings.characterDesc : '',
    `Selalu balas dalam ${lang}.`,
    'Balas dengan singkat dan natural seperti chat WhatsApp.',
    'Jangan gunakan markdown yang berlebihan.',
  ].filter(Boolean).join(' ');
}
