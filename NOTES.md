# Ringkasan Arsitektur Kamui

## Gambaran umum

Kamui adalah CLI chat LLM berbasis Rust yang bersifat **provider-agnostic** dan sadar konteks repository. Aplikasi memisahkan antarmuka chat, integrasi provider, pengambilan konteks project, eksekusi tools, dan persistence agar provider baru dapat ditambahkan tanpa mengubah alur utama aplikasi.

```text
Terminal pengguna
      |
      v
  src/main.rs
      |
      v
  src/chat.rs  <-------------------->  src/storage.rs (SQLite)
      |
      +----> src/context.rs (instruksi project, @file, @diff, @staged)
      |
      +----> src/tools.rs (read, list, command, patch)
      |
      v
Provider trait (src/provider/mod.rs)
      |
      v
OpenAI adapter (src/provider/openai.rs)
      |
      v
OpenAI-compatible Chat Completions API
```

## Modul utama

### `src/main.rs`

Entry point dan composition root aplikasi. Modul ini menangani:

- pemuatan konfigurasi dari environment, `.env` lokal, dan `.env` global;
- parsing argumen CLI;
- pembuatan provider, storage, context, dan tool registry;
- pemilihan session yang akan dilanjutkan;
- startup loop chat.

### `src/chat.rs`

Pusat orkestrasi aplikasi dan antarmuka terminal. Tanggung jawab utamanya:

- membaca input pengguna dan menjalankan command seperti `/sessions`, `/resume`, dan `/stats`;
- mengirim request dan menampilkan respons streaming;
- menjalankan agent loop ketika model meminta tool;
- meminta konfirmasi sebelum tool yang dapat mengubah sistem dijalankan;
- membuat judul session setelah percakapan pertama;
- menyimpan turn yang berhasil secara atomik;
- menangani shutdown dengan aman.

Agent loop dibatasi jumlah putarannya. Jika provider mengembalikan tool call, chat loop mengeksekusi tool terkait, menambahkan hasilnya sebagai message, lalu meminta respons lanjutan sampai model memberikan jawaban biasa.

### `src/provider/mod.rs`

Lapisan abstraksi provider. Berisi tipe domain yang tidak bergantung pada API tertentu, antara lain:

- `Provider` trait;
- request dan response chat;
- message dan event streaming;
- usage/token metadata;
- definisi dan pemanggilan tool.

Pemisahan ini mencegah struktur khusus OpenAI bocor ke chat loop, storage, context, atau tools.

### `src/provider/openai.rs`

Adapter untuk OpenAI-compatible Chat Completions API. Modul ini bertugas:

- memetakan tipe domain ke payload HTTP OpenAI;
- mengirim request menggunakan `reqwest`;
- memproses respons SSE streaming;
- merakit delta konten dan tool call yang datang bertahap;
- mengubah hasil API kembali ke tipe provider-agnostic.

Layanan seperti OpenRouter, Ollama, LM Studio, Groq, DeepSeek, atau LiteLLM dapat digunakan selama menyediakan API yang kompatibel dengan OpenAI.

### `src/context.rs`

Lapisan konteks repository dan keamanan path. Fitur utamanya:

- membaca `KAMUI.md`, atau `AGENTS.md` sebagai fallback, dari project root;
- mengembangkan referensi `@path` menjadi isi file UTF-8;
- mengambil perubahan Git melalui `@diff` dan `@staged`;
- memastikan path relatif tetap berada di dalam project root;
- menolak absolute path, directory, file non-UTF-8, dan symlink/path yang keluar dari root;
- menerapkan batas ukuran file dan total attachment.

Isi attachment hanya digunakan pada request aktif. History menyimpan prompt asli, bukan hasil ekspansinya.

### `src/tools.rs`

Runtime tool provider-agnostic. Komponen utamanya adalah:

- trait async `Tool`;
- `ToolRegistry` untuk menyimpan definisi dan melakukan dispatch berdasarkan nama;
- `list_directory` dan `read_file` sebagai tool read-only;
- `run_command` untuk menjalankan command dengan timeout dan batas output;
- `patch_file` untuk membuat atau mengubah satu file melalui exact-match replacement.

`run_command` dan `patch_file` memerlukan persetujuan pengguna. Policy konfirmasi dikendalikan oleh Kamui/chat loop, bukan oleh model. Jika tersedia, binary RTK dapat digunakan untuk mengompresi output command; eksekusi langsung tetap menjadi fallback.

### `src/storage.rs`

Lapisan persistence berbasis SQLite. Menangani:

- schema dan migrasi melalui `PRAGMA user_version`;
- session, message, dan usage;
- pencarian, rename, resume, dan penghapusan session;
- penyimpanan tool request serta tool result;
- transaksi atomik untuk satu turn percakapan;
- foreign key dan cascade deletion.

SQLite dibundel melalui fitur `rusqlite/bundled`. Lokasi data mengikuti application data directory OS dan dapat dioverride dengan `KAMUI_DATA_DIR`.

## Alur request utama

1. `main.rs` memuat konfigurasi dan menyusun dependency.
2. `chat.rs` menerima prompt pengguna.
3. `context.rs` memuat instruksi project dan mengembangkan attachment yang direferensikan.
4. Chat loop membentuk request menggunakan tipe netral dari `provider/mod.rs`.
5. Adapter `openai.rs` mengirim request dan menerjemahkan SSE menjadi event internal.
6. Delta teks langsung ditampilkan di terminal.
7. Jika model meminta tool, `ToolRegistry` melakukan dispatch setelah konfirmasi bila diperlukan, lalu hasilnya dikirim kembali ke model.
8. Setelah jawaban final berhasil, seluruh turn disimpan oleh `storage.rs`. Respons parsial yang gagal atau terinterupsi tidak dianggap sebagai turn selesai.

## Batas keamanan penting

- Semua akses file dibatasi ke project root.
- File reference dibatasi 64 KiB per file dan 128 KiB secara total.
- Command memerlukan konfirmasi, berjalan tanpa stdin, memiliki timeout 30 detik, dan output dibatasi.
- Patch memerlukan konfirmasi dan `old_text` harus cocok tepat satu kali.
- Penulisan file dilakukan secara atomik.
- Tool loop memiliki batas putaran agar tidak berjalan tanpa akhir.
- Secret dan API key hanya berasal dari konfigurasi; `.env` tidak boleh masuk version control.

## Konfigurasi dan dependency utama

Konfigurasi penting:

- `OPENAI_API_KEY`
- `OPENAI_BASE_URL`
- `OPENAI_MODEL`
- `KAMUI_CONTEXT_WINDOW`
- `KAMUI_DATA_DIR`

Dependency utama:

- `tokio` untuk runtime async;
- `reqwest` untuk HTTP dan streaming;
- `serde`/`serde_json` untuk serialisasi;
- `rusqlite` untuk persistence SQLite;
- `async-trait` untuk abstraksi provider dan tool async;
- `dotenvy` dan `directories` untuk konfigurasi lintas platform;
- `anyhow` untuk propagasi error.

## Prinsip desain

- Core harus tetap provider-agnostic.
- Detail wire protocol hanya berada di adapter provider.
- Operasi filesystem dan subprocess dianggap sebagai input berisiko.
- Mutasi membutuhkan persetujuan eksplisit pengguna.
- Session dan riwayat tool harus dapat dipulihkan secara konsisten.
- Perubahan diutamakan kecil, lintas platform, dan tidak merusak data lama.
