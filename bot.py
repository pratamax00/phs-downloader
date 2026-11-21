#!/usr/bin/env python3
"""PHS Downloader — unduh file Google Drive ke library media lewat Telegram.

Bot ini membungkus `gdrive-dl` dengan antarmuka tombol: pilih folder tujuan,
kirim link Drive, unduhan berjalan di background, selesai → notifikasi.

Owner-only. Tidak ada fitur pencarian atau scraping situs apa pun.
"""
from __future__ import annotations

import json
import logging
import os
import re
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# ── konfigurasi ────────────────────────────────────────────────────────────
TG_TOKEN = os.environ["TG_TOKEN"]
OWNER_ID = int(os.environ.get("OWNER_ID", "644438052"))
STATE_FILE = Path(os.environ.get("STATE_FILE", "/home/pratamax/apps/bots/phs-downloader/state.json"))
GDRIVE_DL = os.environ.get("GDRIVE_DL", "/home/pratamax/.local/bin/gdrive-dl")
PIXELDRAIN_API = "https://pixeldrain.com/api/file"
JELLYFIN_URL = os.environ.get("JELLYFIN_URL", "http://127.0.0.1:8096")
JELLYFIN_TOKEN = os.environ.get("JELLYFIN_TOKEN", "")

# folder tujuan: label -> path
TARGETS = {
    "films":  "/mnt/storage/media/films",
    "series": "/mnt/storage/media/series",
    "anime":  "/mnt/storage/media/anime",
}
LABEL = {"films": "🎬 Film", "series": "📺 Series", "anime": "🌸 Anime"}

# URL akses layanan — bisa dioverride lewat runtime.env
LINKS = {
    "Filebrowser":   os.environ.get("LINK_FILEBROWSER", "https://files.pratamax.web.id"),
    "Jellyfin LAN":  os.environ.get("LINK_JELLYFIN_LAN", "http://192.168.31.254:8096"),
    "Jellyfin Tailscale": os.environ.get("LINK_JELLYFIN_TS", "http://100.99.15.68:8096"),
}
SERVER_IP = os.environ.get("SERVER_IP", "192.168.31.254")

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("phs-downloader")

# ── Telegram helpers ───────────────────────────────────────────────────────
def tg(method: str, payload: dict | None = None, timeout: int = 40) -> dict:
    url = f"https://api.telegram.org/bot{TG_TOKEN}/{method}"
    data = json.dumps(payload or {}).encode()
    req = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read().decode()
        return json.loads(raw) if raw else {}


def send(text: str, keyboard: dict | None = None) -> dict:
    """Kirim sebagai rich message (tabel/format asli). Fallback ke teks biasa kalau ditolak."""
    payload = {
        "chat_id": OWNER_ID,
        "rich_message": {"markdown": text},
        "disable_web_page_preview": True,
    }
    if keyboard:
        payload["reply_markup"] = keyboard
    try:
        return tg("sendRichMessage", payload)
    except Exception as e:
        log.warning("sendRichMessage gagal (%s), fallback ke sendMessage", e)
        # buang penanda markdown supaya tidak tampil mentah
        plain = re.sub(r"\*\*(.+?)\*\*", r"\1", text)
        plain = re.sub(r"`(.+?)`", r"\1", plain)
        plain = re.sub(r"^_|_$", "", plain, flags=re.M)
        fallback = {
            "chat_id": OWNER_ID,
            "text": plain,
            "disable_web_page_preview": True,
        }
        if keyboard:
            fallback["reply_markup"] = keyboard
        try:
            return tg("sendMessage", fallback)
        except Exception as e2:
            log.error("sendMessage juga gagal: %s", e2)
            return {}


def ack(cb_id: str, text: str = "") -> None:
    try:
        p = {"callback_query_id": cb_id}
        if text:
            p["text"] = text
        tg("answerCallbackQuery", p, timeout=15)
    except Exception:
        pass


def rows(*buttons):
    return [list(buttons[i : i + 2]) for i in range(0, len(buttons), 2)]


def button(text, data: str, style: str = ""):
    b = {"text": text, "callback_data": data}
    if style in ("primary", "success", "danger"):
        b["style"] = style
    return b


# ── state ──────────────────────────────────────────────────────────────────
def load_state() -> dict:
    try:
        return json.loads(STATE_FILE.read_text())
    except Exception:
        return {"offset": 0, "folder": None, "pending": None}


def save_state(s: dict) -> None:
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE_FILE.with_suffix(".tmp")
    tmp.write_text(json.dumps(s, indent=2))
    tmp.replace(STATE_FILE)


STATE = load_state()
BUSY: set[str] = set()   # folder yang sedang dipakai unduh


# ── ekstrak ID Drive ───────────────────────────────────────────────────────
def drive_id(s: str) -> str | None:
    s = s.strip()
    if re.fullmatch(r"[A-Za-z0-9_-]{20,}", s):
        return s
    m = re.search(r"(?:/d/|id=)([A-Za-z0-9_-]{20,})", s)
    return m.group(1) if m else None


def pixeldrain_id(s: str):
    """Kenali link Pixeldrain. Balik (kind, id): kind 'u' file, 'l' list. None kalau bukan."""
    s = s.strip()
    m = re.search(r"pixeldrain\.com/([ul])/([A-Za-z0-9]+)", s)
    return (m.group(1), m.group(2)) if m else None


def pixeldrain_info(fid: str) -> dict:
    req = urllib.request.Request(
        f"{PIXELDRAIN_API}/{fid}/info", headers={"User-Agent": "Mozilla/5.0"}
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read().decode())


def human(n: float) -> str:
    for u in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024:
            return f"{n:.1f} {u}"
        n /= 1024
    return f"{n:.1f} PB"


# ── keyboard ───────────────────────────────────────────────────────────────
def kb_menu():
    return {"inline_keyboard": rows(
        button(LABEL["films"], "pick:films", "primary"),
        button(LABEL["series"], "pick:series", "primary"),
        button(LABEL["anime"], "pick:anime", "primary"),
        button("🔗 Link Akses", "act:links"),
    )}


def kb_confirm(folder: str):
    return {"inline_keyboard": [
        [button(f"✅ Unduh ke {LABEL[folder]}", "go:yes", "success"),
         button("❌ Batal", "go:no", "danger")],
    ]}


def kb_after():
    return {"inline_keyboard": rows(
        button("🔄 Scan Jellyfin", "act:scan", "primary"),
        button("🔗 Link Akses", "act:links"),
        button("📁 Menu", "act:menu"),
    )}


def links_text() -> str:
    """Tampilkan link akses layanan media."""
    lines = ["**🔗 Link Akses**\n", "| Layanan | URL |", "|---|---|"]
    for nama, url in LINKS.items():
        lines.append(f"| {nama} | {url} |")

    lines.append("")
    lines.append("**Folder library**")
    lines.append("| Folder | Path |")
    lines.append("|---|---|")
    for k, p in TARGETS.items():
        lines.append(f"| {LABEL[k]} | `{p}` |")

    lines.append("")
    lines.append("**Catatan**")
    lines.append(f"- Filebrowser bisa dibuka dari mana saja (Cloudflare tunnel).")
    lines.append(f"- Jellyfin **LAN** hanya di jaringan lokal; **Tailscale** kalau di luar rumah.")
    lines.append(f"- Server: `{SERVER_IP}`")
    return "\n".join(lines)


def kb_links():
    return {"inline_keyboard": [
        [button("📁 Menu", "act:menu")],
    ]}


def menu_text() -> str:
    usage = []
    for k, p in TARGETS.items():
        n = 0
        try:
            n = sum(1 for _ in Path(p).iterdir())
        except Exception:
            pass
        usage.append(f"| {LABEL[k]} | `{p}` | {n} item |")
    return (
        "**PHS Downloader**\n"
        "*Unduh file Google Drive / Pixeldrain ke library media*\n\n"
        "| Folder | Path | Isi |\n|---|---|---|\n" + "\n".join(usage) + "\n\n"
        "Pilih folder tujuan, lalu kirim link Google Drive."
    )


# ── unduhan ────────────────────────────────────────────────────────────────
def looks_complete(path: Path) -> tuple[bool, str]:
    """Nilai hasil unduhan dari FILENYA, bukan cuma exit code.

    curl bisa keluar non-zero (18/56/28) setelah byte terakhir diterima — koneksi
    ditutup di akhir. Kalau file ada, berukuran, dan bukan halaman HTML, hasilnya
    dianggap lengkap.
    """
    try:
        size = path.stat().st_size
    except OSError:
        return False, "file tidak ada"
    if size == 0:
        return False, "file 0 byte"
    try:
        head = path.open("rb").read(512).lstrip().lower()
    except OSError:
        head = b""
    if head.startswith(b"<!doctype html") or head.startswith(b"<html") or b"<head" in head[:200]:
        return False, "isinya halaman HTML"
    return True, f"{size} byte"


def run_download(folder: str, url: str) -> None:
    dest = Path(TARGETS[folder])
    BUSY.add(folder)
    send(f"⬇️ **Mulai mengunduh**\n\n"
         f"Folder : {LABEL[folder]}\n"
         f"Tujuan : `{dest}`\n\n"
         f"Kalau file besar, ini bisa lama. Violet kabari kalau selesai.")

    stop = threading.Event()
    seen_max = {"v": 0.0}

    def watch():
        """Pantau ukuran file baru supaya bisa lapor progres."""
        start = {p.name for p in dest.iterdir()} if dest.exists() else set()
        while not stop.wait(60):
            try:
                new = [p for p in dest.iterdir() if p.name not in start and p.is_file()
                       and not p.name.startswith(".")]
                if not new:
                    continue
                f = max(new, key=lambda p: p.stat().st_size)
                sz = f.stat().st_size / 1024 / 1024
                if sz - seen_max["v"] >= 100:          # kabari tiap +100 MB
                    seen_max["v"] = sz
                    send(f"📥 `{f.name}`\n{sz:,.0f} MB terunduh…")
            except Exception:
                pass

    watcher = threading.Thread(target=watch, daemon=True)
    watcher.start()

    try:
            # rekam kondisi awal: nama -> (ukuran, mtime) supaya file yang DI-RESUME
            # (nama sama, ukuran berubah) tetap terdeteksi sebagai hasil unduhan.
            def snapshot():
                snap = {}
                if dest.exists():
                    for p in dest.iterdir():
                        if p.is_file() and not p.name.startswith("."):
                            st = p.stat()
                            snap[p.name] = (st.st_size, st.st_mtime)
                return snap

            before = snapshot()
            r = subprocess.run(
                [GDRIVE_DL, url, "-o", str(dest)],
                capture_output=True, text=True, timeout=6 * 3600,
            )
            after = snapshot()

            # file baru ATAU file yang ukurannya/mtime-nya berubah
            changed = [
                n for n, meta in after.items()
                if n not in before or before[n] != meta
            ]
            # pilih yang paling besar (hindari file kecil seperti log)
            fname = max(changed, key=lambda n: after[n][0]) if changed else None

            ok_file, reason = looks_complete(dest / fname) if fname else (False, "tidak ada file baru")
            if ok_file:
                f = dest / fname
                size = f.stat().st_size
                # pastikan jellyfin bisa baca
                subprocess.run(["chmod", "664", str(f)], check=False)
                subprocess.run(["chmod", "775", str(dest)], check=False)

                extra = ""
                if fname.lower().endswith((".rar", ".zip", ".7z")):
                    extra = "\n\n📦 Ini arsip — bilang saja kalau mau Violet ekstrak."
                if r.returncode != 0:
                    extra += (f"\n\n⚠️ curl keluar kode {r.returncode}, tapi file "
                              f"terverifikasi lengkap ({reason}) — dianggap selesai.")
                send(
                    f"✅ **Selesai**\n\n"
                    f"File   : `{fname}`\n"
                    f"Ukuran : **{human(size)}**\n"
                    f"Folder : {LABEL[folder]}\n"
                    f"Path   : `{f}`{extra}",
                    kb_after(),
                )
            else:
                err = (r.stderr or r.stdout or "").strip()[-600:] or "tanpa pesan"
                # kalau tidak ada perubahan tapi exit 0, mungkin file sudah lengkap sebelumnya
                hint = f"\n\nℹ️ Alasan: {reason}"
                if r.returncode == 0 and not fname:
                    hint = ("\n\nℹ️ Tidak ada file baru atau berubah — kemungkinan file "
                            "sudah lengkap sebelumnya. Cek folder tujuan.")
                send(f"❌ **Gagal mengunduh**\n\n`{err}`{hint}\n\n"
                     f"Kemungkinan: link tidak publik, kuota Drive habis, atau file dihapus.")
    except subprocess.TimeoutExpired:
        send("⏱️ Unduhan melebihi 6 jam dan dihentikan.")
    except Exception as e:
        log.exception("download gagal")
        send(f"❌ Error: `{str(e)[:200]}`")
    finally:
        stop.set()
        BUSY.discard(folder)


def _watch_file(path: Path, label: str, stop: threading.Event) -> None:
    """Pantau ukuran satu file target dan kabari tiap +100 MB."""
    seen = {"v": -1.0}
    while not stop.wait(20):
        try:
            if not path.exists():
                continue
            sz = path.stat().st_size / 1024 / 1024
            if sz - seen["v"] >= 100:
                seen["v"] = sz
                send(f"📥 `{label}`\n{sz:,.0f} MB terunduh…")
        except Exception:
            pass


def run_pixeldrain(folder: str, fid: str) -> None:
    """Unduh satu file Pixeldrain (/u/<id>) ke folder tujuan, lalu notif."""
    dest = Path(TARGETS[folder])
    BUSY.add(folder)
    stop = threading.Event()
    try:
        try:
            info = pixeldrain_info(fid)
        except Exception:
            info = {}
        name = info.get("name") or f"{fid}"
        size = int(info.get("size", 0))
        target = dest / name

        send(
            f"⬇️ **Mulai mengunduh (Pixeldrain)**\n\n"
            f"Folder : {LABEL[folder]}\n"
            f"File   : `{name}`\n"
            f"Ukuran : {human(size) if size else '?'}\n"
            f"Tujuan : `{dest}`\n\n"
            f"Kalau file besar, ini bisa lama. Violet kabari kalau selesai."
        )

        watcher = threading.Thread(
            target=_watch_file, args=(target, name, stop), daemon=True
        )
        watcher.start()

        cmd = [
            "curl", "-fL", "--retry", "3", "--retry-delay", "5",
            "-C", "-", "-o", str(target),
            f"{PIXELDRAIN_API}/{fid}?download",
        ]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=6 * 3600)

        if r.returncode == 0:
            subprocess.run(["chmod", "664", str(target)], check=False)
            subprocess.run(["chmod", "775", str(dest)], check=False)
            final = target.stat().st_size
            extra = ""
            if name.lower().endswith((".rar", ".zip", ".7z")):
                extra = "\n\n📦 Ini arsip — bilang saja kalau mau Violet ekstrak."
            send(
                f"✅ **Selesai**\n\n"
                f"File   : `{name}`\n"
                f"Ukuran : **{human(final)}**\n"
                f"Folder : {LABEL[folder]}\n"
                f"Path   : `{target}`{extra}",
                kb_after(),
            )
        else:
            err = (r.stderr or "").strip()[-600:] or "tanpa pesan"
            send(f"❌ **Gagal mengunduh**\n\n`{err}`\n\n"
                 f"Kemungkinan: file dihapus, link kadaluarsa, atau kuota/rate-limit.")
    except subprocess.TimeoutExpired:
        send("⏱️ Unduhan melebihi 6 jam dan dihentikan.")
    except Exception as e:
        log.exception("pixeldrain download gagal")
        send(f"❌ Error: `{str(e)[:200]}`")
    finally:
        stop.set()
        BUSY.discard(folder)


def jellyfin_scan() -> str:
    if not JELLYFIN_TOKEN:
        return "Token Jellyfin belum di-set, scan tidak dijalankan."
    try:
        req = urllib.request.Request(
            f"{JELLYFIN_URL}/Library/Refresh", data=b"", method="POST",
            headers={"Authorization": f'MediaBrowser Token="{JELLYFIN_TOKEN}"'},
        )
        with urllib.request.urlopen(req, timeout=30) as r:
            return "✅ Scan Jellyfin dipicu. Metadata muncul beberapa saat lagi." if r.status in (200, 204) \
                   else f"Jellyfin membalas HTTP {r.status}"
    except Exception as e:
        return f"Scan gagal: `{str(e)[:150]}`"


# ── handler ────────────────────────────────────────────────────────────────
def handle_callback(cb: dict) -> None:
    data = (cb.get("data") or "").strip()
    chat = (cb.get("message") or {}).get("chat", {}).get("id")
    if chat != OWNER_ID:
        ack(cb.get("id"), "Bukan untuk kamu.")
        return

    if data.startswith("pick:"):
        folder = data.split(":", 1)[1]
        if folder not in TARGETS:
            ack(cb.get("id"), "Folder tidak dikenal")
            return
        STATE["folder"] = folder
        STATE["pending"] = None
        save_state(STATE)
        ack(cb.get("id"), f"Tujuan: {LABEL[folder]}")
        send(f"Tujuan diset: **{LABEL[folder]}**\n`{TARGETS[folder]}`\n\n"
             f"Sekarang kirim link Google Drive atau Pixeldrain-nya.")

    elif data.startswith("go:"):
        if data == "go:no":
            STATE["pending"] = None
            save_state(STATE)
            ack(cb.get("id"), "Dibatalkan")
            send("Dibatalkan.", kb_menu())
            return
        p = STATE.get("pending") or {}
        folder = p.get("folder")
        STATE["pending"] = None
        save_state(STATE)
        not_ready = (not folder or
                     (p.get("kind", "gdrive") == "gdrive" and not p.get("url")) or
                     (p.get("kind") == "pixeldrain" and not p.get("fid")))
        if not_ready:
            ack(cb.get("id"), "Tidak ada yang menunggu")
            send("Tidak ada unduhan yang menunggu.", kb_menu())
            return
        if folder in BUSY:
            ack(cb.get("id"), "Folder masih dipakai")
            send(f"⚠️ {LABEL[folder]} masih dipakai mengunduh. Tunggu selesai.")
            return
        ack(cb.get("id"), "Mulai…")
        if p.get("kind") == "pixeldrain":
            threading.Thread(target=run_pixeldrain, args=(folder, p["fid"]), daemon=True).start()
        else:
            threading.Thread(target=run_download, args=(folder, p.get("url")), daemon=True).start()

    elif data == "act:links":
        ack(cb.get("id"), "Link akses")
        send(links_text(), kb_links())

    elif data == "act:scan":
        ack(cb.get("id"), "Memindai…")
        send(jellyfin_scan(), kb_after())

    elif data == "act:menu":
        ack(cb.get("id"))
        send(menu_text(), kb_menu())

    else:
        ack(cb.get("id"))
        send(menu_text(), kb_menu())


def handle_message(msg: dict) -> None:
    chat = msg.get("chat", {}).get("id")
    if chat != OWNER_ID:
        return
    text = (msg.get("text") or "").strip()
    if not text:
        return

    if text.startswith("/"):
        cmd = text.split()[0].lower().split("@", 1)[0]
        if cmd in ("/start", "/menu", "/help"):
            send(menu_text(), kb_menu())
        elif cmd == "/scan":
            send(jellyfin_scan(), kb_after())
        else:
            send(menu_text(), kb_menu())
        return

    fid = drive_id(text)
    pd = pixeldrain_id(text)
    if not fid and not pd:
        send("Itu bukan link yang dikenali.\n\n"
             "Google Drive: `https://drive.google.com/file/d/<id>/view`\n"
             "Pixeldrain : `https://pixeldrain.com/u/<id>`", kb_menu())
        return

    folder = STATE.get("folder")
    if not folder:
        send("Pilih folder tujuan dulu.", kb_menu())
        return

    if pd:
        kind, pid = pd
        if kind != "u":
            send("Untuk sekarang baru file tunggal (`/u/<id>`) yang didukung. "
                 "Link list (`/l/<id>`) menyusul.", kb_menu())
            return
        STATE["pending"] = {"folder": folder, "kind": "pixeldrain", "fid": pid}
        save_state(STATE)
        send(
            f"**Konfirmasi unduhan (Pixeldrain)**\n\n"
            f"Folder : {LABEL[folder]}\n"
            f"File id: `{pid}`\n\n"
            f"Lanjut?",
            kb_confirm(folder),
        )
        return

    STATE["pending"] = {"folder": folder, "kind": "gdrive", "url": text}
    save_state(STATE)
    send(
        f"**Konfirmasi unduhan**\n\n"
        f"Folder : {LABEL[folder]}\n"
        f"File id: `{fid[:12]}…`\n\n"
        f"Lanjut?",
        kb_confirm(folder),
    )


# ── loop ───────────────────────────────────────────────────────────────────
def main() -> None:
    log.info("PHS Downloader mulai — owner %s", OWNER_ID)
    try:
        tg("setMyCommands", {"commands": [
            {"command": "menu", "description": "Pilih folder tujuan"},
            {"command": "scan", "description": "Scan library Jellyfin"},
            {"command": "help", "description": "Bantuan"},
        ]})
        send("🟢 **PHS Downloader aktif**\n\nPilih folder tujuan untuk mulai.", kb_menu())
    except Exception:
        log.exception("pesan awal gagal")

    while True:
        try:
            d = tg("getUpdates", {
                "offset": STATE.get("offset", 0),
                "timeout": 25,
                "allowed_updates": ["message", "callback_query"],
            }, timeout=40)

            for u in d.get("result", []):
                STATE["offset"] = u["update_id"] + 1
                save_state(STATE)
                if "callback_query" in u:
                    try:
                        handle_callback(u["callback_query"])
                    except Exception:
                        log.exception("callback gagal")
                elif "message" in u:
                    try:
                        handle_message(u["message"])
                    except Exception:
                        log.exception("message gagal")
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError, OSError) as e:
            log.warning("polling error: %s", e)
            time.sleep(3)
        except Exception:
            log.exception("loop error")
            time.sleep(3)


if __name__ == "__main__":
    main()