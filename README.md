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
bash install.sh
```

Empat baris, dan **`bash install.sh` itu juga cara update-nya.** Setiap kali
dijalankan script-nya melakukan urutan yang sama:

| | Yang dikerjakan |
|---|---|
| **1** | `git pull --ff-only` — ambil commit terbaru |
| **2** | `pkg update` + `pkg upgrade` — paket Termux naik versi, lalu `rust`, `clang`, `binutils`, `pkg-config` dipastikan ada |
| **3** | `cargo build` — compile dari source |
| **4** | config awal, client key pertama, keeper + shortcut layar utama + hook boot, lalu relay-nya jalan |

Jadi tidak ada perintah update yang harus diingat: **jalankan ulang baris yang
sama.** Kalau script-nya sendiri ikut berubah waktu di-pull, dia menyerahkan
prosesnya ke versi baru itu di tengah jalan — bukan meneruskan pakai versi lama
yang sudah dibaca separuh.

> **Compile-nya 5 sampai 15 menit** di HP untuk yang pertama, setelah itu jauh
> lebih cepat karena `target/` sudah terisi. Yang dipasang cuma `rust` dan
> `clang`; tidak ada cmake, tidak ada Go, tidak ada Node. Kalau core-nya sedikit
> atau RAM-nya di bawah 4 GB, script otomatis pakai profil `release-small` dan
> `-j1` supaya buildnya tidak kena OOM-kill.

Dua pilihan yang ada: `--no-start` (siapkan saja, jangan jalankan) dan
`--no-pull` (build yang sudah ada di checkout, tanpa jaringan).

`git pull` di situ aman untuk data kamu: `config/config.json`, `data/` dan log
semuanya ada di `.gitignore`, jadi config hidup, database dan key tidak
tersentuh. Kalau checkout-mu punya commit atau editan sendiri sehingga tidak
bisa fast-forward, script-nya bilang dan tetap lanjut build apa yang ada.

Binary rilis di halaman Releases dibuat untuk **arm64** (hampir semua HP sejak
~2016) dan **armv7** (HP 32-bit) — keduanya binary Android asli, bukan emulasi.
Installer-nya **tidak** memakai itu; ambil manual kalau memang mau melewati
compile.

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

Makanya sebelum apa pun dikerjakan, `start` mengambil **sewa port** di
`src/lock.rs`: satu *abstract unix socket* yang namanya diambil dari nomor
port. Bind-nya satu syscall yang atomik — tidak ada celah untuk balapan — dan
namanya hidup di ruang nama milik kernel, bukan di filesystem. Artinya
`--home`, `$TMPDIR`, dan direktori kerja tidak ikut menentukan: dua proses yang
sama-sama mau port 8787 pasti bertemu di nama yang sama, sekalipun segala hal
lain tentang mereka berbeda. Kernel juga yang melepasnya begitu pemegangnya
mati — exit bersih, panic, `SIGKILL`, atau dibunuh *low-memory killer* Android
— jadi tidak pernah ada sisa kunci basi yang harus dibersihkan.

Yang kedua menolak start dan keluar dengan **exit code 3**, menyebut port-nya
dan siapa yang memegangnya. Pemegang sewa mendengarkan di sewanya sendiri dan
menjawab pid-nya kalau ditanya, jadi `--replace` tahu persis siapa yang harus
disuruh berhenti: minta baik-baik (`SIGTERM`), tunggu sewanya lepas, baru
memaksa. Keeper-nya mengerti exit code 3 sebagai "tunggu, jangan restart" —
selain itu tetap dianggap crash dan di-restart seperti biasa.

Rotasi tetap dikecualikan, karena di situlah dua proses melayani satu port
memang benar. Tapi successor harus **membuktikan** dirinya successor: ia
membawa token sekali pakai yang dicetak pendahulunya di
`data/run/handover-<gen>`. `CHTTING_GENERATION` saja tidak cukup — variabel
environment diwariskan ke semua keturunan dan dulu itu saja sudah bisa
melewati pemeriksaan. Dan successor belum jadi relay sampai ia memegang
sewanya, yang baru bisa diambil setelah pendahulunya benar-benar pergi. Kalau
pendahulunya tidak pergi juga, ia disuruh pergi. Jadi tumpang tindihnya
berbatas di kedua ujung.

`data/run/serving.pid` masih ada, tapi sekarang cuma **petunjuk** buat pesan
error. Ia berkunci pada direktori data, dan justru itu sebabnya dulu ia tidak
pernah bisa menangkap dua salinan yang dijalankan dengan `--home` berbeda.
`chtting-relay doctor` menyebutkan port-nya bebas atau sedang dipakai siapa.

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

### Kalau perangkatnya justru lapang

Tablet kelas atas — Dimensity 9400+, Snapdragon 8 Elite, 12–16 GB RAM — punya
core dan memori yang cukup untuk build yang lebih mahal dan biner yang lebih
cepat:

```bash
# fat LTO, satu codegen unit — beberapa menit, beberapa GB RAM
CHTTING_PROFILE=release-fast bash scripts/build-android.sh

# sekalian targetkan chip-nya sendiri
CHTTING_PROFILE=release-fast bash scripts/build-android.sh --tune cortex-x925
```

`--tune` melepas asumsi ARMv8.0 baseline yang dipakai supaya binernya jalan di
perangkat arm64 mana pun. Di core ARMv9.2 itu berarti ekstensi kriptografinya
ikut terpakai — relay meng-SHA-256 setiap client key yang masuk, merantai setiap
baris ledger, dan menghash setiap invoice — plus instruksi dot-product/i8mm dan
model penjadwalan untuk pipeline yang benar.

Harganya: **hasilnya cuma jalan di chip sekelas itu.** Biner ber-`--tune` yang
disalin ke perangkat lama tidak gagal dengan sopan, dia kena SIGILL. Makanya ini
opt-in dan build tanpa tuning tetap yang default.

Di SoC big.LITTLE, sebut core mana saja dari klasternya: semua core dalam satu
SoC mengimplementasikan versi arsitektur yang sama, jadi pilihannya mengubah
model penjadwalan, bukan instruksi yang tersedia. Dimensity 9400+ itu 1×
Cortex-X925 + 3× Cortex-X4 + 4× Cortex-A720, semuanya ARMv9.2-A, jadi
`cortex-x925` aman untuk kedelapan-delapannya.

Nama yang dikenal toolchain-mu:
`rustc --print target-cpus --target aarch64-linux-android`.

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
berpikir, dan yang tidak. Effort yang bisa diminta pemanggil ada empat (off,
low, high, max), dan dua kotak itu membelahnya jadi dua:

* **Default** — `systemPrompt` model itu sendiri. Dipakai untuk pemanggil yang
  benar-benar minta model berpikir (`medium`, `high`, `max`), dan untuk semua
  orang selama kotak kedua masih di mode `none`.
* **No thinking** — satu aturan `systemPrompts[]` dengan id khusus
  `sp-non-thinking` dan `efforts: ["none", "minimal", "low"]`.

```json
"systemPrompts": [
  { "id": "sp-non-thinking", "name": "No thinking", "enabled": true,
    "efforts": ["none", "minimal", "low"],
    "prompt": { "mode": "replace", "text": "Jawab langsung, tanpa basa-basi." } }
]
```

Id itu dipesan, dan daftar effort-nya ikut dipesan: relay menulis ulang daftar
itu tiap kali config dibaca dan disimpan, jadi model yang disimpan waktu
pembagiannya masih di tempat lain ikut maju sendiri — tidak perlu dibuka dan
di-Save satu per satu. Aturan tulisanmu sendiri, dengan id apa pun selain itu,
tidak disentuh.

Pemanggil yang **tidak menyebut effort sama sekali** tidak ada di daftar itu,
karena dia tidak pernah sampai ke sana: diamnya diterjemahkan dulu jadi effort
beneran oleh `defaults.effort` (Settings → *Unspecified thinking*), yang isinya
`high`. Satu nilai itu yang menentukan prompt mana yang dikirim, pita harga mana
yang ditagih, dan apa yang ditulis di baris request — jadi tidak ada request yang
dibilang satu hal lalu ditagih hal lain. Ada test yang mengunci ini.

Kotak yang dibiarkan di mode `none` disimpan sebagai aturan **disabled**: relay
melewati aturan disabled, jadi pemanggilnya tetap jatuh ke Default persis
seolah-olah aturannya tidak ada — tapi teks yang sudah kamu tulis masih ada di
kotaknya waktu model itu dibuka lagi. (Dulu aturannya dibuang, jadi prompt yang
diketik di kotak yang mode-nya masih `none` hilang di balik toast "Saved".)

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
| tanpa thinking | `none`, `minimal` | `nonThinking` |

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

Pemanggil yang tidak pernah menyebut thinking ditagih di pita untuk effort hasil
terjemahan `defaults.effort` — `high`, jadi pita baku, kecuali kamu mengubahnya.
Setel ke `none` kalau kamu mau diam ditagih sebagai tanpa thinking, atau ke
`default` kalau diam memang tidak boleh dihitung sebagai effort apa pun.

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

### Mengirim trace ke Langfuse

Nyalakan di **Settings → Langfuse tracing**, isi `publicKey` dan `secretKey`.
Satu span per request, membawa empat hal yang **tidak bisa diketahui dari log
backend**:

| Isi span | Di mana |
|---|---|
| body pemanggil, sebelum relay menyentuhnya | `langfuse.observation.input` |
| system prompt yang relay suntikkan | `…metadata.injected_system_prompt` |
| body yang benar-benar diterima backend | `…metadata.upstream_input` |
| jawaban yang dikirim balik, setelah reshaping | `langfuse.observation.output` |

Ditambah aritmetikanya: token hitungan lokal, token yang ditagih backend, cached
dan reasoning, `proxy_usd` / `backend_usd` / `profit_usd`, TTFT, lama antre, lama
tokenizing, lama injeksi, token per detik, retry, cache hit, finish reason, dan
model publik vs model backend.

**Trace id-nya adalah uuid request itu sendiri** (tanpa tanda hubung), jadi uid
yang kamu lihat di `relay.log` bisa langsung ditempel ke pencarian Langfuse.

Lewat **OpenTelemetry** (`POST /api/public/otel/v1/traces`), bukan endpoint
`/api/public/ingestion`: dokumen API Langfuse sendiri menandainya deprecated, dan
di Langfuse Cloud endpoint itu berhenti menerima trace saat mode tulis v4-only
mulai **16 November 2026**. JSON protobuf juga berarti tidak perlu `prost` dan
tidak perlu codegen saat build — cuma `serde_json` dan `reqwest` yang sudah ada.

Setiap batch membawa header **`x-langfuse-ingestion-version: 4`**. Tanpa header
itu span-nya tetap diterima, tapi lewat jalur kompatibilitas lama — dan bisa
**telat sampai lima belas menit** muncul di data model v4 maupun di API
Observations/Metrics v2. Trace di sini dibaca sewaktu request-nya masih di layar
orang, jadi telat seperempat jam sama saja dengan hilang. Langfuse versi lama
yang belum mengenal header itu mengabaikannya, jadi `host` self-hosted lama tidak
dirugikan.

Yang membuat span-nya benar-benar **berbentuk v4**, bukan cuma lewat jalurnya:

- **Input dan output menempel di observation**, yaitu
  `langfuse.observation.input` / `…output` — bukan `langfuse.trace.input` /
  `…output`. Di v4 tidak ada entitas trace terpisah: trace itu kumpulan
  observation dengan trace id yang sama, dan input/output milik root observation
  **adalah** input/output keseluruhan trace-nya. Atribut trace yang lama cuma
  disisakan supaya evaluator LLM-as-a-judge level-trace bikinan sebelum v4 masih
  jalan; relay ini tidak mengirimnya, jadi **evaluator yang diarahkan ke trace
  input/output tidak akan jalan** — arahkan ke root observation-nya.
- **Semua yang dipakai untuk filter ada di span itu sendiri**, tidak di
  induknya: `langfuse.user.id`, `langfuse.session.id`, `langfuse.trace.name`,
  `langfuse.trace.tags`, `langfuse.environment`, `langfuse.release`, dan
  `langfuse.version` (string yang sama dengan `release`, di bawah nama yang
  dipakai tabel observation untuk mengelompokkan). Satu request = satu span,
  yang sekaligus root observation **dan** generation pembawa biaya — jadi
  sekarang gratis, dan itu juga alasan span kedua di sini nanti harus diberi set
  yang sama, bukan mewarisinya.
- **Satu span id dikirim sekali saja**, setelah request-nya selesai. v4 tidak
  menjamin dedup span id yang sudah diterima: mengirim ulang untuk mengoreksi
  justru membuat observation **kedua** dan menggelembungkan semua hitungan yang
  diambil darinya.

Yang **tidak pernah** ikut:

- **Key apa pun.** Private key yang `privateUserId`-nya `secret` dikirim sebagai
  sidik jari, dan field yang bisa diisi client key (`user`, `user_id`,
  `api_key`, …) dibuang dari setiap body.
- **Apa pun, kalau antreannya penuh.** Span-nya dibuang dan dihitung.

Tidak ada satu pun yang terjadi di jalur request: `submit` cuma `try_send` ke
channel berbatas, body dipegang lewat `Arc` dan baru diserialisasi kalau span-nya
memang jadi dikirim, dan batch-nya dikirim dari task terpisah. Langfuse yang
mati, lambat, atau salah setelan **tidak bisa** memperlambat atau menggagalkan
satu pun panggilan — dia mundur ke satu percobaan per menit, dan penghitungnya
yang memberi tahu. Lihat sent / queued / dropped / failed di Settings atau di
`GET /api/langfuse`.

`sampleRate` mengatur berapa banyak request **sukses** yang di-trace; kegagalan
selalu ikut selama `captureErrors` menyala, karena kegagalan itu justru alasan
orang membuka trace viewer. Keputusannya diambil di awal request, jadi request
yang tidak terpilih tidak membayar untuk menangkap sesuatu yang akan dibuang.

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
- **Hanya invoice terbaru yang boleh di-void.** Periode itu rentang `seq`, dan
  awal periode berikutnya dibaca dari invoice terbaru yang masih berlaku.
  Kalau yang di tengah di-void, rentangnya tidak tercakup siapa pun — tidak
  ditagih, tidak muncul sebagai belum-ditagih, uangnya hilang begitu saja dari
  pembukuan. Void dari yang terbaru dulu, baru yang sebelumnya, lalu terbitkan
  ulang; penolakannya menyebutkan invoice mana yang harus di-void duluan.
- **Tidak ada kolom `paid` di tiap request.** Baris mana masuk invoice mana
  dihitung saat dibaca — satu invoice mencakup rentang `seq`, dan satu key
  punya puluhan invoice berbanding jutaan baris, jadi join-nya kecil. Menulis
  jawabannya ke tiap baris berarti menulis ulang semua baris belum-ditagih tiap
  kali invoice terbit: di ratusan ribu request per minggu itu jutaan baris,
  ditulis ulang persis saat relay sedang mencatat trafik yang jalan — antrean
  writer-nya yang jebol duluan. Tab Usage tetap menampilkan nomor invoice dan
  status `unbilled` / `issued` / `paid` per baris.
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

### Isi invoice

```
┌──────────────────────────────────────────────────────────────────────┐
│ INV-2026-0001                                       [issued]         │
│                                                                      │
│ Dari    PT Zeiko Relay Indonesia      Ditagihkan ke  CV Pelanggan    │
│         Jakarta Selatan                              Budi            │
│         NPWP 01.234.567.8-901.000                    Bandung         │
│                                                                      │
│ Tanggal invoice  16 Sep 2026      Bayar paling lambat  19 Sep 2026   │
│                                                                      │
│ Model            Req     In      Out    Cached  Reasoning  Jumlah    │
│ model-a          842   9,1 jt   2,4 jt   1,2 jt    180 rb  $24,1180  │
│ model-b          362   3,8 jt   1,1 jt     410 rb  60 rb   $14,2922  │
│                                                                      │
│ Total pemakaian  1.204 request · 16,4 jt token                       │
│ Subtotal $38,4102   Pajak 11% $4,2251   Total $42,6353               │
│                                                                      │
│ Bayar    42.635300 USDT      ≈ Rp 692.823                            │
│ Kurs     Rp 16.250 / USDT    (Indodax mid, 16 Sep 2026)              │
│ Alamat   TQn9Y2khEsLJW1ChVWFMSMeRDow5KcbLSE   (TRC20)                │
│          Kirim hanya lewat TRC20. USDT yang dikirim ke alamat ini    │
│          lewat jaringan lain tidak bisa dikembalikan.                │
└──────────────────────────────────────────────────────────────────────┘
```

Setel di **Settings → Billing**: `dueDays` (bawaan 3 hari), alamat USDT,
jaringannya, kurs IDR per USDT, dan sumber kursnya. Nama perusahaan pelanggan
ada di **Keys → Billing → Company**.

Relay menghitung semuanya dalam USD; dua langkah mengubahnya jadi angka yang
ditransfer pelanggan:

```
total_usdt = total_usd / usdPerUsdt     dibulatkan 6 desimal, satuan terkecil transfer USDT
total_idr  = total_usdt * idrPerUsdt    dibulatkan ke rupiah utuh
```

`usdPerUsdt` ada dan tidak diasumsikan 1, karena USDT pernah lepas peg — invoice
yang menghardcode 1.0 akan salah harga persis di hari itu penting. Angka rupiah
diambil dari total **setelah pajak**, bukan subtotal.

**Alamat, jaringan, kurs, dan tanggal jatuh tempo dibekukan saat invoice terbit.**
Tidak dibaca ulang dari config sesudahnya: ganti dompet besok, invoice yang masih
dipegang pelanggan tetap menyebut dompet yang dia terima. Semuanya ikut content
hash dan ikut trigger SQLite yang menolak menulis ulang invoice yang sudah
terbit.

Invoice yang terbit sebelum field-field ini ada membawa `hashVersion: 1` dan
diverifikasi dengan bentuk kanonik yang memang dipakai saat menulisnya. Kalau
field baru ikut di-hash begitu saja, semua invoice lama akan dilaporkan sebagai
"diubah" padahal tidak — dan pemeriksaan keutuhan yang sering salah lapor adalah
pemeriksaan yang tidak ada yang baca.

---

## 7. Dashboard

Di `http://127.0.0.1:8788`, terikat ke localhost. Vanilla JS, tanpa build step,
tanpa CDN — dan **ikut ter-compile ke dalam binary**, jadi relay bisa dijalankan
dari direktori mana pun.

Bindingnya tetap localhost walau kamu mau membukanya dari perangkat lain, dan
jangan diganti ke `dashboard.host: "0.0.0.0"`: itu menerbitkannya ke semua
perangkat di Wi-Fi itu tanpa TLS dan tanpa cara menariknya kembali. Yang dipakai
adalah salah satu dari dua jalur ber-TLS lewat cloudflared — `/dashboard` di
tunnel relay yang sudah hidup (`dashboard.publishOnRelay`, satu proses untuk
dua-duanya) atau `dashboard.tunnel` yang punya URL sendiri. Lihat
[bagian Cloudflare Tunnel](#membuka-dashboard-dari-perangkat-lain).

| Tab | Isinya |
|---|---|
| Setup | checklist apa yang belum siap, service/shortcut/boot, wake lock, pasang paket, stop relay, **Update & restart** |
| Overview | statistik, grafik harian, rincian per model dan per key, plus tombol **Update & restart** dan **Restart** |
| Models | editor alias: terjemahan nama, prompt, params, limit, tokenizer, reshaping |
| Backends | provider upstream + tombol tes koneksi |
| Prompts | library system prompt |
| Keys | client key, jenisnya (company/private), kuota, batasan model, data penagihan (termasuk nama perusahaan) |
| Requests | log per panggilan + rincian timing dan token |
| Usage | angka dari buku besar: request, token, harga, TTFT, TPS, cache hit, dan status rantai hash |
| Billing | yang belum ditagih per key + rincian per model, terbitkan invoice, riwayat invoice dengan tanggal jatuh tempo dan total USDT/IDR |
| Tokenizer | playground token, biaya satu request chat, pasang vocabulary |
| Playground | kirim request beneran lewat relay |
| Tunnel | start/stop cloudflared untuk **dua** tunnel — relay dan dashboard — URL publik masing-masing, output mentah |
| API Docs | dokumentasi integrasi yang ditulis dari config yang sedang jalan — base URL, endpoint, model, parameter, field usage, limit, error; bisa disalin sebagai Markdown |
| Settings | server, antrean, security, logging, **Langfuse**, aturan tokenizer, default, OpenRouter |
| Logs | ekor `relay.log` |

Beri password lewat Settings kalau HP-mu dipakai orang lain. Secret selalu
tampil termask, dan menyimpan form tidak akan menimpa key asli dengan masknya.

### Lupa password dashboard

Tidak ada email pemulihan dan tidak ada pertanyaan rahasia — dan memang tidak
boleh ada. Di balik tunnel, password dashboard itu satu-satunya pagar antara
siapa pun yang menemukan URL-nya dan panel yang bisa menulis config, membaca
semua prompt tersimpan, dan membuka semua client key. Jalur pemulihan apa pun
yang bisa diselesaikan dari dalam browser berarti menyerahkan panel itu ke orang
yang justru sedang dijaga password tadi.

Yang cuma dipunya pemiliknya adalah HP-nya. Jadi itu yang diminta:

1. Di layar sign-in, tekan **Forgot password?** lalu **Show me the code**.
2. Relay memunculkan **kode 6 digit di HP** — di jendela Termux tempat relay
   jalan, dan di file yang dibacakan perintah ini:

   ```bash
   chtting-relay reset-code
   ```

   Perintah itu yang dipakai kalau relay-nya dijalankan keeper: stderr-nya ke
   `/dev/null`, jadi banner-nya tidak muncul di mana pun.
3. Ketik kodenya di dashboard bersama password baru. Selesai — tinggal sign in
   dengan password yang baru.

Yang bikin 6 digit cukup bukan panjangnya, tapi jatah tebakannya:

- satu kode hidup dalam satu waktu, 10 menit, acak di seluruh sejuta;
- **5 kali salah dan kodenya dibatalkan**, bukan sekadar ditolak — jadi satu
  rangkaian tebakan dapat 5 dari sejuta, dan tebakan yang menghabiskan jatah itu
  ikut membawa pergi kode yang sedang ditebak;
- minta kode saat masih ada yang hidup akan mengembalikan kode yang sama, bukan
  mencetak yang baru, dan kode baru paling cepat 30 detik sekali. Tanpa itu,
  orang asing bisa membuat terminalmu penuh banner reset — berisik, sekaligus
  cara menyembunyikan banner yang asli.

Yang ikut terjadi begitu resetnya berhasil:

- **Semua sesi ikut keluar.** Reset juga yang orang cari kalau curiga ada sesi
  yang bukan miliknya, jadi tidak ada cookie yang selamat — termasuk cookie
  browser yang baru saja mereset.
- **Lockout sign-in dibersihkan.** Terkunci karena salah password berkali-kali
  itu separuh alasan datang ke layar ini; membuktikan diri dengan kode lalu
  disuruh menunggu 30 detik itu hitungan yang hidup lebih lama dari
  pertanyaannya sendiri.
- **Kalau dashboard-nya sedang terpublikasi** — tunnel dashboard hidup, atau
  request-nya masuk lewat nama yang bukan nama mesin ini — password barunya
  tetap harus 16 karakter, sama seperti syarat tunnel dashboard boleh menyala.
  Pintu ini tidak boleh jadi cara panel yang sedang ada di internet berakhir di
  balik empat karakter.
- Ganti password lewat Settings juga membuang kode yang masih menggantung, dan
  kode yang tertinggal di disk dihapus setiap relay start: tantangannya cuma
  pernah ada di memori proses yang mencetaknya, jadi sesudah restart file itu
  cuma kode yang tidak akan diterima siapa pun.

Filenya ada di `data/run/password-reset.json` dengan mode 0600. Itu kedengaran
seperti titik lemahnya dan sebenarnya bukan: `config/config.json` di direktori
yang sama menyimpan password dashboard-nya sendiri apa adanya, jadi apa pun yang
bisa membaca yang satu sudah memegang yang lain.

### Update & restart: `git pull`, build, lalu balik lagi

Relay di HP itu satu clone git plus satu binary hasil build dari clone itu, dan
memajukannya dulu berarti buka Termux dan mengetik tiga perintah. Sekarang tiga
perintah itu ada di balik satu tombol — di **Overview** dan di **Setup**:

```
git pull --ff-only  →  cargo build  →  exec binary yang baru
```

Ini tombol yang setara dengan `bash install.sh` dari terminal, bedanya tombol
ini tidak menyentuh paket Termux — dia pull, build, lalu restart. Paket naik
versi cuma lewat `bash install.sh`.

Ketiganya harus ada. `git pull` saja tidak mengubah apa pun yang bisa dilihat
pemanggil: HTML, CSS, dan JavaScript dashboard **ikut ter-compile ke dalam
binary**, jadi source baru di disk itu source yang tidak ada yang menjalankan.
Makanya tombolnya menarik, membangun apa yang ditarik, baru restart ke hasilnya
— itulah arti "restart" untuk relay yang sekaligus sebuah checkout.

Dua hal yang disengaja:

* **Relay tetap melayani sampai binary baru benar-benar jadi.** Pull yang gagal,
  build yang gagal, toolchain yang belum dipasang — semuanya meninggalkan relay
  yang sedang jalan persis seperti semula dan melapor kenapa. Yang menghentikan
  proses lama cuma satu: binary baru yang sudah ada di disk.
* **Binary yang sedang jalan dipindah dulu, bukan ditimpa.** Menautkan (link) ke
  file yang sedang dieksekusi gagal dengan `ETXTBSY` di Linux. Mengganti namanya
  gratis (proses yang jalan memegang inode-nya, bukan namanya), membebaskan path
  yang dipanggil skrip keeper untuk build baru, dan menyisakan sesuatu untuk
  dikembalikan kalau build-nya gagal.

Di HP prosesnya lima sampai lima belas menit, jadi request-nya dijawab
langsung dan kemajuannya diikuti lewat `GET /api/update` — log build-nya muncul
di halaman selagi jalan. Tombol **Restart** yang di sebelahnya melewati `git
pull` dan cuma menjalankan ulang binary yang sudah ada: hitungan detik, dan
tidak mengubah kode apa pun. Itu yang dipakai kalau cuma ganti port atau bind
address.

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

### PDF siap kirim

`docs/API-Documentation.pdf` adalah versi cetak dari dokumen yang sama, 13
halaman: cara request, **`user_id` — satu field, dan apa yang dia beli**,
bentuk respons beserta blok `usage`, alur frame SSE lengkap dengan keep-alive
dan frame penutup, lalu katalog model di `/v1/models`.

Dokumen itu bicara sebagai **satu layanan**. Tidak ada kata relay, backend,
upstream, provider, atau nama vendor di dalamnya — bukan cuma di kalimatnya,
tapi juga di `type` dan `code` error, di header respons, dan di field yang
dipublikasikan `/v1/models`. Kalau sebuah kalimat bikin pembaca bertanya "di
balik apa?", kalimat itu tidak boleh ada di sana. Tab **API Docs** di dashboard
sekarang menerapkan aturan yang sama, karena tombol Copy as Markdown-nya memang
dipakai buat mengirim dokumen itu ke calon integrator.

Sumbernya `docs/api-documentation.html`. Edit di situ, lalu bangun ulang:

```bash
pip install weasyprint
python3 -c "from weasyprint import HTML; \
  HTML('docs/api-documentation.html').write_pdf('docs/API-Documentation.pdf')"
```

---

## 8. Cloudflare Tunnel

Tab **Tunnel**, atau setel `tunnel.autoStart` di config.

- **quick** — URL `*.trycloudflare.com` gratis, tanpa akun Cloudflare
- **named** — hostname sendiri, pakai tunnel token dari Zero Trust
- autoStart menyalakan tunnel bersama relay dan menyambungkannya lagi kalau putus,
  dengan backoff (jaringan seluler memang sering putus)

Yang dipublikasikan tunnel ini hanya port relay. Tunnel token tidak pernah ikut
tertulis ke buffer log yang ditampilkan dashboard.

### Membuka dashboard dari perangkat lain

Ada dua cara, dan yang pertama biasanya jawabannya.

#### Satu tunnel saja: `dashboard.publishOnRelay`

Panelnya menjawab di **`<URL relay>/dashboard/`** — lewat cloudflared yang sudah
menerbitkan relay, jadi tidak ada proses kedua yang harus jalan. Di HP itu
bedanya nyata: satu cloudflared lagi berarti ~40 MB lagi dan satu URL lagi yang
harus diingat.

Saklarnya ada di tab **Tunnel**, di dalam kartu Relay tunnel — karena memang
bukan tunnel kedua yang perlu di-start/stop, cuma satu path lagi di tunnel yang
sudah hidup. Berlaku di request berikutnya: tidak ada yang restart, dan
mematikannya juga langsung. Selama belum dinyalakan — dan begitu dimatikan lagi
— path itu menjawab persis seperti path lain yang tidak ada, kata per kata, jadi
tidak ada apa pun di URL itu yang memberi tahu bahwa ada panel di sini.

Tiga hal yang tidak akan dilakukannya, apa pun isi config-nya:

- **Menjawab tanpa password yang layak.** Minimal 16 karakter, sama seperti
  syarat tunnel dashboard boleh menyala, dan alasannya sama: URL relay itu URL
  yang kamu kasih ke pemanggil, dan ini menaruh halaman login di atasnya.
- **Menjawab di alamat Wi-Fi.** `server.host` itu `0.0.0.0`, jadi port relay juga
  menjawab di alamat LAN HP-mu, lewat HTTP polos, ke semua perangkat di jaringan
  itu. Panelnya tidak disajikan di sana: cuma di nama-nama mesin ini sendiri,
  hostname tunnel relay, dan `security.dashboardAllowedHosts`. Pemeriksaan ini
  tidak lewat `dashboardOriginGuard` — yang itu boleh kamu matikan, yang ini
  bukan hakmu untuk melonggarkan.
- **Ikut menyala di `--no-dashboard`.** Itu keputusan soal run ini, dan berlaku
  untuk kedua pintu.

Panelnya sama, bukan salinan: satu kumpulan sesi, satu hitungan salah password,
satu kode reset. Sign out — atau reset password — berlaku di dua pintu sekaligus.
Cookie sesinya dibatasi ke `Path=/dashboard`, jadi tidak ikut menempel di
panggilan API yang berbagi origin yang sama.

```jsonc
"dashboard": {
  "password": "correct-horse-battery-staple",   // 16 karakter atau lebih
  "publishOnRelay": true                        // <URL relay>/dashboard/
}
```

#### Tunnel kedua: `dashboard.tunnel`

Buat kalau panelnya memang harus punya URL sendiri — yang bisa dimatikan tanpa
menurunkan relay, dan yang bukan URL yang dipegang pemanggilmu.

Dashboard punya tunnel sendiri: `dashboard.tunnel`, isinya sama persis dengan
`tunnel` di atas, tapi yang dipublikasikan adalah `dashboard.port`. Dua proses
cloudflared terpisah, dua URL terpisah, dan tidak ada konfigurasi yang bisa
membuat salah satunya mempublikasikan port milik yang lain — masing-masing
membaca portnya dari scope-nya sendiri.

Bawaannya `off`. Mempublikasikan panel kontrol itu keputusan, dan default yang
mengambil keputusan itu untukmu akan salah setiap kali.

**Tidak akan menyala sebelum `dashboard.password` minimal 16 karakter.** Di
loopback, password lemah menjaga permukaan yang cuma bisa dijangkau dari HP ini.
Di balik tunnel, password itu satu-satunya pagar: URL-nya bisa ditebak, tercatat
di setiap perantara yang dilewatinya, dan di belakangnya ada panel yang menulis
config, membaca semua prompt tersimpan, dan mengembalikan setiap client key
dalam bentuk aslinya. Dashboard memberi tahu alasannya sebelum tombol Start
ditekan, bukan sesudahnya.

Begitu request datang lewat nama publik dan bukan nama lokal — lewat tunnel yang
mana pun — empat hal berubah:

- penjaga origin menerima nama itu — lewat hostname tunnel yang sedang hidup,
  atau `security.dashboardAllowedHosts`. Penjaganya **tidak dimatikan**: daftar
  namanya bertambah satu, karena nama yang hari ini menunjuk 127.0.0.1 besok
  bisa menunjuk ke tempat lain (itulah DNS rebinding);
- cookie sesi diberi tanda `Secure` (tidak diberi di loopback http biasa, yang
  di sebagian browser justru bikin cookie-nya tidak disimpan sama sekali);
- login tanpa password ditolak langsung, apa pun keadaan tunnelnya — bisa saja
  ada hal lain yang meneruskan portnya;
- password salah dihitung per alamat pemanggil, bukan cuma satu hitungan global,
  supaya satu penyerang tidak bisa mengunci kamu dari panelmu sendiri.

Kedua tunnel ikut mati saat relay berhenti atau berotasi. Tunnel dashboard yang
dibiarkan hidup melewati proses pemiliknya akan menjaga satu URL tetap menyala
ke *penerusnya* — panel kontrol yang dipublikasikan oleh proses yang tidak
pernah menyetujuinya.

Kalau bisa, pasang Cloudflare Access di depan hostname itu. Password ini pagar
terakhir, bukan satu-satunya yang boleh ada.

Tombol **Stop** dan **Restart** di tab itu benar-benar menghentikan cloudflared.
Dulu tidak: proses pengawasnya memegang mutex penjaga child process selama
`child.wait()`, jadi Stop menunggu kunci yang baru dilepas kalau cloudflared
sudah mati duluan dengan sendirinya — persis hal yang mau dibikin terjadi oleh
tombol itu. Sekarang child-nya dimiliki task pengawas itu sendiri dan Stop
mengirim sinyal lewat channel, lalu menunggu sampai prosesnya benar-benar habis
sebelum Restart mengikat yang baru.

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
`architecture`, `pricing`, `supported_parameters`, `reasoning`. Klien OpenAI
lama tetap jalan; klien yang mau tahu harga tidak perlu bertanya ke siapa pun.

Harganya diumumkan **ketiga-tiganya**, bukan cuma yang tengah. Model-model ini
tidak dijual satu harga: tarifnya ikut seberapa keras si pemanggil menyuruh
model berpikir. `pricing.bands` berisi `non_thinking`, `default`, dan `max` —
urut dari yang termurah, masing-masing dengan nama, daftar `efforts` yang
mendaratkan request ke situ, dan **harga utuh**, bukan cuma angka yang berubah.
`pricing.default_band` menyebut band mana yang kena kalau request tidak
menyertakan `reasoning_effort` sama sekali. Di sebelahnya `pricing.overrides`
tetap mengurus sisi yang lain: tarif yang bergerak per jam dan per hari.

Yang tidak pernah diisi tidak ikut diumumkan. `description` kosong,
`context_length` `0`, `max_completion_tokens` `null`, `default_parameters` `{}`
— semuanya dihilangkan, bukan dikirim sebagai blanko yang harus di-*special
case* klien. `0` malah lebih buruk daripada diam: terbaca sebagai model yang
tidak punya ruang sama sekali.

---

## CLI

Semua ini juga ada di tab **Setup** dashboard. CLI-nya dipertahankan buat
scripting dan buat kalau dashboard-nya sendiri yang bermasalah.

```
chtting-relay start [--port N] [--no-dashboard] [--replace]
chtting-relay setup                     keeper + shortcut + hook boot
chtting-relay doctor                    periksa lingkungan dan konfigurasi
chtting-relay reset-code                kode 6 digit buat reset password dashboard
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
  reset.rs       kode 6 digit di HP buat reset password dashboard
  rotate.rs      ganti instance tiap jam tanpa memutus koneksi
  server/        API publik, dashboard + admin API
  store/         SQLite, buku besar, invoice, quota tracker, rate limiter
  system.rs      keeper, shortcut, hook boot, wake lock, restart/stop
  tunnel.rs      supervisor cloudflared
  update.rs      git pull + build + restart ke binary yang baru
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
- **Reset password dashboard butuh HP-nya, bukan email.** Kode 6 digitnya cuma
  muncul di perangkat ini — jendela Termux dan `chtting-relay reset-code` — dan
  yang diterima browser cuma nama tantangannya, tidak pernah kodenya. Lima kali
  salah membatalkan kodenya, resetnya mengeluarkan semua sesi, dan kalau
  panelnya sedang terpublikasi password barunya tetap wajib 16 karakter.
- **Dashboard yang dipublikasikan tidak menyala tanpa password 16 karakter.**
  Berlaku untuk dua-duanya: tunnel dashboard yang terpisah, dan
  `dashboard.publishOnRelay` yang menempelkan panel di `/dashboard` pada port
  relay. Lihat bagian Cloudflare Tunnel di atas untuk apa saja yang berubah
  begitu panel ini bisa dijangkau dari luar HP.
- **Panel di port relay cuma menjawab di nama tempat ia diterbitkan.** Port relay
  mendengarkan di `0.0.0.0`, jadi ia juga menjawab di alamat Wi-Fi HP lewat HTTP
  polos — `/dashboard` tidak disajikan di sana. Yang dijawab cuma nama mesin ini
  sendiri, hostname tunnel relay, dan `dashboardAllowedHosts`; pemeriksaan itu
  tidak ikut mati kalau `dashboardOriginGuard` dimatikan. Kalau belum
  diterbitkan, path-nya menjawab 404 yang sama persis dengan path tak dikenal
  mana pun.
- Setiap respons dashboard membawa CSP (`connect-src 'self'`,
  `frame-ancestors 'none'`, `form-action 'none'`), `nosniff`, `no-referrer`, dan
  `X-Frame-Options: DENY`. Respons API juga `no-store` — isinya config, key, dan
  prompt, jadi tidak boleh ada cache yang menyimpannya. Asetnya tidak, karena
  tidak membawa rahasia apa pun dan melarangnya di-cache berarti mengirim ulang
  seluruh frontend lewat tunnel setiap kali halaman dibuka.
- `tokenizer/install` menerima URL dari pemanggil, jadi itu alat untuk menyuruh
  relay mengambil sesuatu. Loopback, alamat link-local (`169.254.169.254`, tempat
  instance cloud menyimpan kredensialnya), dan skema selain http/https ditolak —
  dicek dari alamat literal maupun setelah hostname-nya diresolusi. Mirror
  kosakata di jaringanmu sendiri tetap jalan.
- **Trace ke Langfuse tidak pernah membawa kredensial.** Private key yang
  `privateUserId`-nya `secret` dikirim sebagai sidik jari, dan field yang bisa
  berisi client key (`user`, `user_id`, `api_key`, …) dibuang dari setiap body.
  Semua pengiriman trace di luar jalur request: Langfuse yang mati atau salah
  setelan tidak bisa memperlambat atau menggagalkan satu pun panggilan.
- **Instruksi pembayaran di invoice dibekukan saat invoice terbit.** Alamat USDT,
  jaringannya, kurs, dan tanggal jatuh tempo disalin ke invoice, tidak dibaca
  ulang dari config. Mengganti dompet tidak boleh diam-diam mengalihkan invoice
  yang masih dipegang pelanggan, dan semuanya ikut masuk content hash — alamat
  yang diubah lewat belakang SQLite sama kelihatannya dengan total yang diubah.

## Lisensi

MIT
