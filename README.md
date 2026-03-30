# 🤖 WA AI Bot

WhatsApp AI chatbot berbasis Baileys + Cloudflare Worker-Chat + R2 storage.

## Fitur

- Chat AI per-user dengan history percakapan
- `/setting` — konfigurasi API key, karakter, model, dll.
- `/reset` — hapus history chat
- `/status` — lihat setting aktif
- History & settings disimpan di **Cloudflare R2**

## Cara Setup

### 1. Install dependencies

```bash
npm install
```

### 2. Setup Cloudflare R2

1. Buka [Cloudflare Dashboard](https://dash.cloudflare.com) → **R2**
2. Buat bucket baru (contoh: `wa-ai-bot`)
3. Buat **R2 API Token** dengan permission `Object Read & Write`
4. Catat:
   - Account ID (ada di URL dashboard)
   - Access Key ID
   - Secret Access Key
   - Bucket name

### 3. Konfigurasi .env

```bash
cp .env.example .env
```

Edit `.env`:

```env
R2_ACCOUNT_ID=abc123...
R2_ACCESS_KEY_ID=xxx
R2_SECRET_ACCESS_KEY=yyy
R2_BUCKET_NAME=wa-ai-bot

WORKER_CHAT_URL=https://worker-chat.yourname.workers.dev
BOT_NAME=My AI Bot
```

### 4. Jalankan bot

```bash
npm start
```

Scan QR code yang muncul di terminal dengan WhatsApp.

## Penggunaan

### Perintah

| Command | Fungsi |
|---------|--------|
| `/setting` | Buka template konfigurasi |
| `/status` | Lihat setting aktif |
| `/reset` | Hapus history chat |
| `/help` | Daftar perintah |

### Setting via /setting

Kirim `/setting`, bot akan membalas template seperti ini:

```
⚙️ SETTING BOT

API_KEY: (api key kamu)
MODEL: gpt-4o-mini
CHARACTER_NAME: Assistant
CHARACTER_DESC: Asisten AI yang helpful
SYSTEM_PROMPT: 
LANGUAGE: id
MAX_HISTORY: 20
REPLY_MODE: text
```

Edit lalu kirim balik — bot otomatis menyimpan perubahan.

## Struktur R2

```
wa-ai-bot/
├── settings/
│   ├── 628123456789.json   # setting per user
│   └── 628987654321.json
└── history/
    ├── 628123456789.json   # chat history per user
    └── 628987654321.json
```

## Struktur Project

```
src/
├── index.js              # Entry point + Baileys setup
├── handlers/
│   ├── message.js        # Router pesan masuk
│   ├── setting.js        # /setting, /status, /reset, /help
│   └── chat.js           # Logic chat ke AI
└── services/
    ├── r2.js             # R2 storage (history & settings)
    └── workerChat.js     # Klien worker-chat (consume SSE)
```

## Tips

- **Group chat**: Dinonaktifkan by default. Edit `message.js` baris `if (isGroup) return;` untuk mengaktifkan.
- **Multi-device**: Auth tersimpan di folder `auth_info/` — backup folder ini.
- **Production**: Gunakan PM2 atau Docker untuk auto-restart.

```bash
# Dengan PM2
npm install -g pm2
pm2 start npm --name "wa-bot" -- start
pm2 save
```
