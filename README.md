# chtting-relay

Relay LLM yang jalan **native di Termux** — bukan Cloudflare Worker, bukan stateless.
Satu proses Node yang menerima panggilan OpenAI-compatible, menerjemahkan nama model,
menyuntik system prompt, mengubah bentuk respons, menghitung token dengan tokenizer
asli, lalu mencatat semuanya ke dashboard lokal.

> *English: a Termux-native, stateful OpenAI-compatible LLM relay with exact token
> counting, model-name translation, prompt injection, response reshaping, a
> localhost dashboard and Cloudflare Tunnel. Everything below applies; the code and
> the dashboard are in English.*

**Nol dependensi npm.** Semuanya pakai Node standard library, jadi tidak ada
`node-gyp`, tidak ada kompilasi native — yang selalu jadi masalah di Termux.

```
     HP kamu (Termux)                                   internet
 ┌───────────────────────────────┐
 │  dashboard  127.0.0.1:8788    │ ← cuma lokal, tidak pernah lewat tunnel
 │  relay API  0.0.0.0:8787      │ ←──── cloudflared ────  https://xxx.trycloudflare.com
 │      │                        │
 │      ├─ terjemah nama model   │
 │      ├─ suntik system prompt  │
 │      ├─ hitung token (exact)  │
 │      ├─ ubah bentuk respons   │
 │      └─ catat metrik → SQLite │
 └──────┼────────────────────────┘
        └────────────────────────────────────────────► backend asli (DeepSeek, dst)
```

---

## Pasang di Termux

```bash
pkg install git
git clone https://github.com/manukmiber/chtting-relay-worker.git
cd chtting-relay-worker
bash scripts/install-termux.sh
```

Script itu memasang Node dan `cloudflared`, mengunduh vocabulary tokenizer yang
kamu pilih, membuat config awal, dan mencetak client key pertama.

Jalankan:

```bash
bash scripts/start-termux.sh        # pakai termux-wake-lock, aman layar mati
```

Buka `http://127.0.0.1:8788` di browser HP. Itu dashboard-nya.

<details>
<summary>Jalan otomatis saat HP nyala / sebagai service</summary>

**Termux:Boot** — pasang app Termux:Boot dari F-Droid, lalu jalankan
`install-termux.sh` sekali lagi; script menaruh launcher di `~/.termux/boot/`.

**termux-services (runit)**

```bash
pkg install termux-services
ln -s ~/chtting-relay-worker/scripts/service $PREFIX/var/service/chtting-relay
sv up chtting-relay
sv status chtting-relay
```
</details>

---

## 1. Tokenizer yang akurat

Ini bagian yang paling menentukan angka biaya kamu benar atau tidak.

Relay ini punya implementasi BPE sendiri, murni JavaScript, dan sudah
**diverifikasi byte-exact** terhadap library rujukan:

| Vocabulary | Rujukan | Hasil |
|---|---|---|
| `cl100k_base`, `o200k_base` | OpenAI `tiktoken` | **token id identik**, bukan cuma jumlahnya |
| DeepSeek, Qwen, Llama-2/3 | HuggingFace `tokenizers` | **jumlah token identik** |
| BERT (WordPiece) | HuggingFace `tokenizers` | persis untuk teks normal |
| T5 (Unigram) | HuggingFace `tokenizers` | persis untuk teks normal¹ |

¹ T5 memakai *precompiled charsmap* sentencepiece yang tidak bisa direplikasi
persis tanpa blob-nya; dianggap NFKC. Untuk teks biasa hasilnya sama, untuk
karakter kontrol langka bisa beda satu-dua token. Model yang benar-benar dipakai
sebagai backend relay (GPT, DeepSeek, Qwen, Llama) semuanya exact.

Yang didukung: tiktoken rank files, dan HuggingFace `tokenizer.json` dalam bentuk
BPE (byte-level maupun metaspace/sentencepiece, termasuk `byte_fallback` dan
penggabungan `<unk>`), Unigram (Viterbi), dan WordPiece.

### Pasang vocabulary

Dari dashboard tab **Tokenizer**, atau:

```bash
npm run tokenizer:fetch cl100k_base o200k_base deepseek qwen
node scripts/fetch-tokenizer.mjs --hf Qwen/Qwen3-8B --as qwen3
HF_TOKEN=hf_xxx node scripts/fetch-tokenizer.mjs llama3     # repo gated
```

Sekali unduh, seterusnya jalan offline. Kalau belum ada vocabulary, relay tetap
jalan memakai estimator sadar-skrip, dan setiap angka diberi tanda
`exact: false` supaya tidak diam-diam dianggap fakta.

### Yang dihitung, bukan cuma teks

Yang dibilling backend bukan cuma isi pesan, tapi juga overhead chat template.
Jadi yang dihitung:

- teks tiap pesan, per-role (system / user / assistant / tool)
- overhead template per pesan — profil `openai`, `chatml`, `llama3`, `deepseek`,
  `mistral`, `gemma`, atau `raw`
- definisi tools, dirender ke bentuk pseudo-TypeScript seperti yang dilakukan OpenAI
- gambar, mengikuti aturan tiling 85 + 170/tile
- reasoning trace dan tool call di sisi output

### Angka mana yang dipakai

Kalau backend mengirim `usage`, itu yang otoritatif — dan relay tetap menyimpan
hitungannya sendiri di sebelahnya sebagai **drift**, supaya kamu bisa lihat
apakah tokenizer kamu sudah cocok dengan yang backend tagih. Kalau backend diam
(banyak gateway tidak mengirim usage saat streaming), hitungan lokal yang
dipakai. Saat streaming, relay otomatis meminta `stream_options.include_usage`.

---

## 2. Terjemahan nama model

Yang dikirim pemanggil ≠ yang dikirim ke backend. Nama asli backend tidak pernah
bocor: tidak di `/v1/models`, tidak di respons, tidak di chunk streaming.

```
POST /v1/chat/completions
{ "model": "manukmiberai/creative-writer", ... }
                  │
                  ▼  route
        backend : ds  (https://api.deepseek.com/v1)
        model   : Deepseek-v4-flash-0731
                  │
                  ▼  respons balik ke pemanggil
{ "model": "manukmiberai/creative-writer", ... }
```

Satu alias bisa punya `aliases` tambahan, dan bisa punya `fallbacks` — daftar
backend cadangan yang dicoba berurutan kalau yang utama mati atau kena limit.

---

## 3. Injeksi system prompt

Tiap alias punya mode:

| Mode | Hasilnya |
|---|---|
| `none` | biarkan apa adanya |
| `prepend` | prompt kamu duluan, lalu system message pemanggil |
| `append` | punya pemanggil duluan, prompt kamu sesudahnya |
| `replace` | cuma punya kamu; system message pemanggil dibuang |
| `merge` | satu system message, punya kamu di atas |

Teksnya bisa ditulis inline di alias, atau diambil dari **library prompt** (tab
Prompts) supaya satu persona dipakai beberapa alias sekaligus.

---

## 4. Mengubah bentuk respons

Berlaku sama untuk respons utuh maupun streaming — jadi klien streaming dan
non-streaming menerima bentuk yang identik.

| Pengaturan | Fungsinya |
|---|---|
| `renameModel` | kembalikan alias publik ke field `model` |
| `reasoning` | `keep` / `strip` / `inline` (bungkus `<think>…</think>`) / `field` |
| `stripFields` | hapus field top-level, misal `system_fingerprint` |
| `setFields` | tambah field top-level sendiri |
| `prefix`, `suffix` | tempel teks di depan/belakang jawaban |
| `replace` | aturan regex atas teks jawaban |

Penulisan ulang saat streaming tidak akan kebobolan pola yang terbelah antar
chunk: titik potong ditarik mundur ke belakang match yang melintasi batas, jadi
`"Deep"` + `"Seek"` di dua chunk berbeda tetap tertangkap.

Bentuk transport juga bisa diubah:

- backend tidak bisa streaming, klien minta streaming → jawaban utuh diputar ulang jadi SSE
- klien minta jawaban utuh, tapi `forceStream: true` → relay streaming ke backend
  (supaya TTFT dan tokens/detik tetap terukur) lalu menyatukannya kembali

Sisi request juga bisa dibentuk: `dropParams` untuk backend yang rewel,
`renameParams` (misal `max_completion_tokens` → `max_tokens`), `injectStop`,
`params` sebagai default dan `forceParams` yang tidak bisa ditawar pemanggil.

---

## 5. Logging dan metrik

Satu baris per panggilan, masuk SQLite (`node:sqlite`, bawaan Node 22 — tanpa
kompilasi). Node lama otomatis jatuh ke JSONL append-only.

Yang dicatat: token input dan output, token cached dan reasoning, **TTFT**,
jendela generasi, **tokens/detik**, total latensi, status, alasan berhenti,
retry, alias dan model backend, client key, IP, user agent, sumber angka usage,
drift lokal vs backend, dan preview prompt/jawaban.

Yang diringkas dashboard: request dan **pengguna harian**, token masuk/keluar per
hari, **rata-rata TTFT** dan p50/p95, **rata-rata TPS** dan p50/p95, error rate,
serta rincian per model dan per key.

Definisi waktunya:

```
   kirim                token pertama                token terakhir
     │◄──── ttft_ms ────►│◄──────── gen_ms ──────────►│
     │◄───────────────── total_ms ───────────────────►│

   tokens_per_sec = completion_tokens / gen_ms
```

TTFT hanya dicatat kalau memang ada streaming — request non-streaming tidak
punya TTFT, dan rata-ratanya tidak dikotori angka palsu.

---

## 6. Dashboard

Di `http://127.0.0.1:8788`, terikat ke localhost dan **tidak pernah dilewatkan
tunnel**. Vanilla JS, tanpa build step, tanpa CDN — jadi tetap terbuka meski HP
sedang offline.

| Tab | Isinya |
|---|---|
| Overview | statistik, grafik harian, rincian per model dan per key |
| Models | editor alias: terjemahan nama, prompt, params, limit, tokenizer, reshaping |
| Backends | provider upstream + tombol tes koneksi |
| Prompts | library system prompt |
| Keys | client key, kuota, batasan model |
| Requests | log per panggilan + rincian timing dan token |
| Tokenizer | playground token, biaya satu request chat, pasang vocabulary |
| Playground | kirim request beneran lewat relay |
| Tunnel | start/stop cloudflared, URL publik, output mentah |
| Settings | server, security, logging, aturan tokenizer, default |
| Logs | ekor `relay.log` |

Beri password lewat Settings kalau HP-mu dipakai orang lain. Secret selalu
tampil termask, dan menyimpan form tidak akan menimpa key asli dengan masknya.

---

## 7. Cloudflare Tunnel

Tab **Tunnel**, atau `node src/cli.js tunnel`.

- **quick** — URL `*.trycloudflare.com` gratis, tanpa akun Cloudflare
- **named** — hostname sendiri, pakai tunnel token dari Zero Trust
- autoStart menyalakan tunnel bersama relay dan menyambungkannya lagi kalau putus
  (jaringan seluler memang sering putus)

Yang dipublikasikan hanya port relay. Dashboard tetap di localhost.

---

## Memakainya

Sama seperti OpenAI API:

```bash
curl https://xxx.trycloudflare.com/v1/chat/completions \
  -H "Authorization: Bearer sk-relay-..." \
  -H "Content-Type: application/json" \
  -d '{
    "model": "manukmiberai/creative-writer",
    "messages": [{"role":"user","content":"Halo"}],
    "stream": true
  }'
```

Endpoint: `GET /health`, `GET /v1/models`, `GET /v1/models/:id`,
`POST /v1/chat/completions`, `POST /v1/completions`, `POST /v1/embeddings`.

---

## CLI

```
chtting start                 jalankan relay + dashboard
chtting doctor                periksa lingkungan dan konfigurasi
chtting config path|show
chtting key new [label]       buat client key, ditampilkan sekali
chtting key list
chtting backend add --id ds --url https://api.deepseek.com/v1 --key sk-...
chtting model add --id manukmiberai/creative-writer --backend ds \
                  --upstream deepseek-chat --tokenizer deepseek
chtting tokenizer list
chtting tunnel                jalankan tunnel saja
```

Opsi: `--port`, `--dashboard-port`, `--no-dashboard`, `--config <file>`.

---

## Layout

```
src/
  tokenizer/    BPE core, loader tiktoken & HuggingFace, pre-tokenizer,
                penghitungan chat, estimator, registry
  relay/        routing, upstream + fallback, transform, SSE, handler + metrik
  server/       API publik, dashboard + admin API
  store/        SQLite dan JSONL
  tunnel/       supervisor cloudflared
public/         dashboard (vanilla JS, tanpa build)
scripts/        setup Termux, service runit, pengunduh tokenizer
tests/          52 test: tokenizer, transform, relay end-to-end
```

Data ada di `data/` (database, log, vocabulary), config di `config/config.json`.
Keduanya di-gitignore. `CHTTING_HOME`, `CHTTING_CONFIG`, `CHTTING_DATA` dan
`CHTTING_TOKENIZER_DIR` bisa memindahkannya.

## Test

```bash
npm test
```

Test tokenizer memakai fixture hasil `tiktoken` dan `tokenizers` asli. Test yang
butuh vocabulary akan di-skip, bukan gagal, kalau vocabulary-nya belum dipasang.

## Catatan keamanan

- API key backend tidak pernah keluar dari HP; pemanggil hanya memegang client key relay.
- Dashboard default terikat `127.0.0.1` dan tidak masuk tunnel. Kalau kamu ubah
  bindingnya, pasang password.
- Prompt hanya disimpan lokal. Kalau tidak mau disimpan sama sekali, set
  `logging.storeBodies` ke `none`.
- Client key ditampilkan penuh sekali saat dibuat, sesudah itu selalu termask.

## Lisensi

MIT
