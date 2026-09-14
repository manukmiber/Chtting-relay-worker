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
 │      ├─ pilih system prompt   │  ← sesuai thinking effort si pemanggil
 │      ├─ hitung token (exact)  │
 │      ├─ bangun ulang respons  │  ← amplop kita, bukan amplop backend
 │      ├─ rem TPS keluar        │  ← biar tunnel tidak berat
 │      ├─ hitung harga bertier  │  ← backend / proxy / profit
 │      └─ catat metrik → SQLite │
 └──────┼────────────────────────┘
        └────────────────────────────────────────────► backend asli (DeepSeek, dst)
```

Yang keluar dari relay ini tidak menyisakan jejak backend sama sekali:

```jsonc
// dari backend                          // ke pemanggil
{ "id": "3251459c-90b4-…",               { "id": "32dadfde-f3dd-4ad8-…",
  "model": "deepseek-flash",               "model": "Wissangeni-512B-V1",
  "system_fingerprint": "aeb564…",         "object": "chat.completion.chunk",
  "created": 1789263746,                   "created": 1789285301,
  "choices": [{ "index": 0,                "choices": [{ "index": 0,
    "delta": { "content": "api",             "delta": { "content": "api" },
               "reasoning_content": null },  "finish_reason": null }] }
    "logprobs": null,
    "finish_reason": null }] }
```

---

## Pasang di Termux

```bash
pkg install git
git clone https://github.com/manukmiber/Chtting-relay-worker
cd Chtting-relay-worker
bash scripts/install-termux.sh
```

Tiga baris itu saja. Script-nya **mengunduh binary siap pakai** untuk CPU HP-mu
kalau rilisnya ada (checksum diverifikasi), membuat config awal, mencetak client
key pertama, memasang keeper + shortcut layar utama + hook boot, lalu
menjalankan relay-nya.

Kalau belum ada rilis yang cocok, script otomatis build dari source:

> **Build dari source lama** — 5 sampai 15 menit di HP, sekali saja. Yang
> dipasang cuma `rust` dan `clang`; tidak ada cmake, tidak ada Go, tidak ada
> Node. Kalau core-nya sedikit script otomatis pakai `-j1` supaya tidak
> kehabisan RAM. Mau memaksa build sendiri: `bash scripts/install-termux.sh --build`.

Binary rilis dibuat untuk **arm64** (hampir semua HP sejak ~2016) dan **armv7**
(HP 32-bit). Keduanya binary Android asli, bukan emulasi.

Setelah itu buka **`http://127.0.0.1:8788`** di browser HP. Tab **Setup** yang
mengurus sisanya — backend, model, tokenizer, tunnel, start/stop/restart,
wake lock, service, semuanya tombol. **Tidak ada lagi yang perlu diketik di
Termux.**

### Menjalankan tanpa membuka Termux

Tab **Setup** menulis tiga hal untuk kamu:

| | Gunanya |
|---|---|
| **Keeper** | relay dihidupkan lagi kalau mati — loop `sh` kecil, tidak butuh paket apa pun |
| **Shortcut layar utama** | Start / Stop / Restart / Buka dashboard jadi ikon — butuh app **Termux:Widget** dari F-Droid |
| **Hook boot** | relay nyala sebelum HP di-unlock — butuh app **Termux:Boot** dari F-Droid |

> **`termux-services` sudah tidak ada** di repo Termux, jadi service runit yang
> dulu dipakai sekarang tidak mengawasi apa pun — cuma kelihatan terpasang.
> Penggantinya **keeper**: satu skrip `sh` di `~/.chtting-relay/keeper.sh` yang
> menjalankan relay dalam loop dan menghidupkannya lagi tiga detik setelah mati.
> Tidak ada `pkg install` tambahan; `sh` selalu ada di Termux.

Satu hal yang **tidak bisa** dipindah ke dashboard: menyalakan relay yang sedang
mati. Dashboard-nya kan disajikan oleh relay itu sendiri. Untuk itulah shortcut
dan hook boot ada — menyalakannya tetap tanpa mengetik, cukup satu tap.

> Restart dari dashboard itu `exec` ke diri sendiri: PID sama, port sama,
> config dibaca ulang, halaman balik lagi dalam sedetik. Jadi mengubah port
> relay atau bind address cukup Save lalu **Restart relay**.

### Ganti diri sendiri tiap jam

Android membunuh proses yang paling lama nongkrong di memori — biasanya tengah
malam, biasanya tidak ada yang sadar sampai pagi. Jadi relay-nya **pensiun
duluan sebelum dibunuh**: tiap jam ia menjalankan salinan baru dirinya sendiri,
salinan itu bind ke port yang sama (`SO_REUSEPORT`), baru yang lama berhenti
menerima koneksi dan menyelesaikan request yang masih jalan.

```
  lama │██████████ melayani ██████████│ meniriskan │ selesai
  baru                │ bind │████████████ melayani ████████████ …
```

Yang bikin serah terima ini mulus juga bikin satu jebakan: karena
`SO_REUSEPORT`, **bind ke port yang sudah dipakai itu berhasil, bukan gagal**.
Dua relay yang dijalankan sendiri-sendiri akan sama-sama mendengarkan di port
yang sama, dan kernel membagi koneksi baru ke salah satunya secara acak. Yang
lama menjawab pakai config yang ia pegang sejak start — jadi key yang baru
dibuat lewat dashboard tidak dikenal olehnya, dan pemanggil melihat
`invalid API key` di sebagian request dan sukses di sebagian lain.

Makanya `start` menulis `data/run/serving.pid` dan menolak jalan kalau pid di
situ masih hidup dan masih relay ini. Rotasi dikecualikan — successor-nya punya
nomor generasi, jadi tumpang tindihnya memang disengaja. Yang dijalankan
manual tidak punya itu dan harus bilang `--replace` kalau memang mau
mengambil alih port-nya.

Tidak ada request yang putus dan tidak ada koneksi yang ditolak, karena selalu
ada yang mendengarkan. Kalau salinan barunya gagal naik, yang lama jalan terus
seperti biasa dan mencoba lagi jam berikutnya. Atur di **Settings → Staying
alive**, atau `server.rotateHours` (`0` untuk mematikan).

---

## Build sendiri dan rilis

Di PC (tidak perlu HP), silakan cross-compile untuk Android:

```bash
# sekali saja: unduh Android NDK
curl -LO https://dl.google.com/android/repository/android-ndk-r27c-linux.zip
unzip -q android-ndk-r27c-linux.zip
export ANDROID_NDK_HOME="$PWD/android-ndk-r27c"

bash scripts/build-android.sh              # arm64
bash scripts/build-android.sh aarch64 armv7  # dua-duanya
```

Hasilnya di `target/<target>/release/chtting-relay` — tinggal salin ke HP,
`chmod +x`, jalankan. Tidak perlu toolchain Rust di HP sama sekali.

Kalau build langsung di HP dan RAM-nya pas-pasan, installer otomatis pakai
profil `release-small` (satu codegen unit, tanpa LTO, dioptimasi ukuran) supaya
linker-nya tidak kena OOM killer di tengah jalan. Mau memaksa:

```bash
cargo build --profile release-small -j1
```

Dependensinya sengaja dijaga supaya **tidak ada cmake, Go, Node, atau compiler
C++**: TLS-nya `ring` bukan aws-lc, tokenizer-nya dibangun tanpa backend C++
`esaxx`, regex-nya `fancy-regex` yang murni Rust. Yang perlu di Termux cuma
`rust` dan `clang`.

Untuk merilis: `git tag v2.1.0 && git push origin v2.1.0`. GitHub Actions
membangun kedua arsitektur, membuat checksum, dan menempelkannya ke Release.

CI (`cargo test`, clippy, rustfmt, plus cross-compile Android) jalan di tiap
push dan PR. Semuanya cuma perintah cargo biasa — **kalau nanti isinya diganti
framework lain, CI dan rilisnya tetap jalan tanpa diubah.**

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

Yang dipasang di HP juga menyusut: satu file binary (dashboard dan vocabulary
OpenAI ikut di dalamnya), tanpa runtime Node, tanpa `node_modules`.

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

### Menghitung ulang yang sudah pernah dihitung

Klien chat mengirim ulang seluruh transkrip tiap giliran: giliran ke-N membawa
semua pesan giliran ke-(N-1) persis byte per byte, plus satu pesan baru.
Encoding adalah bagian mahal dari menghitung — puluhan milidetik begitu satu
scene roleplay memanjang — dan hampir semuanya adalah pekerjaan yang sudah
pernah dikerjakan proses ini.

Jadi tiap vocabulary mengingat jumlah token per potongan teks, dikunci dengan
SHA-256. Hash-nya sengaja kriptografis, bukan hash cepat: nilainya menentukan
tagihan, jadi tabrakan bukan cuma jalur lambat melainkan invoice yang salah —
dan hashing tetap sekitar tiga orde lebih murah daripada BPE-nya sendiri.

Batasnya per generasi, bukan LRU (yang butuh write lock tiap *baca*): begitu
map panas penuh ia jadi map dingin dan map baru menggantikan. Memori berhenti
di dua generasi, dan percakapan yang masih hidup naik lagi ke map panas pada
lookup berikutnya. Ingatannya ikut mati bersama encoder-nya, jadi vocabulary
yang dimuat ulang tidak pernah mewarisi hitungan vocabulary lama.

Diukur pada transkrip yang tumbuh satu pertukaran per giliran (cl100k, x86):

| pesan | token | tanpa ingatan | dengan ingatan |
|------:|------:|--------------:|---------------:|
| 41 | 3 328 | 2,75 ms | 0,20 ms |
| 121 | 9 128 | 6,66 ms | 0,39 ms |
| 201 | 14 928 | 10,30 ms | 0,70 ms |
| 281 | 20 728 | 14,63 ms | 0,76 ms |

Angkanya sendiri tergantung mesin dan vocabulary; yang bisa dipegang adalah
bentuknya — biaya per giliran berhenti tumbuh mengikuti panjang percakapan dan
mengikuti panjang pesan baru saja. Jalankan sendiri dengan
`cargo test --release -- --ignored --nocapture growing_roleplay`.

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

Tiap model juga punya pemiliknya sendiri, yang terbit sebagai `owned_by` di
`/v1/models`. Dikosongkan, isinya **ZeikoAI** — model yang dilayani di sini
memang model rumah sendiri, mesin siapa pun yang menjawabnya.

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

### Dua prompt: Default dan No thinking

Panggilan tanpa reasoning dan panggilan dengan effort maksimum butuh instruksi
yang berbeda: yang pertama perlu jawabannya dibentuk langsung, yang kedua perlu
ruang untuk berpikir. Tapi di praktiknya pembacanya cuma dua — yang minta model
berpikir, dan yang tidak. Jadi di dashboard isinya dua kotak, bukan editor
aturan:

* **Default** — `systemPrompt` model itu sendiri. Dipakai untuk pemanggil yang
  minta model berpikir (`low`, `medium`, `high`, `max`), dan untuk semua orang
  selama kotak kedua masih di mode `none`.
* **No thinking** — satu aturan `systemPrompts[]` dengan id khusus
  `sp-non-thinking` dan `efforts: ["none", "minimal", "default"]`.

```json
"systemPrompts": [
  { "id": "sp-non-thinking", "name": "No thinking", "enabled": true,
    "efforts": ["none", "minimal", "default"],
    "prompt": { "mode": "replace", "text": "Jawab langsung, tanpa basa-basi." } }
]
```

Tiga effort itu persis yang ditanggung **band harga no-thinking**, dan memang
harus tetap begitu: diam bukan pilihan untuk berpikir, jadi jangan dijawab
seperti itu dan jangan ditagih seperti itu. Request yang dibilang satu hal lalu
ditagih hal lain adalah satu-satunya bug yang tidak kelihatan dari layar mana
pun. Ada test yang mengunci ini.

Kotak yang dibiarkan di mode `none` tidak menulis aturan sama sekali — jadi
pemanggilnya jatuh ke Default, yang memang arti dari "belum saya isi".

Di bawah dua kotak itu `systemPrompts` tetap daftar aturan biasa buat hal yang
lebih sempit: aturan pertama yang cocok yang menang, dan aturan tulisanmu
sendiri ditaruh **sebelum** aturan No thinking. Effort-nya dibaca dari body
pemanggil apa pun ejaannya — `reasoning_effort` ala OpenAI, `reasoning.effort`
ala OpenRouter, `enable_thinking` ala Qwen, atau `thinking.budget_tokens` ala
Anthropic yang dipetakan ke level menurut besarnya.

---

## 4. Mengubah bentuk respons

**Dibangun ulang, bukan disaring.** Relay tidak menyaring JSON backend — ia
memulai amplop sendiri lalu menyalin segelintir field yang disebut namanya:

```json
{ "id": "<uuid v4 kita>", "object": "chat.completion.chunk",
  "created": <saat request masuk>, "model": "<alias publik>",
  "choices": [{ "index": 0, "delta": { "content": "…" }, "finish_reason": null }] }
```

Menyaring dengan cara menghapus itu arahnya terbalik: yang hilang cuma yang
sempat kita sebut, jadi begitu backend menambah field baru besok, field itu ikut
terkirim. Dengan dibangun ulang, `system_fingerprint`, id request backend,
`created`-nya, `service_tier`, `logprobs`, dan apa pun yang belum ada hari ini
tidak perlu dihapus — memang tidak pernah disalin.

Blok `usage` juga tidak ikut. Yang dikirim relay adalah miliknya sendiri:
`prompt_tokens` hasil hitungan tokenizer relay atas body pemanggil, bukan angka
backend, plus harga request itu:

```json
"usage": { "prompt_tokens": 1284, "completion_tokens": 909, "total_tokens": 2193,
           "completion_tokens_details": { "reasoning_tokens": 629 },
           "usage": 0.001291238, "cost": 0.001291238 }
```

Harganya sembilan angka di belakang koma, bukan enam: request pendek di harga
sepersepuluh dolar per sejuta token cuma beberapa per sejuta sen, dan dibulatkan
ke enam angka semua request kecil terbaca gratis. `cost` itu angka yang sama
dalam ejaan OpenRouter — satu harga, dua nama, bukan dua harga.

Kalau daftar harga relay sendiri dimatikan, harga yang dipakai adalah harga
per-token yang **diumumkan** model itu di `/v1/models`, jendela jam dan harinya
ikut. Jadi pemanggil bisa mengalikan sendiri tarif yang mereka baca dengan
jumlah token yang mereka terima dan sampai di angka yang sama. Yang tidak punya
harga sama sekali tetap tidak melaporkan uang — bukan nol yang terbaca gratis.

Selebihnya berlaku sama untuk respons utuh maupun streaming — jadi klien
streaming dan non-streaming menerima bentuk yang identik.

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

### Kegagalan backend juga dibentuk ulang

Error adalah respons yang paling sering di-copy-paste orang ke tempat lain, jadi
di situlah kebocoran paling mahal. Dulu kalimat backend diteruskan apa adanya —
lengkap dengan nama backend, host-nya saat koneksi gagal, dan kata-katanya
sendiri. Sekarang statusnya dipetakan per kelas dan kalimatnya milik relay:

| Status backend | Yang diterima pemanggil | Alasannya |
|---|---|---|
| 400, 422 | 400 `invalid_request` | request-nya sendiri yang ditolak, pemanggil bisa memperbaiki |
| 413 | 413 `too_large` | body kebesaran |
| 408, 504 | 504 `timeout` | model kelamaan menjawab |
| 429 | 429 `rate_limit_exceeded` | model sedang sibuk |
| 401, 402, 403, 404, 5xx | 502 `upstream_unavailable` | itu kredensial/urusan kita, bukan key pemanggil |

Yang terakhir itu yang paling penting: 401 dari backend berarti **key kita**
yang ditolak. Meneruskannya sebagai 401 akan memberi tahu pemanggil bahwa key
*mereka* yang salah — keliru, dan tidak ada yang bisa mereka lakukan.

Jawaban asli backend tetap tercatat utuh di baris request dan di `relay.log`,
tempat operator memang membutuhkannya dan tidak ada orang luar yang bisa
membacanya.

### Keep-alive dengan kalimat sendiri

Selama backend berpikir, tidak ada apa pun yang lewat kabel, dan cloudflared
atau NAT operator akan menganggap koneksinya mati. Relay mengisi diam itu dengan
komentar SSE miliknya sendiri:

```
: Zeiko is still here, Just be patience
```

Komentar bukan event, jadi tidak ada klien yang mem-parse-nya — dia cuma
lalu-lintas, dan itu memang seluruh tujuannya. Keep-alive milik backend di-parse
lalu dibuang, tidak diteruskan: bentuknya saja sudah jadi sidik jari backend
mana yang ada di belakang.

### Menahan kecepatan keluar

Backend yang jalan di 170 token/detik mendorong 170 token/detik ke dalam tunnel,
dan di uplink HP itulah bebannya terasa. `maxTokensPerSecond` per model menahan
laju keluar — misalnya ke 35 — tanpa ada yang merasa lambat (tetap jauh lebih
cepat dari kecepatan orang membaca), dan menyisakan napas buat request lain.

Caranya dengan tidak membaca dari backend lebih cepat daripada menulis ke
pemanggil, jadi jedanya merambat balik lewat TCP window, bukan menumpuk token di
memori. Delta pertama tidak pernah ditahan, supaya TTFT tetap angka backend dan
bukan angka bikinan remnya.

---

## 5. Daftar harga

Tiap request punya tiga angka: **backend** (yang ditagih ke kita), **proxy**
(yang kita tagih), dan **profit** (selisihnya). Tarifnya per juta token, dan
tarif jual yang dibiarkan `0` diturunkan dari tarif backend plus margin.

Sisi jualnya bukan satu baris angka, tapi **kartu tarif tiga pita** — karena
begitulah model-model ini memang dijual: satu tarif baku, satu lebih mahal saat
pemanggil minta thinking maksimum, satu lebih murah saat thinking dimatikan.
Pitanya dipilih dari thinking effort yang diminta pemanggil, dan hanya itu:

| Pita | Dipilih oleh | Ditentukan lewat |
|---|---|---|
| baku | `low`, `medium`, `high` | `inputUsdPerM`, `cachedInputUsdPerM`, `outputUsdPerM` |
| max thinking | `max` (dan budget thinking di atas 32K) | `maxThinking` |
| tanpa thinking | `none`, `minimal`, **dan pemanggil yang tidak menyebut apa pun** | `nonThinking` |

```json
"inputUsdPerM": 0.35,
"cachedInputUsdPerM": 0.10,
"outputUsdPerM": 1.5,
"maxThinking":  { "inputUsdPerM": 0.35, "cachedInputUsdPerM": 0.10, "outputUsdPerM": 2.0 },
"nonThinking":  { "inputUsdPerM": 0.35, "cachedInputUsdPerM": 0.10, "outputUsdPerM": 1.2 }
```

Tarif yang dibiarkan `0` di sebuah pita menagih tarif baku, bukan menagih nol —
jadi pita yang cuma menggeser output cukup menyebut satu angka. Token reasoning
itu token output, dengan tarif output pita yang sedang berlaku.

Diam ditagih sebagai tanpa thinking dengan sengaja: pemanggil yang tidak pernah
menyebut thinking tidak memilih membelinya, jadi tidak membayarnya.

### Daftar harga ZeikoAI

Tarif jual yang dipasang di `config/config.example.json`, USD per juta token:

| Model | Input | Cache read | Output | Max thinking | Tanpa thinking |
|---|---|---|---|---|---|
| `Jagad-512B-V1` — SFW | $0,35 | $0,10 | $1,50 | $2,00 | $1,20 |
| `Asmarandana-512B-V1` — NSFW | $0,50 | $0,10 | $2,00 | $2,65 | $1,60 |
| `Wissangeni-512B-V1` — uncensored | $0,80 | $0,20 | $4,00 | $6,00 | $3,50 |

Tiga kolom terakhir itu ketiga pitanya. Tarif input dan cache read di sini tidak
berubah antar pita, tapi tiap pita tetap menyimpannya sendiri — kalau suatu saat
salah satunya beda, cukup diisi, tanpa perlu aturan tambahan.

### Tier: syarat yang tidak muat di kartu tarif

Jam, ukuran prompt, tarif akhir pekan. `tiers` itu daftar, berlaku **di atas**
pita mana pun yang sedang dipakai, dan **semua tier yang cocok ikut berlaku**,
berurutan. Prompt 300 ribu token di jam padat membayar keduanya — bukan relay
yang harus memilih satu alasan untuk menaikkan harga.

```json
"tiers": [
  { "name": "jam padat",     "inputMultiplier": 1.25, "outputMultiplier": 1.25,
    "when": { "hours": [{ "from": 19, "to": 23 }] } },
  { "name": "di atas 256K",  "inputMultiplier": 2,
    "when": { "minInputTokens": 256000 } },
  { "name": "akhir pekan",   "inputUsdPerM": 0.14,
    "when": { "weekdays": [5, 6] }, "stop": true }
]
```

Thinking effort tidak lagi ditulis di sini — itu kartu tarifnya. Pitanya
ditetapkan sebelum tier mana pun dibaca, jadi tidak ada tier ber-`stop` yang
bisa memotong rantai lebih dulu dan membuat request effort maksimum membayar
tarif baku. Pita yang berlaku tercatat di baris request bersama tier, sebagai
`max thinking` atau `no thinking`.

Syarat yang bisa dipakai di `when`: model (glob), thinking effort (daftar atau
batas `minEffort`/`maxEffort`), jam (melingkar lewat tengah malam), hari,
jumlah token input/output/total, streaming atau tidak, kena cache atau tidak.
Jumlahnya tidak dibatasi, dan tier boleh ditaruh global atau per model — punya
model dievaluasi belakangan, jadi dia yang berkata terakhir.

Tier tidak pernah menyentuh angka backend: markup kita tidak bisa mengubah
tagihan orang lain. Daftar lengkapnya di [docs/CONFIG.md](docs/CONFIG.md).

### Jawaban yang menolak

Model yang menolak tetap harus membaca prompt-nya dulu sebelum memutuskan, jadi
request-nya tidak gratis — tapi juga tidak sepadan dengan harga satu jawaban
penuh. Ketiga model itu menagih **$0,05 per request** yang ditolak, flat,
menggantikan hitungan tokennya:

```json
"refusalUsd": 0.05,
"refusalPhrases": ["I cannot do that. I only provide AI roleplay."]
```

Penolakan dikenali dari jawabannya sendiri — penolakan itu `200` yang sukses,
bukan error — dengan mencocokkan kalimat di `refusalPhrases` di mana pun di
dalam jawaban, tanpa peduli huruf besar-kecil atau di mana barisnya dipotong.
Yang dicocokkan cuma bagian yang **diucapkan**: blok `<think>...</think>` yang
sebagian backend kirim sebagai `content` biasa dibuang dulu, karena penalaran
model rutin mengutip kalimat penolakan justru waktu ia memutuskan *tidak*
menolak — dan membacanya sebagai jawaban berarti menagih request yang dilayani
dengan harga penolakan. Penekanan markdown juga diabaikan, jadi
`**I cannot do that.** I only provide AI roleplay.` tetap terbaca sebagai
penolakan.
Barisnya tetap membawa `backend_usd` apa adanya, jadi ongkos mengatakan tidak
kelihatan sebagai rugi, bukan hilang dari pembukuan. `price_tiers` di baris itu
berbunyi `refusal`.

---

## 6. Logging dan metrik

Satu baris per panggilan, masuk SQLite (WAL, di-*bundle* jadi tidak bergantung
versi sqlite Termux). Handler tidak pernah menunggu disk: baris dikirim lewat
channel ke satu writer yang menulisnya per-batch dalam satu transaksi.

Yang dicatat: token input dan output, token cached dan reasoning, **TTFT**,
jendela generasi, **tokens/detik**, total latensi, lama antre, status, alasan
berhenti, retry, alias dan model backend, client key, IP, user agent, sumber
angka usage, drift lokal vs backend, dan preview prompt/jawaban.

### Token input pemanggil vs token yang ditagih backend

System prompt yang relay suntikkan itu biaya relay, bukan biaya pemanggil — dia
tidak menulisnya dan tidak bisa melihatnya. Jadi tiap baris menyimpan dua angka:

| Kolom | Artinya |
|---|---|
| `user_prompt_tokens` | yang ditagihkan ke pemanggil, dan satu-satunya yang dia lihat di `usage.prompt_tokens` |
| `billed_prompt_tokens` | yang ditagih backend, sudah termasuk system prompt |
| `system_prompt_tokens` | ukuran suntikan itu sendiri, menurut tokenizer relay |

Pemanggil dibebani **hitungan pesannya sendiri**: relay menghitung body yang
masuk dulu, baru menyuntik, baru mengirim. Kirim 6K token, yang balik ya 6K —
bukan 10K. Dari sisi pemanggil, suntikan system prompt dan markup itu tidak bisa
dibedakan, jadi angkanya memang harus sama persis dengan yang dia kirim.

Karena yang diukur adalah body **sebelum relay menyentuhnya**, ini tidak berhenti
di system prompt. Aturan rewrite request (`requestTransform.replace`) yang
memanjangkan teks pemanggil juga jadi biaya relay: yang dikirim ke backend
memang jadi lebih panjang, tapi angka pemanggil tidak ikut naik.

Angka pemanggil **tidak pernah datang dari backend**, bahkan ketika backend
melaporkan usage-nya sendiri. Dua alasan yang mengarah ke tempat yang sama:
tokenizer backend bukan urusan pemanggil, dan angka backend sudah termasuk
suntikan yang bukan tulisannya. Yang backend tagih tetap dicatat di
`billed_prompt_tokens` — itu margin operator, dan cuma operator yang melihatnya.

Mau menagihkan system prompt ke pemanggil? Nyalakan
`tokenizer.billSystemPromptToUser`. Dua angkanya tetap dicatat, jadi selisihnya
tetap terlihat di tab Usage.

Harga tiap request ikut tercatat di barisnya: `backend_usd`, `proxy_usd`,
`profit_usd`, dan `price_tiers` — nama tier yang benar-benar berlaku.

### Buku besar yang tidak bisa diubah

Selain log request yang bisa di-*prune*, ada tabel `usage_ledger` yang **hanya
menerima baris baru**:

- `UPDATE` dan `DELETE` ditolak trigger SQLite — dari proses ini maupun dari
  `sqlite3` di terminal sebelah.
- Tiap baris membawa hash baris sebelumnya. Jadi kalau ada yang menghapus
  trigger-nya lalu mengedit angka langsung di file, rantainya putus dan tab
  Usage menunjukkan **baris mana** yang tidak cocok.
- Tiap request menulis dua baris: `input` begitu backend menerima request
  (jadi token yang sudah terpakai tetap tercatat walau jawabannya tidak pernah
  datang), dan `final` saat selesai. Keduanya tidak pernah mencatat angka yang
  sama dua kali, jadi `SUM` di atas tabel selalu benar.
- Baris `input` ditulis sebelum backend sempat bicara, dan isinya hitungan lokal
  relay — yang juga angka final pemanggil, jadi biasanya tidak ada yang berubah
  di akhir. Kalau toh berubah, selisihnya **diposting sebagai koreksi** di baris
  `final`: angka minus, baris sendiri. Baris yang sudah masuk tidak pernah
  ditimpa, dan totalnya tetap sama persis dengan `usage.prompt_tokens` yang
  diterima pemanggil.

Isinya yang diringkas tab Usage: jumlah request, token masuk, token keluar,
TTFT, token/detik, dan cache hit. Prune tidak pernah menyentuh tabel ini —
angka usage tetap utuh walau log request-nya sudah dibersihkan.

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

### Jejak per request

Tiap request punya **UUID v4** yang dibuat saat dia masuk, dan uuid yang sama
itu yang dipakai di baris log, di `id` balasan, di header `x-relay-request-id`,
dan sebagai primary key barisnya di database. Jadi keluhan yang menyebut satu
id bisa langsung ditarik ke barisnya.

```
req 32dadfde in    2026-09-13T14:41:41.383+07:00 model=Wissangeni-512B-V1 key=hp user=tenant-42 effort=high stream=true bytes=147 ip=127.0.0.1
req 32dadfde inj         0.1ms  rule=spr_think mode=replace
req 32dadfde tok       811.1ms  9 caller / 9 upstream  o200k_base exact
req 32dadfde ttft     2014.5ms
req 32dadfde done     2766.8ms  status=200 stop
req 32dadfde sum   uid=32dadfde-f3dd-4ad8-acc5-387b72415f46 model=Wissangeni-512B-V1 key=hp user=tenant-42 effort=high | ram=82.6MB net=147B in/1 437B out/1 889B up | tok=9 in (0 cached, 0% hit) 909 out (629 reasoning) | backend=$0.000391 proxy=$0.001291 profit=$0.000900 [jam padat, thinking effort tinggi] | latency=2766.8ms ttft=2014.5ms tps=2171.84 (held to 6)
```

Baris terakhir sengaja memuat semuanya sekaligus: RAM, jaringan masuk/keluar/ke
backend, token in dan out beserta cache rate, total backend, total proxy,
profit, tier yang berlaku, latensi, TTFT dan TPS. Satu layar HP yang lewat sudah
cukup untuk tahu satu request itu berapa.

Semuanya lewat logger yang sama, yang menulis ke stderr dan ke `relay.log`
sekaligus — jadi **tab Logs di dashboard menampilkan persis yang ditampilkan
Termux**. Matikan lewat `logging.verboseRequests` kalau mau balik ke satu baris
per request.

### Siapa yang memanggil

Satu client key sering dipakai banyak orang. Kalau pemanggil mengirim `user` di
body (atau header `x-user-id`), id itu dicatat di barisnya **dan diteruskan ke
backend** — karena backend yang meng-cache prompt biasanya meng-*key* cache-nya
per user, dan tanpa itu prefix cache satu orang bisa dipakai request orang lain.
Matikan per backend lewat `forwardUserId` kalau cuma mau mencatat tanpa
meneruskan.

---

## Dua jenis client key

Sebuah client key sekarang punya **jenis**, dan jenis itu menentukan satu hal —
**request ini atas nama siapa.** Sisanya mengikuti dari situ.

|  | `company` | `private` |
|---|---|---|
| Di belakang key | banyak pengguna akhir | satu pemegang |
| `user_id` yang dikirim ke backend | yang dikirim pemanggil (`user` atau `x-user-id`) | identitas key itu sendiri |
| Kalau pemanggil mengisi `user` | dipakai | **diabaikan** |
| Rincian usage per | pengguna akhir | key |
| `maxTokensPerSecond` | berlaku | **tidak pernah berlaku** |

**Company** itu reseller. Tiap panggilan membawa pengguna akhirnya sendiri, dan
id itulah yang dilihat backend, yang memisahkan prompt cache satu pelanggan dari
pelanggan lain, dan yang jadi rincian di invoice. Ini persis perilaku lama, jadi
config yang sudah ada tidak berubah apa-apa — key tanpa `kind` dibaca sebagai
`company`.

**Private** itu satu orang, dan key-nya *adalah* penggunanya. Apa pun yang
pemanggil tulis di `user` tidak digubris: dia tidak bisa menaruh pemakaiannya
atas nama orang lain, dan tidak bisa masuk ke partisi cache milik orang lain.
Yang naik ke backend diatur `security.privateUserId`, dan defaultnya **sidik
jari** dari key (SHA-256 dipotong), bukan key-nya:

```jsonc
// company key                        // private key
{ "model": "…",                       { "model": "…",
  "user": "pelanggan-42",               "user": "u_9f3c1ab77e20d4e1c8b6a015",
  "user_id": "pelanggan-42" }           "user_id": "u_9f3c1ab77e20d4e1c8b6a015" }
```

Stabil, unik per key, dan tidak membocorkan apa-apa. Mengirim key-nya sendiri
bisa (`privateUserId: "secret"`) tapi bukan default, karena itu artinya menulis
kredensial yang masih berlaku ke log request orang lain.

### Private key tidak direm

`maxTokensPerSecond` ada supaya trafik satu reseller tidak menghabiskan uplink
HP untuk semua orang. Key dengan satu pemegang di belakangnya adalah kasus yang
tidak merugikan siapa-siapa, jadi **balasannya keluar secepat backend bisa
menghasilkannya** — tidak ada pacing sama sekali. Baris request-nya mencatat
`targetTps` = 0, bukan angka batas yang sebenarnya tidak pernah dipakai.

---

## Usage per key, invoice, dan reset

Tiap key punya **usage**-nya sendiri: total harga yang dipakai, model apa saja
yang dipakai, dan berapa masing-masing model. Semua angka itu datang dari
`usage_ledger`, jadi harga yang tercatat adalah harga **saat request itu jalan**
— mengubah daftar harga besok tidak menulis ulang tagihan kemarin.

```
Billing → per key
┌───────────────────────────────────────────────────────────────────┐
│ acme          company   1.204 request   14,2 jt token   $38,4102  │
│ pribadi-ku    private       310 request  2,1 jt token    $6,8830  │
└───────────────────────────────────────────────────────────────────┘
                    ↓ buka salah satu
        model-a   842 request   $24,1180
        model-b   362 request   $14,2922
```

### "Reset usage setelah invoice keluar"

Buku besarnya **hanya bisa ditambah** — SQLite menolak mengubah maupun menghapus
barisnya, dan tiap baris membawa hash baris sebelumnya. Jadi invoice tidak
menghapus apa pun. Dia **menarik garis**:

```
  usage_ledger   ─── seq ───────────────────────────────────────────────►
    … 41  42  43 │ 44  45  46  47 │ 48  49  50 …
                 │                │
           invoice #1        invoice #2         "belum ditagih"
           toSeq = 43        toSeq = 47         = semua setelah 47
```

Angka **belum ditagih** milik sebuah key adalah semua yang dia pakai setelah
garis yang ditarik invoice terakhirnya. Menerbitkan invoice memajukan garis itu
— makanya angkanya jadi nol sesudahnya, tanpa satu baris pun dihapus, dan
periode lama masih bisa dibaca ulang berbulan-bulan kemudian.

Yang perlu diketahui:

- Invoice tidak bisa dibatalkan penerbitannya. **Void** menandainya batal dan
  mengembalikan periodenya, jadi invoice berikutnya menagih dua periode.
- Request yang masih jalan saat invoice terbit masuk ke invoice berikutnya.
  Harganya baru diketahui saat jawabannya selesai, dan memang belum ditagih.
- `billing.minimumUsd` **menahan** periode kecil supaya tetap terbuka, bukan
  membuangnya — pemakaiannya bergulir ke invoice berikutnya.
- Angka di invoice yang sudah terbit ikut di-hash, dan SQLite menolak `UPDATE`
  yang menyentuhnya. Yang masih boleh berubah cuma `status`, `settledAt`, dan
  catatan.

Terbitkan lewat tombol di tab **Billing**, atau nyalakan `billing.autoIssue`
supaya siklus bulanan mengerjakannya sendiri — hanya untuk key yang memasang
`billing.autoInvoice`, karena menutup periode tagihan itu keputusan.

---

## 7. Dashboard

Di `http://127.0.0.1:8788`, terikat ke localhost dan **tidak pernah dilewatkan
tunnel**. Vanilla JS, tanpa build step, tanpa CDN — dan sekarang **ikut
ter-compile ke dalam binary**, jadi relay bisa dijalankan dari direktori mana pun.

| Tab | Isinya |
|---|---|
| Setup | checklist apa yang belum siap, service/shortcut/boot, wake lock, pasang paket, start–stop–restart relay |
| Overview | statistik, grafik harian, rincian per model dan per key |
| Models | editor alias: terjemahan nama, prompt, params, limit, tokenizer, reshaping |
| Backends | provider upstream + tombol tes koneksi |
| Prompts | library system prompt |
| Keys | client key, jenisnya (company/private), kuota, batasan model, data penagihan |
| Requests | log per panggilan + rincian timing dan token |
| Usage | angka dari buku besar: request, token, harga, TTFT, TPS, cache hit, dan status rantai hash |
| Billing | yang belum ditagih per key + rincian per model, terbitkan invoice, riwayat invoice |
| Tokenizer | playground token, biaya satu request chat, pasang vocabulary |
| Playground | kirim request beneran lewat relay |
| Tunnel | start/stop cloudflared, URL publik, output mentah |
| API Docs | dokumentasi integrasi yang ditulis dari config yang sedang jalan — base URL, endpoint, model, parameter, field usage, limit, error; bisa disalin sebagai Markdown |
| Settings | server, antrean, security, logging, aturan tokenizer, default, OpenRouter |
| Logs | ekor `relay.log` |

Beri password lewat Settings kalau HP-mu dipakai orang lain. Secret selalu
tampil termask, dan menyimpan form tidak akan menimpa key asli dengan masknya.

### Dokumentasi API untuk yang mau integrasi

Tab **API Docs** menyusun dokumentasi lengkap dari config yang sedang jalan:
base URL (pakai URL tunnel kalau tunnel hidup), daftar endpoint, id model
persis seperti yang dikembalikan `GET /v1/models`, parameter mana yang
diproses relay dan mana yang diteruskan apa adanya, field `usage` yang dipakai
buat menagih, limit per key, bentuk error, plus contoh curl, Python, dan
potongan SSE.

Tidak ada satu pun angka di halaman itu yang diketik tangan — semuanya dibaca
dari config, jadi begitu harga, kuota, atau limit berubah, dokumennya ikut
berubah. Tombol **Copy as Markdown** mengeluarkan seluruh halaman sebagai satu
dokumen Markdown yang tinggal ditempel ke email calon integrator; **Download
.md** menyimpannya sebagai file.

Nilai key tidak pernah ikut: contohnya memakai placeholder
`Kunci-Zeiko-XXXX…`, jadi dokumen itu aman dikirim, dan key asli dikirim
terpisah lewat jalur yang kamu percaya.

---

## 8. Cloudflare Tunnel

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
| `maxConcurrentRequests` | `512` | berapa request yang dikerjakan sekaligus. Bisa diubah dari dashboard dan langsung berlaku, tanpa restart |
| `queueCapacity` | `2048` | berapa yang boleh mengantre. `0` berarti langsung tolak, tanpa antre |
| `queueTimeoutMs` | `30000` | berapa lama sebuah request menunggu giliran sebelum menyerah. Setel di bawah timeout client-mu |
| `workerThreads` | `0` | jumlah thread Tokio; `0` berarti satu per core. Turunkan kalau mau menyisakan tenaga buat aplikasi lain |
| `keepAliveTimeoutMs` | `75000` | keep-alive koneksi masuk |

Request yang lewat batas **mengantre**, bukan langsung ditolak: pemanggil yang
menunggu 300 ms lalu dapat jawaban lebih terlayani daripada yang dapat 503 lalu
retry ke tembok yang sama. Antreannya FIFO — yang paling lama menunggu dapat slot
berikutnya, jadi tidak ada yang kelaparan di belakang gelombang pendatang baru.
Yang dijawab **503 + `Retry-After`** hanya kalau antreannya penuh atau
menunggunya lewat batas waktu.

Antreannya di memori, bukan di disk. Tiap request di dalamnya sudah berupa
koneksi HTTP terbuka dengan pemanggil menunggu di ujung sana; menuliskannya ke
disk dulu justru menambah latensi tepat di jalur yang sedang tertekan, dan tidak
membeli apa-apa — koneksinya ikut mati bersama prosesnya.

Tab **Settings** menampilkan keadaan antrean saat itu juga: berapa yang jalan,
berapa yang menunggu, puncak antrean, rata-rata lama menunggu, dan berapa yang
ditolak.

Kuota per key (`keys[].quota`) dihitung di memori: `requestsPerMinute` pakai
sliding window ter-*shard*, `requestsPerDay` dan `tokensPerDay` pakai counter
harian yang di-seed dari database saat start dan berputar sendiri di tengah malam
zona waktumu.

---

## Jadi provider di OpenRouter

OpenRouter membaca satu URL untuk tahu model apa yang relay ini layani dan
berapa harganya. Nyalakan lewat **Settings → OpenRouter**, lalu isi harga tiap
model di **Models → (pilih model) → OpenRouter**.

```bash
curl http://127.0.0.1:8787/provider/models
```

Dokumennya mengikuti `schema_version` 2.4: modality masuk dan keluar,
`supported_parameters`, `pricing`, `capacity`, kuantisasi, tokenizer, datacenter,
dan compliance.

Beberapa keputusan yang sengaja diambil:

- **Nama model backend tidak pernah muncul.** Yang dipublikasikan `id` publik,
  sama seperti `/v1/models`. Ada test yang mengunci ini.
- **Harga yang belum diisi tidak diterbitkan**, bukan diterbitkan sebagai nol.
  Harga salah lebih berbahaya daripada harga yang belum ada.
- **Harga disimpan sebagai teks**, bukan angka. `0.0000006` kehilangan digit
  terakhirnya kalau lewat `f64`, sementara OpenRouter membacanya sebagai desimal.
- **Parameter yang relay buang tidak diiklankan.** Kalau `dropParams` memuang
  `top_p`, dia tidak muncul di `supported_parameters`.
- **Concurrency yang diterbitkan adalah batas relay yang sebenarnya** kalau kamu
  tidak mengisinya sendiri. Justru itu gunanya: OpenRouter jadi tidak mengirim
  lebih banyak daripada yang sanggup dilayani HP.
- **Klaim zero-data-retention ditolak** selama `logging.storeBodies` masih
  menyimpan teks prompt. Menerbitkannya berarti berbohong ke pengguna OpenRouter.

Tombol **Preview what OpenRouter sees** di Settings menampilkan dokumen persis
seperti yang akan diterima OpenRouter.

Yang OpenRouter ukur sendiri — uptime, TTFT, dan throughput — adalah angka yang
sudah relay catat juga, jadi tab Usage dan penilaian mereka melihat hal yang sama.

| Setelan | Di mana |
|---|---|
| `openrouter.enabled` | terbitkan dokumennya atau tidak |
| `openrouter.path` | URL-nya; ganti path perlu restart, `/provider/models` tetap aktif |
| `openrouter.token` | opsional, kalau daftar harganya tidak mau dibaca sembarang orang |
| `openrouter.isReady` | matikan untuk terdaftar tanpa dikirimi trafik |
| `models[].openrouter.*` | harga, kapasitas, kuantisasi, modality, dan batas tiap model |

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
`POST /v1/chat/completions`, `POST /v1/completions`, `POST /v1/embeddings` —
semuanya juga tanpa awalan `/v1` (`/models`, `/chat/completions`,
`/completions`, `/embeddings`), karena sebagian klien menambahkan `/v1` sendiri
dan sebagian lagi sudah diberi base URL yang berakhir di situ.

`GET /v1/models` memakai amplop OpenAI di luar (`object: "list"`,
`object: "model"`, `owned_by`) dan dokumen model OpenRouter di dalam:
`architecture`, `pricing` lengkap dengan `overrides` per jam dan per hari,
`top_provider`, `supported_parameters`, `reasoning`. Klien OpenAI lama tetap
jalan; klien yang mau tahu harga tidak perlu bertanya ke siapa pun.

---

## CLI

Semua ini juga ada di tab **Setup** dashboard. CLI-nya dipertahankan buat
scripting dan buat kalau dashboard-nya sendiri yang bermasalah.

```
chtting-relay start [--port N] [--no-dashboard] [--replace]
chtting-relay setup                     keeper + shortcut + hook boot
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
  pricing.rs     thinking effort, kartu tarif tiga pita, tier, backend/proxy/profit
  relay/         upstream + fallback, transform, SSE, pacing, trace, handler
  rotate.rs      ganti instance tiap jam tanpa memutus koneksi
  server/        API publik, dashboard + admin API
  store/         SQLite, buku besar, invoice, quota tracker, rate limiter
  system.rs      keeper, shortcut, hook boot, wake lock, restart/stop
  tunnel.rs      supervisor cloudflared
public/          dashboard (vanilla JS, ikut ter-compile ke binary)
scripts/         setup Termux, build Android
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
sungguhan lewat HTTP di depan backend tiruan — termasuk 300 pemanggil serentak,
antrean yang menahan lonjakan tanpa menolak siapa pun, antrean penuh yang
dijawab 503 + `Retry-After`, dan buku besar yang menolak diubah lalu tetap
mendeteksi perubahan yang dilakukan lewat belakang trigger-nya. Ada juga
`tests/billing.rs`: private key yang tidak bisa mengaku jadi orang lain dan
tidak direm, company key yang meneruskan pengguna akhirnya, harga yang masuk ke
buku besar, dan invoice yang menutup periode lalu meninggalkan periode kosong
tanpa menghapus satu baris pun.

## Catatan keamanan

- **Tidak ada satu pun data backend yang bocor ke pemanggil.** Setiap balasan
  dibangun ulang dari amplop milik kita sendiri — uuid v4 kita, timestamp kita,
  nama model publik kita — lalu hanya beberapa field yang disalin masuk. Jadi
  `system_fingerprint`, id request backend, `created`-nya, `service_tier`,
  `logprobs`, dan field apa pun yang backend tambahkan besok tidak ikut, karena
  memang tidak pernah disalin. Menyaring dengan cara menghapus itu terbalik:
  yang terhapus cuma yang sempat kita sebut namanya.
- `usage` juga punya kita: `prompt_tokens` adalah hitungan tokenizer relay atas
  body pemanggil sendiri, bukan angka backend, dan `usage` di dalamnya adalah
  harga menurut daftar harga kita. Angka backend tetap dicatat di baris request
  supaya marginnya kelihatan — oleh operator, bukan oleh pemanggil.
- Keep-alive saat backend diam dikirim dengan kalimat kita sendiri; keep-alive
  milik backend di-parse lalu dibuang, karena bentuknya saja sudah menunjukkan
  backend mana yang di belakang.
- API key backend tidak pernah keluar dari HP; pemanggil hanya memegang client key relay.
- **Client key juga tidak keluar dari HP.** Private key mengirim sidik jari
  dirinya ke backend, bukan key-nya — stabil dan unik, tapi tidak bisa dipakai
  siapa pun untuk memanggil relay ini.
- **Loopback bukan pagar di Android.** Semua aplikasi di HP bisa membuka
  `127.0.0.1:8788`, dan halaman web yang kamu buka pun bisa *mengirim* request
  ke sana walau tidak bisa membaca jawabannya — padahal menambah backend atau
  mematikan `requireClientKey` tidak butuh jawaban untuk jadi berguna. Karena
  itu dashboard menolak request dengan `Origin` dari situs lain, dan `Host`
  yang bukan nama mesin ini (itu cara kerja DNS rebinding).
  **Tetap pasang `dashboard.password`**: penjaga origin itu urusan browser,
  password itu urusan segala hal yang bukan browser. Tab Setup mengingatkan
  kalau belum ada.
- `CF-Connecting-IP` / `X-Forwarded-For` hanya dipercaya kalau koneksinya sendiri
  datang dari mesin ini, tempat cloudflared jalan. Header dari jaringan itu cuma
  string yang diketik pemanggil, dan `blockedIps` dicek terhadap hasilnya — kalau
  dipercaya, daftar blokirnya cuma jadi saran.
- Perbandingan client key memakai constant-time compare. Pencariannya lewat
  indeks hash, jadi jumlah key tidak mengubah waktu yang dibutuhkan.
- Prompt hanya disimpan lokal. Kalau tidak mau disimpan sama sekali, set
  `logging.storeBodies` ke `none`.
- Client key ditampilkan penuh sekali saat dibuat, sesudah itu selalu termask.

## Lisensi

MIT
