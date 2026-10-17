# NBS Download Manager

Download manager desktop ala Internet Download Manager, dibangun dengan **Rust + Tauri 2**.

## Fitur

| Fitur | Implementasi |
| --- | --- |
| **Multi-threaded / segmented downloading (1–8 koneksi)** | File dibagi menjadi beberapa rentang byte (HTTP `Range`), tiap koneksi menulis ke part file sendiri. Koneksi yang selesai lebih dulu akan **membelah segmen terbesar yang tersisa** (dynamic segmentation) sehingga semua koneksi tetap sibuk sampai akhir. |
| **Pause & resume** | Progres tiap segmen disimpan ke `downloads.json`. Saat resume, part file dicek ulang terhadap progres yang tersimpan, lalu dilanjutkan dengan `Range` dari offset terakhir — juga setelah aplikasi ditutup. ETag/ukuran dicek agar file yang berubah di server diunduh ulang. |
| **Dynamic file merging** | Segmen yang sudah selesai dan berurutan langsung digabung ke file output **selama download masih berjalan**, sehingga di akhir hampir tidak ada yang perlu digabung. |
| **Real-time progress & speed** | Kecepatan (smoothed), ETA, progress per segmen, grafik kecepatan total, dikirim ke UI 2× per detik. |
| **Speed limiter** | Token bucket async: batas global (semua download) dan batas per download, bisa diubah saat berjalan. |
| **Download torrent** | Engine BitTorrent [librqbit](https://github.com/ikatson/rqbit) (diambil dari [Torrent-Stream](https://github.com/wawanferd/Torrent-Stream)). Tambah dari magnet link, info-hash, link `.torrent`, atau file `.torrent` (tombol *Torrent*, `Ctrl+T`, tempel di kotak link, drag & drop, atau dibuka dari sistem). Metadata diambil dulu sehingga file bisa dipilih; file bisa ditambah/dilewati kapan saja. Pause/resume tetap tersimpan setelah aplikasi ditutup. |
| **Stream torrent** | Mode *Stream* menyimpan torrent di folder cache dan memutarnya di pemutar bawaan selagi masih terunduh: file dilayani server HTTP lokal (`127.0.0.1`, mendukung `Range`), dan piece di sekitar posisi tonton diunduh lebih dulu sehingga seek tetap bisa. Subtitle `.srt/.vtt` di dalam torrent dipasang otomatis. Format yang tidak didukung webview (mis. MKV HEVC/AC3) bisa dibuka di pemutar eksternal (VLC, mpv, atau pemutar default sistem) tanpa menunggu unduhan selesai. *Keep* memindahkan hasil stream ke folder unduhan. |
| **Seeding** | Mode unggah: *Keep seeding*, *Stop seeding* (torrent dijeda saat selesai, tetap bisa diputar), atau *Never upload*. Batas kecepatan unduh/unggah torrent terpisah dari batas global HTTP. |
| **Integrasi browser** | Ekstensi Chrome/Edge/Brave/Firefox (Manifest V3) menangkap download dan mengirimnya ke aplikasi lewat **Local WebSocket Server** (`ws://127.0.0.1:18765`), dengan fallback **Native Messaging** (host `nbs-native-host`, yang juga menjalankan aplikasi bila belum terbuka). Cookie, referrer dan user-agent browser ikut diteruskan sehingga link yang butuh login tetap bisa diunduh. |

Lainnya: antrean dengan batas download simultan, tray icon, notifikasi selesai, tema terang/gelap, menu klik kanan "Download with NBS Download Manager" (juga untuk magnet link). Download `.torrent` dan magnet link yang ditangkap ekstensi otomatis masuk ke dialog torrent.

## Struktur

```
crates/nbs-core/    Engine download (Rust murni, tanpa GUI) + tes
crates/nbs-torrent/ Engine torrent (librqbit): download, stream HTTP lokal, seeding + tes
src-tauri/          Aplikasi Tauri: perintah, WebSocket bridge, tray, settings, torrent
third_party/        librqbit 9.0.1 dengan patch dari Torrent-Stream (file sparse di Windows)
native-host/        Native Messaging host (relay stdin/stdout ⇄ WebSocket)
ui/                 Frontend (HTML/CSS/JS tanpa build step)
extension/          Ekstensi browser (satu folder untuk Chrome & Firefox)
```

## Build & jalankan

Prasyarat: Rust stable, Node.js 18+, dan [dependensi sistem Tauri](https://v2.tauri.app/start/prerequisites/)
(Linux: `libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev`).

```bash
npm install
npm run dev      # build native host + jalankan aplikasi dalam mode dev
npm run build    # installer release (di target/release/bundle), native host ikut dibundel
cargo test       # tes engine (segmentasi, pause/resume, limiter, merging, antrean, torrent)
```

`npm run build` memakai `src-tauri/tauri.bundle.conf.json`: sebelum compile, `scripts/prepare-native-host.mjs` membangun
`nbs-native-host` lalu menaruhnya di `src-tauri/binaries/` agar ikut terpasang di samping aplikasi. `cargo build`/`cargo test`
biasa tidak membutuhkannya.

### Build release Windows

Di Windows cukup `npm run build` — hasilnya installer NSIS (`target/release/bundle/nsis/*-setup.exe`, per-user tanpa hak admin)
dan MSI (`target/release/bundle/msi/*.msi`).

Dari Linux/macOS bisa cross-compile installer NSIS dengan `./scripts/build-windows.sh` (perlu
`rustup target add x86_64-pc-windows-msvc`, `cargo install --locked cargo-xwin`, serta `nsis`, `lld`, `llvm`, `clang`);
hasilnya di `target/x86_64-pc-windows-msvc/release/bundle/nsis/`.

Workflow GitHub Actions `.github/workflows/windows-release.yml` membuat installer NSIS + MSI di runner Windows (jalankan manual
dari tab *Actions*, atau push tag `v*` untuk membuat draft release berisi installer).

## Memasang ekstensi browser

- **Chrome / Edge / Brave**: buka `chrome://extensions`, aktifkan *Developer mode*, klik *Load unpacked*, pilih folder `extension/`.
  ID ekstensi selalu `flikhhghfebchbjchkfcdicnjgidonkd` (dikunci lewat field `key`).
- **Firefox (121+)**: buka `about:debugging#/runtime/this-firefox` → *Load Temporary Add-on* → pilih `extension/manifest.json`.
  Di Firefox, izinkan akses situs dari popup ekstensi agar cookie ikut terkirim.

Selama aplikasi berjalan, ikon ekstensi tanpa badge berarti terhubung; badge `!` berarti aplikasi tidak berjalan (download tetap ditangani browser).

### Native Messaging (opsional)

Di aplikasi: **Settings → Browser integration → Native messaging host**, klik browser yang dipakai. Ini menulis manifest host
(dan registry di Windows) yang menunjuk ke `nbs-native-host` di folder yang sama dengan aplikasi. Setelah itu ekstensi bisa
menjalankan aplikasi otomatis ketika ada download yang ditangkap.

## Torrent

- **Tambah torrent**: tombol *Torrent* / `Ctrl+T`, tempel magnet di kotak link, drag & drop file `.torrent` ke jendela, atau
  klik link magnet / file `.torrent` di sistem. Pilih file, lalu **Download** (ke folder unduhan torrent) atau **Stream**
  (memutar video terbesar yang dipilih).
- **Pemutar bawaan** memakai `<video>` dari webview (WebView2 di Windows, WebKitGTK di Linux): MP4/H.264, WebM/VP9 dan
  sejenisnya bisa langsung diputar. `←/→` ±10 detik, `Spasi` putar/jeda, `F` layar penuh. Untuk format lain gunakan
  **Open in player**: stream diteruskan ke pemutar yang diatur di *Options → Torrents* (kosong = pemutar default sistem
  lewat playlist `.m3u`).
- **Options → Torrents**: folder unduhan torrent, folder cache stream, batas kecepatan, mode unggah, port listen, pemutar
  eksternal, dan tombol *Open magnet links with NBS* (Windows: registry `HKCU`, Linux: `xdg-mime`). Installer juga
  mendaftarkan asosiasi file `.torrent`.
- Data sesi torrent disimpan di folder data aplikasi (`torrents/`), sehingga torrent dan progresnya kembali saat aplikasi
  dibuka ulang. Port listen, folder cache stream, dan mode *Never upload* berlaku setelah restart.
- `third_party/librqbit` adalah librqbit 9.0.1 dengan patch dari Torrent-Stream (lihat `TRNTSTREAM_PATCH.md`): file
  torrent dibuat *sparse* di Windows sehingga stream film besar tidak langsung memakan ukuran penuhnya di disk.
- Gunakan hanya untuk konten yang boleh Anda unduh dan bagikan.

## Keamanan

WebSocket server hanya listen di `127.0.0.1` dan menolak koneksi dengan header `Origin` dari halaman web (hanya
`chrome-extension://`, `moz-extension://` atau tanpa Origin seperti native host yang diterima), sehingga situs web tidak bisa
menyuruh aplikasi mengunduh sesuatu.

Server stream torrent hanya listen di `127.0.0.1` dan bersifat read-only (tidak bisa menambah atau mengubah torrent).
