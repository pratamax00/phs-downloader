# AGENTS.md — PHS Downloader

- Bot bersifat pribadi (owner-only via `OWNER_ID`), tanpa fitur scraping atau pencarian situs.
- Semua credential, chat ID, domain, dan IP pribadi wajib disuplai lewat environment variable (`runtime.env`), **jangan pernah** di-hardcode atau di-commit ke repo.
- Build static binary dilakukan lewat GitHub Actions (`.github/workflows/build.yml`).
- Standar UI mengikuti Bot API 10.3+ (`sendRichMessage` / `editMessageText` in-place dengan fallback HTML).
