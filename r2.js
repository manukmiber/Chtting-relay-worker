// src/services/r2.js
import { S3Client, GetObjectCommand, PutObjectCommand, DeleteObjectCommand } from '@aws-sdk/client-s3';

const s3 = new S3Client({
  region: 'auto',
  endpoint: `https://${process.env.R2_ACCOUNT_ID}.r2.cloudflarestorage.com`,
  credentials: {
    accessKeyId: process.env.R2_ACCESS_KEY_ID,
    secretAccessKey: process.env.R2_SECRET_ACCESS_KEY,
  },
});

const BUCKET = process.env.R2_BUCKET_NAME ?? 'wa-ai-bot';

/* ─── Low-level helpers ─────────────────────────────────────── */

export async function getJson(key) {
  try {
    const res = await s3.send(new GetObjectCommand({ Bucket: BUCKET, Key: key }));
    const str = await res.Body.transformToString();
    return JSON.parse(str);
  } catch (e) {
    if (e.name === 'NoSuchKey' || e.$metadata?.httpStatusCode === 404) return null;
    throw e;
  }
}

export async function putJson(key, data) {
  await s3.send(new PutObjectCommand({
    Bucket: BUCKET,
    Key: key,
    Body: JSON.stringify(data, null, 2),
    ContentType: 'application/json',
  }));
}

export async function deleteKey(key) {
  await s3.send(new DeleteObjectCommand({ Bucket: BUCKET, Key: key }));
}

/* ─── Chat History ──────────────────────────────────────────── */
// Key: history/{userId}.json
// Value: Array<{ role, content, ts }>

export async function getHistory(userId) {
  return (await getJson(`history/${userId}.json`)) ?? [];
}

export async function saveHistory(userId, messages) {
  const maxHistory = parseInt(process.env.MAX_HISTORY ?? '30');
  const trimmed = messages.slice(-maxHistory);
  await putJson(`history/${userId}.json`, trimmed);
}

export async function clearHistory(userId) {
  await deleteKey(`history/${userId}.json`);
}

/* ─── User Settings ─────────────────────────────────────────── */
// Key: settings/{userId}.json
// Value: UserSettings object

/** @returns {UserSettings} */
export async function getSettings(userId) {
  const saved = await getJson(`settings/${userId}.json`);
  return { ...DEFAULT_SETTINGS, ...(saved ?? {}) };
}

export async function saveSettings(userId, settings) {
  await putJson(`settings/${userId}.json`, settings);
}

/* ─── Default Settings ──────────────────────────────────────── */

export const DEFAULT_SETTINGS = {
  apiKey         : '',          // API key untuk worker-chat
  model          : 'gpt-4o-mini',
  characterName  : 'Assistant',
  characterDesc  : 'Asisten AI yang helpful, ramah, dan informatif.',
  systemPrompt   : '',          // kosong = auto-generate dari characterDesc
  language       : 'id',        // id / en
  maxHistory     : 20,
  replyMode      : 'text',      // text | quote
  active         : true,        // false = bot tidak merespons user ini
};

/* ─── Setting Keys (untuk parsing) ──────────────────────────── */

export const SETTING_KEYS = [
  { key: 'apiKey',        label: 'API_KEY',         hint: 'API key untuk worker-chat (wajib)' },
  { key: 'model',         label: 'MODEL',           hint: 'Model AI. Contoh: gpt-4o-mini, gpt-4o, claude-sonnet-4-5' },
  { key: 'characterName', label: 'CHARACTER_NAME',  hint: 'Nama karakter bot kamu' },
  { key: 'characterDesc', label: 'CHARACTER_DESC',  hint: 'Deskripsi singkat kepribadian bot' },
  { key: 'systemPrompt',  label: 'SYSTEM_PROMPT',   hint: 'System prompt lengkap (opsional, override CHARACTER_DESC)' },
  { key: 'language',      label: 'LANGUAGE',        hint: 'Bahasa utama: id atau en' },
  { key: 'maxHistory',    label: 'MAX_HISTORY',     hint: 'Jumlah max pesan history (5–50)' },
  { key: 'replyMode',     label: 'REPLY_MODE',      hint: 'Mode balas: text atau quote' },
];
