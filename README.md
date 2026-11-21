# PHS Downloader

Bot Telegram pribadi (owner-only) untuk mengunduh file Google Drive (`gdrive-dl`) dan Pixeldrain ke library media Jellyfin di `homeserver vio`.

## Arsitektur & Build

- **Runtime utama (baru):** Rust static binary (`x86_64-unknown-linux-musl`, `ureq` + `rustls`, IPv4-first, zero-tokio).
- **Build sandbox:** GitHub Actions (`.github/workflows/build.yml`) — tidak membutuhkan toolchain Rust di `homeserver vio`.
- **Fallback lama:** `bot.py` (Python 3 stdlib).

## Variabel Environment

Disimpan di `~/secrets/apps/phs-downloader/runtime.env`:

- `TG_TOKEN` (wajib)
- `OWNER_ID` (default `644438052`)
- `STATE_FILE` (default `/home/pratamax/apps/bots/phs-downloader/state.json`)
- `GDRIVE_DL` (default `/home/pratamax/.local/bin/gdrive-dl`)
- `JELLYFIN_URL`, `JELLYFIN_TOKEN`
- `LINK_FILEBROWSER`, `LINK_JELLYFIN_LAN`, `LINK_JELLYFIN_TS`, `SERVER_IP`

## Verifikasi Binary

```bash
./bin/phs-downloader --version
set -a && source ~/secrets/apps/phs-downloader/runtime.env && set +a && ./bin/phs-downloader --check
```
