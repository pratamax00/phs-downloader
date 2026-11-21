# PHS Downloader

Bot Telegram ringan (owner-only) berbasis Rust static binary untuk mengunduh file Google Drive (`gdrive-dl`) dan Pixeldrain ke library media Jellyfin, dengan UI Bot API 10.3+ (rich message, in-place edit, tabel GFM, `<details>`, dan native `<tg-button>`).

## Fitur

- **Ultra-ringan:** ~2,5 MB RSS (`ureq` + `rustls`, IPv4-first, zero-tokio, `x86_64-unknown-linux-musl`).
- **UI Bot API 10.3+:** `sendRichMessage` & `editMessageText` in-place (klik tombol mengedit pesan lama, bukan spam bubble baru), highlight `==status==`, tabel GFM kanonik `|:---|:---|`, blok `<details>`, tombol native `<tg-button>`, dan fallback HTML otomatis.
- **Download engine:** Google Drive via `gdrive-dl` & Pixeldrain (`/u/<id>`) via `curl -C -`, timeout 6 jam, watcher progres tiap +100 MB (edit in-place), verifikasi file non-HTML, auto `chmod 664`, dan trigger scan library Jellyfin.
- **Build otomatis:** GitHub Actions merilis static binary ke GitHub Releases (`latest`).

## Variabel Environment

| Variabel | Wajib | Keterangan |
| :--- | :--- | :--- |
| `TG_TOKEN` | Ya | Token bot Telegram |
| `OWNER_ID` | Ya | Chat ID Telegram pemilik bot |
| `STATE_FILE` | Tidak | Path file state JSON (default `./state.json`) |
| `GDRIVE_DL` | Tidak | Path executable `gdrive-dl` (default `gdrive-dl`) |
| `DIR_FILMS` / `DIR_SERIES` / `DIR_ANIME` | Tidak | Path folder library media |
| `JELLYFIN_URL` / `JELLYFIN_TOKEN` | Tidak | Endpoint & token API Jellyfin untuk `/Library/Refresh` |
| `LINK_FILEBROWSER` | Tidak | URL layanan Filebrowser (opsional, tampil di menu Link Akses) |
| `LINK_JELLYFIN_LAN` / `LINK_JELLYFIN_TS` | Tidak | URL layanan Jellyfin LAN / Tailscale (opsional) |
| `SERVER_IP` | Tidak | Info IP server lokal (opsional) |

## Verifikasi Binary

```bash
./bin/phs-downloader --version
set -a && source /path/to/runtime.env && set +a && ./bin/phs-downloader --check
```
