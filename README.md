# chtting-relay

Relay LLM yang jalan **native di Termux** — bukan Cloudflare Worker, bukan stateless.
Satu proses **Rust** yang menerima panggilan OpenAI-compatible, menerjemahkan nama
model, menyuntik system prompt, mengubah bentuk respons, menghitung token dengan
tokenizer asli, lalu mencatat semuanya ke dashboard lokal.

> *English: a Termux-native, stateful OpenAI-compatible LLM relay in Rust, with
> exact token counting, model-name translation, prompt injection, response
> reshaping, a localhost dashboard and Cloudflare Tunnel. Everything below
> applies; the code and the dashboard are in English.*

**Dibangun untuk ratusan pengguna konkuren.** Runtime Tokio multi-thread memakai
semua core HP, config dibaca lock-free, metrik ditulis satu writer di belakang
channel, dan kuota harian dijawab dari memori — bukan query SQLite per request.

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
git clone https://github.com/manukmiber/Chtting-relay-worker
cd Chtting-relay-worker
bash scripts/install-termux.sh
```

Script itu memasang toolchain Rust dan `cloudflared`, membangun binary,
menawarkan unduh vocabulary tokenizer, membuat config awal, dan mencetak client
key pertama.

> **Build pertama lama** — 5 sampai 15 menit di HP, sekali saja. Yang dipasang
> cuma `rust` dan `clang`; tidak ada cmake, tidak ada Go, tidak ada Node.
> Kalau core-nya sedikit script otomatis pakai `-j1` supaya tidak kehabisan RAM.

Jalankan:

```bash
bash scripts/start-termux.sh        # pakai termux-wake-lock, aman layar mati
```

Buka `http://127.0.0.1:8788` di browser HP. Itu dashboard-nya.

<details>
<summary>Jalan otomatis sebagai service</summary>

```bash
pkg install termux-services
ln -s ~/Chtting-relay-worker/scripts/service $PREFIX/var/service/chtting-relay
sv up chtting-relay
sv status chtting-relay
```
</details>

---

## Kenapa Rust

Versi sebelumnya jalan di Node. Untuk ratusan pengguna konkuren di HP, ini yang
berubah:

| | Node (v1) | Rust (v2) |
|---|---|---|
| Paralelisme | satu event loop, satu core | Tokio multi-thread, semua core |
| Baca config per request | clone objek | `ArcSwap` — baca atomik, tanpa lock |
| Cek kuota harian | satu `SELECT` SQLite per request | counter di memori, di-seed saat start |
| Tulis metrik | sinkron di jalur request | channel → satu writer, batch per transaksi |
| Tokenisasi | JS, blocking event loop | pool blocking, tidak menahan stream orang lain |
| Kelebihan beban | antre sampai HP megap | semaphore + **503 langsung** |
| Koneksi ke backend | handshake per request | connection pool reqwest, TLS tetap hangat |
| Tokenizer | implementasi BPE sendiri | library rujukan aslinya |

Yang dipasang di HP juga menyusut: satu binary statis, tanpa runtime Node,
tanpa `node_modules`.

---

## 1. Tokenizer yang akurat

Ini bagian yang paling menentukan angka biaya kamu benar atau tidak.

Versi Rust tidak memakai implementasi BPE sendiri — ia memakai **implementasi
rujukannya langsung**:

| Vocabulary | Dipakai | Hasil |
|---|---|---|
| `cl100k_base`, `o200k_base`, `p50k`, `r50k` | `tiktoken-rs` (rank file OpenAI, ikut ter-compile ke binary) | token id identik dengan `tiktoken` Python |
| DeepSeek, Qwen, Llama, Mistral, Gemma, GLM | crate `tokenizers` milik HuggingFace | identik — ini kode yang sama yang dipakai package Python-nya |
| BERT (WordPiece), T5 (Unigram) | crate `tokenizers` | identik, termasuk *precompiled charsmap* |

Bedanya dengan versi Node: dulu BPE, byte-fallback, dan pre-tokenizer ditulis
ulang dengan tangan, dan T5/BERT hanya mendekati. Sekarang semuanya exact karena
yang jalan memang library aslinya.

Diverifikasi lewat test terhadap fixture yang dibuat pakai `tiktoken` dan
`tokenizers` Python asli — jalankan `cargo test`.

**Vocabulary OpenAI tidak perlu diunduh sama sekali**; ikut di dalam binary.

### Pasang vocabulary model terbuka

Dari dashboard tab **Tokenizer**, atau:

```bash
chtting-relay tokenizer list
chtting-relay tokenizer install deepseek
chtting-relay tokenizer install Qwen/Qwen3-8B --as qwen3
```

Sekali unduh, seterusnya jalan offline. File diperiksa dulu sebelum disimpan,
jadi unduhan gagal tidak akan menyamar jadi vocabulary terpasang.

Kalau vocabulary belum ada, relay tetap jalan memakai estimator sadar-skrip, dan
setiap angka diberi tanda `exact: false` supaya tidak diam-diam dianggap fakta.

### Yang dihitung, bukan cuma teks

Yang dibilling backend bukan cuma isi pesan, tapi juga overhead chat template:

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

Aturan rewrite request tidak pernah menyentuh system prompt kamu sendiri.

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

Satu baris per panggilan, masuk SQLite (WAL, di-*bundle* jadi tidak bergantung
versi sqlite Termux). Handler tidak pernah menunggu disk: baris dikirim lewat
channel ke satu writer yang menulisnya per-batch dalam satu transaksi.

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
tunnel**. Vanilla JS, tanpa build step, tanpa CDN — dan sekarang **ikut
ter-compile ke dalam binary**, jadi relay bisa dijalankan dari direktori mana pun.

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

Tab **Tunnel**, atau setel `tunnel.autoStart` di config.

- **quick** — URL `*.trycloudflare.com` gratis, tanpa akun Cloudflare
- **named** — hostname sendiri, pakai tunnel token dari Zero Trust
- autoStart menyalakan tunnel bersama relay dan menyambungkannya lagi kalau putus,
  dengan backoff (jaringan seluler memang sering putus)

Yang dipublikasikan hanya port relay. Dashboard tetap di localhost. Tunnel token
tidak pernah ikut tertulis ke buffer log yang ditampilkan dashboard.

---

## Setelan untuk banyak pengguna

Di `config.json` bagian `server`:

| Kunci | Default | Artinya |
|---|---|---|
| `maxConcurrentRequests` | `512` | batas panggilan yang sedang berjalan ke backend. Lewat itu relay menjawab **503 + `Retry-After`** langsung, bukan mengantre sampai HP tidak sanggup |
| `workerThreads` | `0` | jumlah thread Tokio; `0` berarti satu per core. Turunkan kalau mau menyisakan tenaga buat aplikasi lain |
| `keepAliveTimeoutMs` | `75000` | keep-alive koneksi masuk |

Kuota per key (`keys[].quota`) dihitung di memori: `requestsPerMinute` pakai
sliding window ter-*shard*, `requestsPerDay` dan `tokensPerDay` pakai counter
harian yang di-seed dari database saat start dan berputar sendiri di tengah malam
zona waktumu.

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
`POST /v1/chat/completions`, `POST /chat/completions`, `POST /v1/completions`,
`POST /v1/embeddings`.

---

## CLI

```
chtting-relay start [--port N] [--no-dashboard]
chtting-relay doctor                    periksa lingkungan dan konfigurasi
chtting-relay config path|show
chtting-relay key new --label "hp saya" client key baru, ditampilkan sekali
chtting-relay key list
chtting-relay backend add --name deepseek \
      --base-url https://api.deepseek.com/v1 --api-key sk-...
chtting-relay backend list
chtting-relay model add --id manukmiberai/creative-writer \
      --backend <id> --upstream Deepseek-v4-flash-0731
chtting-relay model list
chtting-relay tokenizer list
chtting-relay tokenizer install deepseek
```

`--home <dir>` memindahkan seluruh state.

---

## Layout

```
src/
  config.rs      skema config, simpan atomik, validasi, masking
  state.rs       yang dibagi semua handler
  logging.rs     log berlevel, file writer di background
  tokenizer/     registry vocabulary, penghitungan chat, estimator
  relay/         upstream + fallback, transform, SSE, handler + metrik
  server/        API publik, dashboard + admin API
  store/         SQLite, quota tracker, rate limiter
  tunnel.rs      supervisor cloudflared
public/          dashboard (vanilla JS, ikut ter-compile ke binary)
scripts/         setup Termux, service runit
tests/           test end-to-end lewat HTTP asli
```

Data ada di `data/` (database, log, vocabulary), config di `config/config.json`.
Keduanya di-gitignore. `CHTTING_HOME`, `CHTTING_CONFIG`, `CHTTING_DATA` dan
`CHTTING_TOKENIZER_DIR` bisa memindahkannya.

## Test

```bash
cargo test
```

Test tokenizer membandingkan dengan fixture hasil `tiktoken` dan `tokenizers`
Python asli. Test yang butuh vocabulary HuggingFace akan di-skip, bukan gagal,
kalau vocabulary-nya belum dipasang. Test relay dan dashboard menjalankan server
sungguhan lewat HTTP di depan backend tiruan — termasuk 300 pemanggil serentak
dan pengujian bahwa kelebihan beban dijawab 503, bukan diantre.

## Catatan keamanan

- API key backend tidak pernah keluar dari HP; pemanggil hanya memegang client key relay.
- Dashboard default terikat `127.0.0.1` dan tidak masuk tunnel. Kalau kamu ubah
  bindingnya, pasang password.
- Perbandingan client key memakai constant-time compare.
- Prompt hanya disimpan lokal. Kalau tidak mau disimpan sama sekali, set
  `logging.storeBodies` ke `none`.
- Client key ditampilkan penuh sekali saat dibuat, sesudah itu selalu termask.

## Lisensi

MIT
