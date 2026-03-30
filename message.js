// src/handlers/message.js
import {
  showSettingTemplate,
  isSettingMessage,
  parseAndSaveSetting,
  showStatus,
  handleReset,
  handleHelp,
} from './setting.js';
import { handleChat } from './chat.js';

const PREFIX = process.env.CMD_PREFIX ?? '/';

/**
 * Router utama untuk semua pesan masuk.
 * Dipanggil dari index.js setiap ada pesan baru.
 */
export async function routeMessage(sock, msg) {
  // ── Abaikan pesan dari bot sendiri ───────────────────────────
  if (msg.key.fromMe) return;

  // ── Ambil info pesan ─────────────────────────────────────────
  const jid = msg.key.remoteJid;
  if (!jid) return;

  // Abaikan pesan grup (opsional — hapus baris ini untuk support grup)
  const isGroup = jid.endsWith('@g.us');
  if (isGroup) return;

  const msgContent = msg.message;
  if (!msgContent) return;

  // Ekstrak teks
  const text = extractText(msgContent)?.trim();
  if (!text) return;

  // userId = nomor WA tanpa @s.whatsapp.net
  const userId = jid.replace('@s.whatsapp.net', '');

  // Quoted message id (untuk reply mode)
  const quotedMsgId = msgContent?.extendedTextMessage?.contextInfo?.stanzaId ?? null;

  console.log(`[msg] ${userId}: ${text.slice(0, 80)}`);

  // ── Deteksi apakah ini pesan setting (template diisi user) ───
  if (isSettingMessage(text)) {
    await parseAndSaveSetting(sock, jid, userId, text);
    return;
  }

  // ── Command routing ──────────────────────────────────────────
  if (text.startsWith(PREFIX)) {
    const [cmd, ...args] = text.slice(PREFIX.length).trim().split(/\s+/);

    switch (cmd.toLowerCase()) {
      case 'setting':
        await showSettingTemplate(sock, jid, userId);
        return;

      case 'status':
        await showStatus(sock, jid, userId);
        return;

      case 'reset':
        await handleReset(sock, jid, userId);
        return;

      case 'help':
      case 'start':
        await handleHelp(sock, jid);
        return;

      default:
        // Command tidak dikenal → tetap proses sebagai chat biasa
        break;
    }
  }

  // ── Chat biasa → kirim ke AI ─────────────────────────────────
  await handleChat(sock, jid, userId, text, quotedMsgId);
}

/* ─── Ekstrak teks dari berbagai tipe pesan ─────────────────── */

function extractText(msgContent) {
  return (
    msgContent?.conversation ??
    msgContent?.extendedTextMessage?.text ??
    msgContent?.imageMessage?.caption ??
    msgContent?.videoMessage?.caption ??
    null
  );
}
