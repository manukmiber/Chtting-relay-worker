// src/handlers/setting.js
import { getSettings, saveSettings, clearHistory, SETTING_KEYS, DEFAULT_SETTINGS } from '../services/r2.js';

/* ─── Simbol marker untuk mendeteksi pesan setting ──────────── */
const SETTING_MARKER = '⚙️ *SETTING BOT*';

/* ─── Tampilkan template setting ────────────────────────────── */

export async function showSettingTemplate(sock, jid, userId) {
  const current = await getSettings(userId);

  const lines = [
    SETTING_MARKER,
    '',
    '📝 *Edit lalu kirim balik pesan ini*',
    '(Boleh hapus baris yang tidak ingin diubah)',
    '',
    '─────────────────────────',
  ];

  for (const { key, label, hint } of SETTING_KEYS) {
    const val = current[key] ?? DEFAULT_SETTINGS[key] ?? '';
    lines.push(`${label}: ${val}`);
    lines.push(`  _(${hint})_`);
    lines.push('');
  }

  lines.push('─────────────────────────');
  lines.push('💡 Gunakan /reset untuk hapus history chat');
  lines.push('💡 Gunakan /status untuk lihat setting aktif');

  await sock.sendMessage(jid, { text: lines.join('\n') });
}

/* ─── Deteksi apakah pesan ini adalah reply setting ─────────── */

export function isSettingMessage(text) {
  if (!text) return false;
  // Cek apakah mengandung minimal 1 key yang valid dalam format KEY: value
  return SETTING_KEYS.some(({ label }) =>
    new RegExp(`^${label}\\s*:`, 'm').test(text)
  );
}

/* ─── Parse dan simpan setting dari teks ────────────────────── */

export async function parseAndSaveSetting(sock, jid, userId, text) {
  const current = await getSettings(userId);
  const updated = { ...current };
  const changed = [];

  for (const { key, label } of SETTING_KEYS) {
    const match = text.match(new RegExp(`^${label}\\s*:\\s*(.+)$`, 'm'));
    if (!match) continue;

    const raw = match[1].trim();

    // Validasi per key
    const { valid, value, error } = validateSettingValue(key, raw);
    if (!valid) {
      await sock.sendMessage(jid, {
        text: `❌ *${label}* tidak valid: ${error}`,
      });
      return;
    }

    if (String(current[key]) !== String(value)) {
      updated[key] = value;
      changed.push(`✅ ${label}: \`${value}\``);
    }
  }

  if (changed.length === 0) {
    await sock.sendMessage(jid, { text: '⚠️ Tidak ada setting yang berubah.' });
    return;
  }

  await saveSettings(userId, updated);

  const msg = [
    '✅ *Setting berhasil disimpan!*',
    '',
    '*Perubahan:*',
    ...changed,
    '',
    '_Kirim /status untuk melihat semua setting aktif._',
  ].join('\n');

  await sock.sendMessage(jid, { text: msg });
}

/* ─── Tampilkan status setting aktif ────────────────────────── */

export async function showStatus(sock, jid, userId) {
  const s = await getSettings(userId);

  const apiKeyDisplay = s.apiKey
    ? `${s.apiKey.slice(0, 6)}...${s.apiKey.slice(-4)}`
    : '❌ Belum diset';

  const lines = [
    '📊 *Status Setting Aktif*',
    '',
    `🔑 API Key     : ${apiKeyDisplay}`,
    `🤖 Model       : ${s.model}`,
    `👤 Karakter    : ${s.characterName}`,
    `📝 Deskripsi   : ${s.characterDesc?.slice(0, 60) || '-'}`,
    `🌐 Bahasa      : ${s.language === 'id' ? 'Indonesia' : 'English'}`,
    `📜 History max : ${s.maxHistory} pesan`,
    `💬 Reply mode  : ${s.replyMode}`,
    `⚡ Bot aktif   : ${s.active ? 'Ya' : 'Tidak'}`,
    '',
    s.systemPrompt
      ? `_System prompt custom aktif (${s.systemPrompt.length} karakter)_`
      : '_System prompt: auto dari CHARACTER_DESC_',
  ];

  await sock.sendMessage(jid, { text: lines.join('\n') });
}

/* ─── Handle /reset ─────────────────────────────────────────── */

export async function handleReset(sock, jid, userId) {
  await clearHistory(userId);
  await sock.sendMessage(jid, {
    text: '🗑️ History chat berhasil dihapus. Percakapan dimulai dari awal.',
  });
}

/* ─── Handle /help ──────────────────────────────────────────── */

export async function handleHelp(sock, jid) {
  const prefix = process.env.CMD_PREFIX ?? '/';
  const lines = [
    `🤖 *${process.env.BOT_NAME ?? 'AI Bot'} — Daftar Perintah*`,
    '',
    `${prefix}setting  — Buka template konfigurasi bot`,
    `${prefix}status   — Lihat setting yang sedang aktif`,
    `${prefix}reset    — Hapus history percakapan`,
    `${prefix}help     — Tampilkan pesan ini`,
    '',
    '_Untuk mulai chat, cukup kirim pesan biasa._',
    '_Pastikan API key sudah diset via_ ${prefix}setting',
  ];
  await sock.sendMessage(jid, { text: lines.join('\n') });
}

/* ─── Validator ─────────────────────────────────────────────── */

function validateSettingValue(key, raw) {
  switch (key) {
    case 'apiKey':
      if (raw.length < 10) return { valid: false, error: 'Terlalu pendek, minimal 10 karakter.' };
      return { valid: true, value: raw };

    case 'model':
      return { valid: true, value: raw };

    case 'language':
      if (!['id', 'en'].includes(raw.toLowerCase()))
        return { valid: false, error: 'Hanya "id" atau "en" yang valid.' };
      return { valid: true, value: raw.toLowerCase() };

    case 'maxHistory': {
      const n = parseInt(raw);
      if (isNaN(n) || n < 5 || n > 50)
        return { valid: false, error: 'Harus angka antara 5–50.' };
      return { valid: true, value: n };
    }

    case 'replyMode':
      if (!['text', 'quote'].includes(raw.toLowerCase()))
        return { valid: false, error: 'Hanya "text" atau "quote" yang valid.' };
      return { valid: true, value: raw.toLowerCase() };

    default:
      return { valid: true, value: raw };
  }
}
