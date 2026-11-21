# AGENTS.md — PHS Downloader

- Owner: Adit (`644438052`). Bot bersifat pribadi (owner-only), tanpa fitur scraping/pencarian situs.
- Build binary Rust dilakukan di GitHub Actions (`.github/workflows/build.yml`), bukan compile di `homeserver vio`.
- Credential disimpan di `~/secrets/apps/phs-downloader/runtime.env` (jangan pernah commit ke git).
- Jangan restart `phs-downloader.service` tanpa izin eksplisit Adit.
- Identitas Git wajib: `Adit <83448385+pratamax00@users.noreply.github.com>`.
