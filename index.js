// src/index.js
import 'dotenv/config';
import path from 'path';
import { fileURLToPath } from 'url';
import pino from 'pino';
import qrcode from 'qrcode-terminal';

import makeWASocket, {
  useMultiFileAuthState,
  DisconnectReason,
  fetchLatestBaileysVersion,
  makeCacheableSignalKeyStore,
  makeInMemoryStore,
} from '@whiskeysockets/baileys';

import { routeMessage } from './handlers/message.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const AUTH_DIR  = path.join(__dirname, '..', 'auth_info');
const STORE_DIR = path.join(__dirname, '..', 'store');

// Logger — set level ke 'warn' supaya tidak terlalu verbose
const logger = pino({ level: 'warn' });

/* ─── In-memory store (opsional, untuk load pesan lama) ─────── */
const store = makeInMemoryStore({ logger });
store.readFromFile(`${STORE_DIR}/store.json`);
setInterval(() => store.writeToFile(`${STORE_DIR}/store.json`), 10_000);

/* ─── Koneksi utama ─────────────────────────────────────────── */

async function connectToWhatsApp() {
  const { state, saveCreds } = await useMultiFileAuthState(AUTH_DIR);
  const { version }          = await fetchLatestBaileysVersion();

  console.log(`\n🤖 ${process.env.BOT_NAME ?? 'AI Bot'} starting...`);
  console.log(`📦 Baileys v${version.join('.')}\n`);

  const sock = makeWASocket({
    version,
    logger,
    auth: {
      creds: state.creds,
      keys : makeCacheableSignalKeyStore(state.keys, logger),
    },
    printQRInTerminal: false, // kita handle sendiri biar lebih bersih
    markOnlineOnConnect: true,
    generateHighQualityLinkPreview: false,
    // Abaikan status update & broadcast untuk efisiensi
    shouldIgnoreJid: jid =>
      jid === 'status@broadcast' ||
      jid.endsWith('@newsletter'),
  });

  // Bind store ke socket
  store.bind(sock.ev);

  /* ── QR Code ──────────────────────────────────────────────── */
  sock.ev.on('connection.update', async ({ connection, lastDisconnect, qr }) => {
    if (qr) {
      console.log('\n📱 Scan QR code ini dengan WhatsApp kamu:\n');
      qrcode.generate(qr, { small: true });
    }

    if (connection === 'close') {
      const statusCode  = lastDisconnect?.error?.output?.statusCode;
      const shouldRecon = statusCode !== DisconnectReason.loggedOut;

      console.log(`\n⚠️  Koneksi terputus (${statusCode}). Reconnect: ${shouldRecon}`);

      if (shouldRecon) {
        setTimeout(connectToWhatsApp, 3_000);
      } else {
        console.log('🚪 Logged out. Hapus folder auth_info lalu restart.');
        process.exit(1);
      }
    }

    if (connection === 'open') {
      console.log(`\n✅ WhatsApp terhubung!`);
      console.log(`📞 Nomor: ${sock.user?.id?.split(':')[0]}`);
      console.log(`💬 Siap menerima pesan.\n`);
    }
  });

  /* ── Simpan credentials saat update ──────────────────────── */
  sock.ev.on('creds.update', saveCreds);

  /* ── Handle pesan masuk ──────────────────────────────────── */
  sock.ev.on('messages.upsert', async ({ messages, type }) => {
    if (type !== 'notify') return; // hanya proses pesan baru (bukan history)

    for (const msg of messages) {
      try {
        await routeMessage(sock, msg);
      } catch (err) {
        console.error('[main] error handling message:', err);
      }
    }
  });

  return sock;
}

/* ─── Buat direktori yang dibutuhkan ───────────────────────── */
import fs from 'fs';
fs.mkdirSync(AUTH_DIR,  { recursive: true });
fs.mkdirSync(STORE_DIR, { recursive: true });

/* ─── Start ─────────────────────────────────────────────────── */
connectToWhatsApp();
