// src/handlers/chat.js
import { getHistory, saveHistory, getSettings } from '../services/r2.js';
import { callWorkerChat, buildSystemPrompt, WorkerChatError } from '../services/workerChat.js';

/**
 * Handle pesan chat biasa → kirim ke worker-chat → balas ke user.
 */
export async function handleChat(sock, jid, userId, userText, quotedMsgId = null) {
  const settings = await getSettings(userId);

  // Cek API key
  const apiKey = settings.apiKey || process.env.DEFAULT_API_KEY;
  if (!apiKey) {
    await sock.sendMessage(jid, {
      text: '⚠️ API key belum diset. Kirim */setting* untuk mengkonfigurasi bot.',
    });
    return;
  }

  // Cek apakah bot aktif untuk user ini
  if (!settings.active) return;

  // ── Kirim "typing..." indicator ──────────────────────────────
  await sock.sendPresenceUpdate('composing', jid);

  let aiReply;

  try {
    // Load history
    const history = await getHistory(userId);

    // Tambah pesan user ke history
    const messages = [
      ...history,
      { role: 'user', content: userText, ts: Date.now() },
    ];

    // Build system prompt
    const systemPrompt = buildSystemPrompt(settings);

    // Panggil worker-chat
    aiReply = await callWorkerChat({
      apiKey,
      model   : settings.model,
      systemPrompt,
      messages: messages.map(({ role, content }) => ({ role, content })), // strip ts
    });

    // Simpan history (user + assistant)
    messages.push({ role: 'assistant', content: aiReply, ts: Date.now() });
    await saveHistory(userId, messages);

  } catch (err) {
    await sock.sendPresenceUpdate('paused', jid);

    if (err instanceof WorkerChatError) {
      const msg = friendlyError(err.status, err.body);
      await sock.sendMessage(jid, { text: msg });
    } else {
      console.error('[chat] unexpected error:', err);
      await sock.sendMessage(jid, {
        text: '❌ Terjadi kesalahan tak terduga. Coba lagi sebentar.',
      });
    }
    return;
  }

  await sock.sendPresenceUpdate('paused', jid);

  // ── Kirim balasan ────────────────────────────────────────────
  if (settings.replyMode === 'quote' && quotedMsgId) {
    await sock.sendMessage(jid, {
      text: aiReply,
      quoted: { key: { id: quotedMsgId, remoteJid: jid } },
    });
  } else {
    await sock.sendMessage(jid, { text: aiReply });
  }
}

/* ─── Terjemah error code ke pesan ramah ───────────────────── */

function friendlyError(status, body) {
  if (status === 401) return '🔑 API key tidak valid. Cek via */setting*.';
  if (status === 429) return '⏳ Rate limit tercapai. Tunggu sebentar lalu coba lagi.';
  if (status === 402) return '💳 Quota habis. Cek plan kamu di worker-chat.';
  if (status >= 500)  return '🌐 Server AI sedang bermasalah. Coba lagi nanti.';
  return `❌ Error ${status}. Coba lagi atau hubungi admin.`;
}
