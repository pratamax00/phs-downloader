use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PIXELDRAIN_API: &str = "https://pixeldrain.com/api/file";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

// ── IPv4-first resolver (anti IPv6 blackhole) ───────────────────────────────
struct Ipv4FirstResolver;

impl ureq::Resolver for Ipv4FirstResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let mut addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        addrs.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
        Ok(addrs)
    }
}

// ── konfigurasi ─────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    state_file: PathBuf,
    gdrive_dl: String,
    jellyfin_url: String,
    jellyfin_token: String,
    link_filebrowser: String,
    link_jellyfin_lan: String,
    link_jellyfin_ts: String,
    server_ip: String,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let tg_token = env::var("TG_TOKEN")
            .map_err(|_| "Env TG_TOKEN belum diset".to_string())?;
        if tg_token.trim().is_empty() {
            return Err("Env TG_TOKEN kosong".to_string());
        }
        let owner_id = env::var("OWNER_ID")
            .unwrap_or_else(|_| "644438052".to_string())
            .parse::<i64>()
            .map_err(|e| format!("OWNER_ID tidak valid: {e}"))?;
        let state_file = PathBuf::from(
            env::var("STATE_FILE")
                .unwrap_or_else(|_| "/home/pratamax/apps/bots/phs-downloader/state.json".to_string()),
        );
        let gdrive_dl = env::var("GDRIVE_DL")
            .unwrap_or_else(|_| "/home/pratamax/.local/bin/gdrive-dl".to_string());
        let jellyfin_url = env::var("JELLYFIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8096".to_string());
        let jellyfin_token = env::var("JELLYFIN_TOKEN").unwrap_or_default();
        let link_filebrowser = env::var("LINK_FILEBROWSER")
            .unwrap_or_else(|_| "https://files.pratamax.web.id".to_string());
        let link_jellyfin_lan = env::var("LINK_JELLYFIN_LAN")
            .unwrap_or_else(|_| "http://192.168.31.254:8096".to_string());
        let link_jellyfin_ts = env::var("LINK_JELLYFIN_TS")
            .unwrap_or_else(|_| "http://100.99.15.68:8096".to_string());
        let server_ip = env::var("SERVER_IP")
            .unwrap_or_else(|_| "192.168.31.254".to_string());

        Ok(Self {
            tg_token,
            owner_id,
            state_file,
            gdrive_dl,
            jellyfin_url,
            jellyfin_token,
            link_filebrowser,
            link_jellyfin_lan,
            link_jellyfin_ts,
            server_ip,
        })
    }
}

fn target_path(folder: &str) -> Option<&'static str> {
    match folder {
        "films" => Some("/mnt/storage/media/films"),
        "series" => Some("/mnt/storage/media/series"),
        "anime" => Some("/mnt/storage/media/anime"),
        _ => None,
    }
}

fn target_label(folder: &str) -> &'static str {
    match folder {
        "films" => "🎬 Film",
        "series" => "📺 Series",
        "anime" => "🌸 Anime",
        _ => "📁 Lainnya",
    }
}

const TARGET_KEYS: [&str; 3] = ["films", "series", "anime"];

// ── state ───────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Pending {
    folder: String,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fid: Option<String>,
}

fn default_kind() -> String {
    "gdrive".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    pending: Option<Pending>,
}

fn load_state(path: &Path) -> BotState {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => BotState::default(),
    }
}

fn save_state(path: &Path, state: &BotState) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(data) = serde_json::to_string_pretty(state) {
        if fs::write(&tmp, data).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }
}

// ── app context ─────────────────────────────────────────────────────────────
struct App {
    cfg: Config,
    agent: ureq::Agent,
    state: Mutex<BotState>,
    busy: Mutex<HashSet<String>>,
}

impl App {
    fn new(cfg: Config) -> Arc<Self> {
        let agent = ureq::AgentBuilder::new()
            .resolver(Ipv4FirstResolver)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(45))
            .build();
        let initial_state = load_state(&cfg.state_file);
        Arc::new(Self {
            cfg,
            agent,
            state: Mutex::new(initial_state),
            busy: Mutex::new(HashSet::new()),
        })
    }

    fn tg(&self, method: &str, payload: Value) -> Result<Value, String> {
        let url = format!("https://api.telegram.org/bot{}/{}", self.cfg.tg_token, method);
        let resp = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|e| format!("{e}"))?;
        let text = resp.into_string().map_err(|e| format!("{e}"))?;
        if text.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).map_err(|e| format!("JSON decode gagal: {e}"))
    }

    fn send(&self, text: &str, keyboard: Option<Value>) {
        let mut payload = json!({
            "chat_id": self.cfg.owner_id,
            "rich_message": {"markdown": text},
            "disable_web_page_preview": true,
        });
        if let Some(kb) = &keyboard {
            payload["reply_markup"] = kb.clone();
        }
        if let Err(e) = self.tg("sendRichMessage", payload) {
            log_warn(&format!("sendRichMessage gagal ({e}), fallback ke sendMessage"));
            let plain = strip_markdown_fallback(text);
            let mut fallback = json!({
                "chat_id": self.cfg.owner_id,
                "text": plain,
                "disable_web_page_preview": true,
            });
            if let Some(kb) = keyboard {
                fallback["reply_markup"] = kb;
            }
            if let Err(e2) = self.tg("sendMessage", fallback) {
                log_error(&format!("sendMessage juga gagal: {e2}"));
            }
        }
    }

    /// Non-blocking callback ack agar respons tombol tetap 1x RTT.
    fn ack(self: &Arc<Self>, cb_id: String, text: &str) {
        let app = Arc::clone(self);
        let msg = text.to_string();
        thread::spawn(move || {
            let mut payload = json!({"callback_query_id": cb_id});
            if !msg.is_empty() {
                payload["text"] = json!(msg);
            }
            let _ = app.tg("answerCallbackQuery", payload);
        });
    }

    fn persist_state(&self, s: &BotState) {
        save_state(&self.cfg.state_file, s);
    }

    fn links_text(&self) -> String {
        let mut lines = vec![
            "**🔗 Link Akses**\n".to_string(),
            "| Layanan | URL |".to_string(),
            "|---|---|".to_string(),
            format!("| Filebrowser | {} |", self.cfg.link_filebrowser),
            format!("| Jellyfin LAN | {} |", self.cfg.link_jellyfin_lan),
            format!("| Jellyfin Tailscale | {} |", self.cfg.link_jellyfin_ts),
            "".to_string(),
            "**Folder library**".to_string(),
            "| Folder | Path |".to_string(),
            "|---|---|".to_string(),
        ];
        for k in TARGET_KEYS {
            if let Some(p) = target_path(k) {
                lines.push(format!("| {} | `{}` |", target_label(k), p));
            }
        }
        lines.push("".to_string());
        lines.push("**Catatan**".to_string());
        lines.push("- Filebrowser bisa dibuka dari mana saja (Cloudflare tunnel).".to_string());
        lines.push(
            "- Jellyfin **LAN** hanya di jaringan lokal; **Tailscale** kalau di luar rumah."
                .to_string(),
        );
        lines.push(format!("- Server: `{}`", self.cfg.server_ip));
        lines.join("\n")
    }

    fn jellyfin_scan(&self) -> String {
        if self.cfg.jellyfin_token.is_empty() {
            return "Token Jellyfin belum di-set, scan tidak dijalankan.".to_string();
        }
        let url = format!("{}/Library/Refresh", self.cfg.jellyfin_url.trim_end_matches('/'));
        let auth = format!("MediaBrowser Token=\"{}\"", self.cfg.jellyfin_token);
        match self.agent.post(&url).set("Authorization", &auth).call() {
            Ok(resp) => {
                let status = resp.status();
                if status == 200 || status == 204 {
                    "✅ Scan Jellyfin dipicu. Metadata muncul beberapa saat lagi.".to_string()
                } else {
                    format!("Jellyfin membalas HTTP {status}")
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                format!("Scan gagal: `{}`", truncate_str(&msg, 150))
            }
        }
    }

    fn pixeldrain_info(&self, fid: &str) -> Value {
        let url = format!("{PIXELDRAIN_API}/{fid}/info");
        match self.agent.get(&url).set("User-Agent", "Mozilla/5.0").call() {
            Ok(resp) => resp.into_json::<Value>().unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        }
    }
}

// ── helper murni ────────────────────────────────────────────────────────────
fn log_info(msg: &str) {
    eprintln!("[INFO] {msg}");
}

fn log_warn(msg: &str) {
    eprintln!("[WARN] {msg}");
}

fn log_error(msg: &str) {
    eprintln!("[ERROR] {msg}");
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn tail_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        chars[chars.len() - max_chars..].iter().collect()
    }
}

fn strip_markdown_fallback(text: &str) -> String {
    static RE_BOLD: OnceLock<Regex> = OnceLock::new();
    static RE_CODE: OnceLock<Regex> = OnceLock::new();
    static RE_ITALIC: OnceLock<Regex> = OnceLock::new();

    let re_bold = RE_BOLD.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    let re_code = RE_CODE.get_or_init(|| Regex::new(r"`(.+?)`").unwrap());
    let re_italic = RE_ITALIC.get_or_init(|| Regex::new(r"(?m)^_|_$").unwrap());

    let s = re_bold.replace_all(text, "$1");
    let s = re_code.replace_all(&s, "$1");
    re_italic.replace_all(&s, "").into_owned()
}

fn drive_id(s: &str) -> Option<String> {
    static RE_FULL: OnceLock<Regex> = OnceLock::new();
    static RE_URL: OnceLock<Regex> = OnceLock::new();

    let s = s.trim();
    let re_full = RE_FULL.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{20,}$").unwrap());
    if re_full.is_match(s) {
        return Some(s.to_string());
    }
    let re_url = RE_URL.get_or_init(|| Regex::new(r"(?:/d/|id=)([A-Za-z0-9_-]{20,})").unwrap());
    re_url.captures(s).map(|c| c[1].to_string())
}

fn pixeldrain_id(s: &str) -> Option<(String, String)> {
    static RE_PD: OnceLock<Regex> = OnceLock::new();
    let re = RE_PD.get_or_init(|| Regex::new(r"pixeldrain\.com/([ul])/([A-Za-z0-9]+)").unwrap());
    re.captures(s.trim())
        .map(|c| (c[1].to_string(), c[2].to_string()))
}

fn human(bytes: u64) -> String {
    let mut n = bytes as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 {
            return format!("{n:.1} {unit}");
        }
        n /= 1024.0;
    }
    format!("{n:.1} PB")
}

fn format_mb(mb: f64) -> String {
    let rounded = mb.round() as u64;
    let s = rounded.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn looks_complete(path: &Path) -> (bool, String) {
    let size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return (false, "file tidak ada".to_string()),
    };
    if size == 0 {
        return (false, "file 0 byte".to_string());
    }
    let mut buf = [0u8; 512];
    let n = match File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => 0,
    };
    let head_str = String::from_utf8_lossy(&buf[..n]);
    let trimmed = head_str.trim_start().to_ascii_lowercase();
    let prefix200: String = trimmed.chars().take(200).collect();
    if trimmed.starts_with("<!doctype html")
        || trimmed.starts_with("<html")
        || prefix200.contains("<head")
    {
        return (false, "isinya halaman HTML".to_string());
    }
    (true, format!("{size} byte"))
}

fn is_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".rar") || lower.ends_with(".zip") || lower.ends_with(".7z")
}

// ── keyboard ────────────────────────────────────────────────────────────────
fn button(text: &str, data: &str, style: &str) -> Value {
    let mut b = json!({"text": text, "callback_data": data});
    if matches!(style, "primary" | "success" | "danger") {
        b["style"] = json!(style);
    }
    b
}

fn rows(buttons: Vec<Value>) -> Value {
    let mut out = Vec::new();
    let mut chunk = Vec::with_capacity(2);
    for b in buttons {
        chunk.push(b);
        if chunk.len() == 2 {
            out.push(Value::Array(std::mem::take(&mut chunk)));
        }
    }
    if !chunk.is_empty() {
        out.push(Value::Array(chunk));
    }
    Value::Array(out)
}

fn kb_menu() -> Value {
    json!({
        "inline_keyboard": rows(vec![
            button(target_label("films"), "pick:films", "primary"),
            button(target_label("series"), "pick:series", "primary"),
            button(target_label("anime"), "pick:anime", "primary"),
            button("🔗 Link Akses", "act:links", ""),
        ])
    })
}

fn kb_confirm(folder: &str) -> Value {
    json!({
        "inline_keyboard": [[
            button(&format!("✅ Unduh ke {}", target_label(folder)), "go:yes", "success"),
            button("❌ Batal", "go:no", "danger"),
        ]]
    })
}

fn kb_after() -> Value {
    json!({
        "inline_keyboard": rows(vec![
            button("🔄 Scan Jellyfin", "act:scan", "primary"),
            button("🔗 Link Akses", "act:links", ""),
            button("📁 Menu", "act:menu", ""),
        ])
    })
}

fn kb_links() -> Value {
    json!({
        "inline_keyboard": [[button("📁 Menu", "act:menu", "")]]
    })
}

fn menu_text() -> String {
    let mut usage = Vec::new();
    for k in TARGET_KEYS {
        if let Some(p) = target_path(k) {
            let count = fs::read_dir(p)
                .map(|rd| rd.filter_map(Result::ok).count())
                .unwrap_or(0);
            usage.push(format!("| {} | `{}` | {} item |", target_label(k), p, count));
        }
    }
    format!(
        "**PHS Downloader**\n*Unduh file Google Drive / Pixeldrain ke library media*\n\n| Folder | Path | Isi |\n|---|---|---|\n{}\n\nPilih folder tujuan, lalu kirim link Google Drive.",
        usage.join("\n")
    )
}

// ── subprocess dengan timeout 6 jam (anti-deadlock pipe) ───────────────────
enum CmdOutcome {
    Finished { code: i32, stdout: String, stderr: String },
    TimedOut,
    SpawnError(String),
}

fn run_cmd_with_timeout(program: &str, args: &[String], timeout: Duration) -> CmdOutcome {
    let mut child = match Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return CmdOutcome::SpawnError(format!("{e}")),
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let out_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut p) = stdout_pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let err_handle = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut p) = stderr_pipe {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_handle.join().unwrap_or_default();
                let stderr = err_handle.join().unwrap_or_default();
                let code = status.code().unwrap_or(-1);
                return CmdOutcome::Finished { code, stdout, stderr };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CmdOutcome::TimedOut;
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                let _ = child.kill();
                return CmdOutcome::SpawnError(format!("{e}"));
            }
        }
    }
}

fn snapshot_dir(dest: &Path) -> HashMap<String, (u64, u128)> {
    let mut snap = HashMap::new();
    if let Ok(rd) = fs::read_dir(dest) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    snap.insert(name, (meta.len(), mtime));
                }
            }
        }
    }
    snap
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < total {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    stop.load(Ordering::Relaxed)
}

// ── unduhan ─────────────────────────────────────────────────────────────────
fn run_download(app: Arc<App>, folder: String, url: String) {
    let Some(dest_str) = target_path(&folder) else {
        return;
    };
    let dest = PathBuf::from(dest_str);
    {
        app.busy.lock().unwrap().insert(folder.clone());
    }

    app.send(
        &format!(
            "⬇️ **Mulai mengunduh**\n\nFolder : {}\nTujuan : `{}`\n\nKalau file besar, ini bisa lama. Violet kabari kalau selesai.",
            target_label(&folder),
            dest.display()
        ),
        None,
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::clone(&stop);
    let app_watch = Arc::clone(&app);
    let dest_watch = dest.clone();

    let watcher = thread::spawn(move || {
        let start_names: HashSet<String> = fs::read_dir(&dest_watch)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let mut seen_max: f64 = 0.0;

        while !sleep_interruptible(&stop_watch, Duration::from_secs(60)) {
            let Ok(rd) = fs::read_dir(&dest_watch) else {
                continue;
            };
            let mut largest: Option<(String, u64)> = None;
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || start_names.contains(&name) {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        let sz = meta.len();
                        if largest.as_ref().map(|(_, s)| sz > *s).unwrap_or(true) {
                            largest = Some((name, sz));
                        }
                    }
                }
            }
            if let Some((name, bytes)) = largest {
                let sz_mb = bytes as f64 / 1024.0 / 1024.0;
                if sz_mb - seen_max >= 100.0 {
                    seen_max = sz_mb;
                    app_watch.send(
                        &format!("📥 `{}`\n{} MB terunduh…", name, format_mb(sz_mb)),
                        None,
                    );
                }
            }
        }
    });

    let before = snapshot_dir(&dest);
    let args = vec![url, "-o".to_string(), dest.to_string_lossy().into_owned()];
    let outcome = run_cmd_with_timeout(&app.cfg.gdrive_dl, &args, DOWNLOAD_TIMEOUT);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();

    match outcome {
        CmdOutcome::Finished { code, stdout, stderr } => {
            let after = snapshot_dir(&dest);
            let mut changed: Vec<(&String, &(u64, u128))> = after
                .iter()
                .filter(|(n, meta)| before.get(*n) != Some(*meta))
                .collect();
            changed.sort_by_key(|(_, (sz, _))| *sz);
            let fname = changed.last().map(|(n, _)| (*n).clone());

            let (ok_file, reason) = match &fname {
                Some(n) => looks_complete(&dest.join(n)),
                None => (false, "tidak ada file baru".to_string()),
            };

            if ok_file {
                let fname = fname.unwrap();
                let f = dest.join(&fname);
                let size = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
                let _ = Command::new("chmod").args(["664", &f.to_string_lossy()]).status();
                let _ = Command::new("chmod").args(["775", &dest.to_string_lossy()]).status();

                let mut extra = String::new();
                if is_archive(&fname) {
                    extra.push_str("\n\n📦 Ini arsip — bilang saja kalau mau Violet ekstrak.");
                }
                if code != 0 {
                    extra.push_str(&format!(
                        "\n\n⚠️ curl keluar kode {code}, tapi file terverifikasi lengkap ({reason}) — dianggap selesai."
                    ));
                }
                app.send(
                    &format!(
                        "✅ **Selesai**\n\nFile   : `{fname}`\nUkuran : **{}**\nFolder : {}\nPath   : `{}`{extra}",
                        human(size),
                        target_label(&folder),
                        f.display()
                    ),
                    Some(kb_after()),
                );
            } else {
                let raw_err = if !stderr.trim().is_empty() {
                    stderr.trim()
                } else {
                    stdout.trim()
                };
                let err = if raw_err.is_empty() {
                    "tanpa pesan".to_string()
                } else {
                    tail_str(raw_err, 600)
                };
                let hint = if code == 0 && fname.is_none() {
                    "\n\nℹ️ Tidak ada file baru atau berubah — kemungkinan file sudah lengkap sebelumnya. Cek folder tujuan.".to_string()
                } else {
                    format!("\n\nℹ️ Alasan: {reason}")
                };
                app.send(
                    &format!(
                        "❌ **Gagal mengunduh**\n\n`{err}`{hint}\n\nKemungkinan: link tidak publik, kuota Drive habis, atau file dihapus."
                    ),
                    None,
                );
            }
        }
        CmdOutcome::TimedOut => {
            app.send("⏱️ Unduhan melebihi 6 jam dan dihentikan.", None);
        }
        CmdOutcome::SpawnError(e) => {
            log_error(&format!("download gagal: {e}"));
            app.send(&format!("❌ Error: `{}`", truncate_str(&e, 200)), None);
        }
    }

    app.busy.lock().unwrap().remove(&folder);
}

fn run_pixeldrain(app: Arc<App>, folder: String, fid: String) {
    let Some(dest_str) = target_path(&folder) else {
        return;
    };
    let dest = PathBuf::from(dest_str);
    {
        app.busy.lock().unwrap().insert(folder.clone());
    }

    let info = app.pixeldrain_info(&fid);
    let name = info
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| fid.clone());
    let expected_size = info.get("size").and_then(Value::as_u64).unwrap_or(0);
    let target = dest.join(&name);

    let size_display = if expected_size > 0 {
        human(expected_size)
    } else {
        "?".to_string()
    };

    app.send(
        &format!(
            "⬇️ **Mulai mengunduh (Pixeldrain)**\n\nFolder : {}\nFile   : `{name}`\nUkuran : {size_display}\nTujuan : `{}`\n\nKalau file besar, ini bisa lama. Violet kabari kalau selesai.",
            target_label(&folder),
            dest.display()
        ),
        None,
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::clone(&stop);
    let app_watch = Arc::clone(&app);
    let target_watch = target.clone();
    let label_watch = name.clone();

    let watcher = thread::spawn(move || {
        let mut seen: f64 = -1.0;
        while !sleep_interruptible(&stop_watch, Duration::from_secs(20)) {
            if let Ok(meta) = fs::metadata(&target_watch) {
                let sz_mb = meta.len() as f64 / 1024.0 / 1024.0;
                if sz_mb - seen >= 100.0 {
                    seen = sz_mb;
                    app_watch.send(
                        &format!("📥 `{}`\n{} MB terunduh…", label_watch, format_mb(sz_mb)),
                        None,
                    );
                }
            }
        }
    });

    let args = vec![
        "-fL".to_string(),
        "--retry".to_string(),
        "3".to_string(),
        "--retry-delay".to_string(),
        "5".to_string(),
        "-C".to_string(),
        "-".to_string(),
        "-o".to_string(),
        target.to_string_lossy().into_owned(),
        format!("{PIXELDRAIN_API}/{fid}?download"),
    ];
    let outcome = run_cmd_with_timeout("curl", &args, DOWNLOAD_TIMEOUT);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();

    match outcome {
        CmdOutcome::Finished { code, stderr, .. } => {
            if code == 0 {
                let _ = Command::new("chmod").args(["664", &target.to_string_lossy()]).status();
                let _ = Command::new("chmod").args(["775", &dest.to_string_lossy()]).status();
                let final_size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                let extra = if is_archive(&name) {
                    "\n\n📦 Ini arsip — bilang saja kalau mau Violet ekstrak."
                } else {
                    ""
                };
                app.send(
                    &format!(
                        "✅ **Selesai**\n\nFile   : `{name}`\nUkuran : **{}**\nFolder : {}\nPath   : `{}`{extra}",
                        human(final_size),
                        target_label(&folder),
                        target.display()
                    ),
                    Some(kb_after()),
                );
            } else {
                let err = if stderr.trim().is_empty() {
                    "tanpa pesan".to_string()
                } else {
                    tail_str(stderr.trim(), 600)
                };
                app.send(
                    &format!(
                        "❌ **Gagal mengunduh**\n\n`{err}`\n\nKemungkinan: file dihapus, link kadaluarsa, atau kuota/rate-limit."
                    ),
                    None,
                );
            }
        }
        CmdOutcome::TimedOut => {
            app.send("⏱️ Unduhan melebihi 6 jam dan dihentikan.", None);
        }
        CmdOutcome::SpawnError(e) => {
            log_error(&format!("pixeldrain download gagal: {e}"));
            app.send(&format!("❌ Error: `{}`", truncate_str(&e, 200)), None);
        }
    }

    app.busy.lock().unwrap().remove(&folder);
}

// ── handler ─────────────────────────────────────────────────────────────────
fn handle_callback(app: &Arc<App>, cb: &Value) {
    let cb_id = cb
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let data = cb
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let chat_id = cb
        .pointer("/message/chat/id")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    if chat_id != app.cfg.owner_id {
        app.ack(cb_id, "Akses khusus owner.");
        return;
    }

    if let Some(folder) = data.strip_prefix("pick:") {
        let Some(path) = target_path(folder) else {
            app.ack(cb_id, "Folder tidak dikenal");
            return;
        };
        {
            let mut st = app.state.lock().unwrap();
            st.folder = Some(folder.to_string());
            st.pending = None;
            app.persist_state(&st);
        }
        app.ack(cb_id, &format!("Tujuan: {}", target_label(folder)));
        app.send(
            &format!(
                "Tujuan diset: **{}**\n`{path}`\n\nSekarang kirim link Google Drive atau Pixeldrain-nya.",
                target_label(folder)
            ),
            None,
        );
    } else if data.starts_with("go:") {
        if data == "go:no" {
            {
                let mut st = app.state.lock().unwrap();
                st.pending = None;
                app.persist_state(&st);
            }
            app.ack(cb_id, "Dibatalkan");
            app.send("Dibatalkan.", Some(kb_menu()));
            return;
        }
        let pending = {
            let mut st = app.state.lock().unwrap();
            let p = st.pending.take();
            app.persist_state(&st);
            p
        };
        let Some(p) = pending else {
            app.ack(cb_id, "Tidak ada yang menunggu");
            app.send("Tidak ada unduhan yang menunggu.", Some(kb_menu()));
            return;
        };
        let not_ready = p.folder.is_empty()
            || (p.kind == "gdrive" && p.url.as_deref().unwrap_or("").is_empty())
            || (p.kind == "pixeldrain" && p.fid.as_deref().unwrap_or("").is_empty());
        if not_ready {
            app.ack(cb_id, "Tidak ada yang menunggu");
            app.send("Tidak ada unduhan yang menunggu.", Some(kb_menu()));
            return;
        }
        if app.busy.lock().unwrap().contains(&p.folder) {
            app.ack(cb_id, "Folder masih dipakai");
            app.send(
                &format!(
                    "⚠️ {} masih dipakai mengunduh. Tunggu selesai.",
                    target_label(&p.folder)
                ),
                None,
            );
            return;
        }
        app.ack(cb_id, "Mulai…");
        let app_clone = Arc::clone(app);
        if p.kind == "pixeldrain" {
            let fid = p.fid.unwrap_or_default();
            thread::spawn(move || run_pixeldrain(app_clone, p.folder, fid));
        } else {
            let url = p.url.unwrap_or_default();
            thread::spawn(move || run_download(app_clone, p.folder, url));
        }
    } else if data == "act:links" {
        app.ack(cb_id, "Link akses");
        app.send(&app.links_text(), Some(kb_links()));
    } else if data == "act:scan" {
        app.ack(cb_id, "Memindai…");
        app.send(&app.jellyfin_scan(), Some(kb_after()));
    } else {
        app.ack(cb_id, "");
        app.send(&menu_text(), Some(kb_menu()));
    }
}

fn handle_message(app: &Arc<App>, msg: &Value) {
    let chat_id = msg.pointer("/chat/id").and_then(Value::as_i64).unwrap_or(0);
    if chat_id != app.cfg.owner_id {
        return;
    }
    let text = msg
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if text.is_empty() {
        return;
    }

    if text.starts_with('/') {
        let cmd = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let cmd = cmd.split('@').next().unwrap_or("");
        match cmd {
            "/scan" => app.send(&app.jellyfin_scan(), Some(kb_after())),
            _ => app.send(&menu_text(), Some(kb_menu())),
        }
        return;
    }

    let fid = drive_id(text);
    let pd = pixeldrain_id(text);
    if fid.is_none() && pd.is_none() {
        app.send(
            "Itu bukan link yang dikenali.\n\nGoogle Drive: `https://drive.google.com/file/d/<id>/view`\nPixeldrain : `https://pixeldrain.com/u/<id>`",
            Some(kb_menu()),
        );
        return;
    }

    let folder = { app.state.lock().unwrap().folder.clone() };
    let Some(folder) = folder else {
        app.send("Pilih folder tujuan dulu.", Some(kb_menu()));
        return;
    };

    if let Some((kind, pid)) = pd {
        if kind != "u" {
            app.send(
                "Untuk sekarang baru file tunggal (`/u/<id>`) yang didukung. Link list (`/l/<id>`) menyusul.",
                Some(kb_menu()),
            );
            return;
        }
        {
            let mut st = app.state.lock().unwrap();
            st.pending = Some(Pending {
                folder: folder.clone(),
                kind: "pixeldrain".to_string(),
                url: None,
                fid: Some(pid.clone()),
            });
            app.persist_state(&st);
        }
        app.send(
            &format!(
                "**Konfirmasi unduhan (Pixeldrain)**\n\nFolder : {}\nFile id: `{pid}`\n\nLanjut?",
                target_label(&folder)
            ),
            Some(kb_confirm(&folder)),
        );
        return;
    }

    let fid = fid.unwrap();
    {
        let mut st = app.state.lock().unwrap();
        st.pending = Some(Pending {
            folder: folder.clone(),
            kind: "gdrive".to_string(),
            url: Some(text.to_string()),
            fid: None,
        });
        app.persist_state(&st);
    }
    app.send(
        &format!(
            "**Konfirmasi unduhan**\n\nFolder : {}\nFile id: `{}…`\n\nLanjut?",
            target_label(&folder),
            truncate_str(&fid, 12)
        ),
        Some(kb_confirm(&folder)),
    );
}

// ── main loop ───────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("phs-downloader {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env() {
            Ok(cfg) => {
                println!(
                    "OK: phs-downloader {VERSION} | owner={} | state={} | gdrive_dl={} | jellyfin={}",
                    cfg.owner_id,
                    cfg.state_file.display(),
                    cfg.gdrive_dl,
                    cfg.jellyfin_url
                );
                return;
            }
            Err(e) => {
                eprintln!("CONFIG ERROR: {e}");
                std::process::exit(1);
            }
        }
    }

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            log_error(&e);
            std::process::exit(1);
        }
    };

    let app = App::new(cfg);
    log_info(&format!(
        "PHS Downloader v{VERSION} (Rust) mulai — owner {}",
        app.cfg.owner_id
    ));

    if let Err(e) = app.tg(
        "setMyCommands",
        json!({
            "commands": [
                {"command": "menu", "description": "Pilih folder tujuan"},
                {"command": "scan", "description": "Scan library Jellyfin"},
                {"command": "help", "description": "Bantuan"},
            ]
        }),
    ) {
        log_warn(&format!("setMyCommands gagal: {e}"));
    }
    app.send(
        "🟢 **PHS Downloader aktif**\n\nPilih folder tujuan untuk mulai.",
        Some(kb_menu()),
    );

    loop {
        let offset = { app.state.lock().unwrap().offset };
        let resp = app.tg(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": 25,
                "allowed_updates": ["message", "callback_query"],
            }),
        );

        match resp {
            Ok(data) => {
                if let Some(updates) = data.get("result").and_then(Value::as_array) {
                    for u in updates {
                        if let Some(uid) = u.get("update_id").and_then(Value::as_i64) {
                            let mut st = app.state.lock().unwrap();
                            st.offset = uid + 1;
                            app.persist_state(&st);
                        }
                        if let Some(cb) = u.get("callback_query") {
                            handle_callback(&app, cb);
                        } else if let Some(msg) = u.get("message") {
                            handle_message(&app, msg);
                        }
                    }
                }
            }
            Err(e) => {
                log_warn(&format!("polling error: {e}"));
                thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drive_id() {
        assert_eq!(
            drive_id("https://drive.google.com/file/d/1AbCdEfGhIjKlMnOpQrStUvWx/view?usp=sharing"),
            Some("1AbCdEfGhIjKlMnOpQrStUvWx".to_string())
        );
        assert_eq!(
            drive_id("https://drive.google.com/open?id=1AbCdEfGhIjKlMnOpQrStUvWx"),
            Some("1AbCdEfGhIjKlMnOpQrStUvWx".to_string())
        );
        assert_eq!(
            drive_id("1AbCdEfGhIjKlMnOpQrStUvWx"),
            Some("1AbCdEfGhIjKlMnOpQrStUvWx".to_string())
        );
        assert_eq!(drive_id("bukan link"), None);
    }

    #[test]
    fn test_pixeldrain_id() {
        assert_eq!(
            pixeldrain_id("https://pixeldrain.com/u/AbC123xy"),
            Some(("u".to_string(), "AbC123xy".to_string()))
        );
        assert_eq!(
            pixeldrain_id("https://pixeldrain.com/l/List999"),
            Some(("l".to_string(), "List999".to_string()))
        );
        assert_eq!(pixeldrain_id("https://example.com/u/abc"), None);
    }

    #[test]
    fn test_human() {
        assert_eq!(human(512), "512.0 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1024 * 1024 * 5), "5.0 MB");
    }

    #[test]
    fn test_format_mb() {
        assert_eq!(format_mb(100.4), "100");
        assert_eq!(format_mb(1523.7), "1,524");
    }

    #[test]
    fn test_strip_markdown_fallback() {
        let out = strip_markdown_fallback("**Judul**\n`kode`\n_miring_");
        assert_eq!(out, "Judul\nkode\nmiring");
    }

    #[test]
    fn test_looks_complete() {
        let dir = env::temp_dir().join("phs_dl_test");
        let _ = fs::create_dir_all(&dir);

        let empty = dir.join("empty.bin");
        fs::write(&empty, b"").unwrap();
        assert!(!looks_complete(&empty).0);

        let html = dir.join("err.html");
        fs::write(&html, b"  <!DOCTYPE html><html><head></head></html>").unwrap();
        assert!(!looks_complete(&html).0);

        let ok = dir.join("movie.mkv");
        fs::write(&ok, b"Rar!\x1a\x07\x01\x00hello").unwrap();
        assert!(looks_complete(&ok).0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_state_roundtrip() {
        let dir = env::temp_dir().join("phs_dl_state_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let s = BotState {
            offset: 262919394,
            folder: Some("anime".to_string()),
            pending: None,
        };
        save_state(&path, &s);
        assert_eq!(load_state(&path), s);
        let _ = fs::remove_dir_all(&dir);
    }
}
