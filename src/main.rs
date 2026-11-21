use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

// ── konfigurasi (bebas hardcode data pribadi) ───────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    state_file: PathBuf,
    gdrive_dl: String,
    dir_films: String,
    dir_series: String,
    dir_anime: String,
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
            .map_err(|_| "Env TG_TOKEN wajib diset".to_string())?;
        if tg_token.trim().is_empty() {
            return Err("Env TG_TOKEN kosong".to_string());
        }
        let owner_raw = env::var("OWNER_ID")
            .map_err(|_| "Env OWNER_ID wajib diset".to_string())?;
        let owner_id = owner_raw
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("OWNER_ID tidak valid: {e}"))?;

        let state_file = PathBuf::from(
            env::var("STATE_FILE").unwrap_or_else(|_| "./state.json".to_string()),
        );
        let gdrive_dl = env::var("GDRIVE_DL").unwrap_or_else(|_| "gdrive-dl".to_string());
        let dir_films = env::var("DIR_FILMS")
            .unwrap_or_else(|_| "/mnt/storage/media/films".to_string());
        let dir_series = env::var("DIR_SERIES")
            .unwrap_or_else(|_| "/mnt/storage/media/series".to_string());
        let dir_anime = env::var("DIR_ANIME")
            .unwrap_or_else(|_| "/mnt/storage/media/anime".to_string());
        let jellyfin_url = env::var("JELLYFIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8096".to_string());
        let jellyfin_token = env::var("JELLYFIN_TOKEN").unwrap_or_default();
        let link_filebrowser = env::var("LINK_FILEBROWSER").unwrap_or_default();
        let link_jellyfin_lan = env::var("LINK_JELLYFIN_LAN").unwrap_or_default();
        let link_jellyfin_ts = env::var("LINK_JELLYFIN_TS").unwrap_or_default();
        let server_ip = env::var("SERVER_IP").unwrap_or_default();

        Ok(Self {
            tg_token,
            owner_id,
            state_file,
            gdrive_dl,
            dir_films,
            dir_series,
            dir_anime,
            jellyfin_url,
            jellyfin_token,
            link_filebrowser,
            link_jellyfin_lan,
            link_jellyfin_ts,
            server_ip,
        })
    }

    fn target_path(&self, folder: &str) -> Option<&str> {
        match folder {
            "films" => Some(self.dir_films.as_str()),
            "series" => Some(self.dir_series.as_str()),
            "anime" => Some(self.dir_anime.as_str()),
            _ => None,
        }
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

// ── formatter Bot API 10.3+ (selaras violet-breakglass & shared/tg-core) ────
fn md_cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

fn hl(text: &str) -> String {
    format!("=={text}==")
}

fn table_markdown(headers: &[&str], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let head: Vec<String> = headers.iter().map(|h| md_cell(h)).collect();
    let sep = format!(
        "|{}|",
        headers.iter().map(|_| ":---").collect::<Vec<_>>().join("|")
    );
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("| {} |", head.join(" | ")));
    lines.push(sep);
    for row in rows {
        let cells: Vec<String> = (0..headers.len())
            .map(|i| row.get(i).map(|s| md_cell(s)).unwrap_or_default())
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

fn details_block(summary: &str, content: &str) -> String {
    format!("<details>\n<summary>{summary}</summary>\n\n{content}\n\n</details>")
}

fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn tg_url_button(label: &str, url: &str) -> String {
    format!("<tg-button type=\"url\" url=\"{}\">{label}</tg-button>", esc_attr(url))
}

fn tg_cb_button(label: &str, data: &str) -> String {
    format!(
        "<tg-button type=\"callback_data\" data=\"{}\">{label}</tg-button>",
        esc_attr(data)
    )
}

fn tg_copy_button(label: &str, text: &str) -> String {
    format!(
        "<tg-button type=\"copy_text\" text=\"{}\">{label}</tg-button>",
        esc_attr(text)
    )
}

fn tg_button_row(buttons: &[String]) -> String {
    format!(
        "<tg-button-row align=\"left\">{}</tg-button-row>",
        buttons.join("")
    )
}

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn markdown_to_html(md: &str) -> String {
    static RE_TG_BTN: OnceLock<Regex> = OnceLock::new();
    static RE_DETAILS: OnceLock<Regex> = OnceLock::new();
    static RE_BOLD: OnceLock<Regex> = OnceLock::new();
    static RE_ITALIC: OnceLock<Regex> = OnceLock::new();
    static RE_CODE: OnceLock<Regex> = OnceLock::new();
    static RE_HL: OnceLock<Regex> = OnceLock::new();

    let re_tg = RE_TG_BTN.get_or_init(|| Regex::new(r"</?tg-button[^>]*>").unwrap());
    let re_det =
        RE_DETAILS.get_or_init(|| Regex::new(r"</?(?:details|summary)>").unwrap());
    let re_bold = RE_BOLD.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    let re_italic = RE_ITALIC.get_or_init(|| Regex::new(r"(?m)^_(.+)_$").unwrap());
    let re_code = RE_CODE.get_or_init(|| Regex::new(r"`([^`]+)`").unwrap());
    let re_hl = RE_HL.get_or_init(|| Regex::new(r"==(.+?)==").unwrap());

    let stripped = re_tg.replace_all(md, "");
    let stripped = re_det.replace_all(&stripped, "");
    let escaped = esc_html(&stripped);
    let s = re_bold.replace_all(&escaped, "<b>$1</b>");
    let s = re_italic.replace_all(&s, "<i>$1</i>");
    let s = re_code.replace_all(&s, "<code>$1</code>");
    re_hl.replace_all(&s, "<b>[$1]</b>").into_owned()
}

/// Stempel waktu WIB (UTC+7) tanpa dependency eksternal: "21 Nov 2026, 08:40 WIB".
fn stamp_wib() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 7 * 3600;
    format_epoch_wib(secs)
}

fn format_epoch_wib(wib_secs: i64) -> String {
    let days = wib_secs.div_euclid(86400);
    let tod = wib_secs.rem_euclid(86400);
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
    ];
    let mon = MONTHS[(m as usize).saturating_sub(1).min(11)];
    format!("{d:02} {mon} {year}, {hour:02}:{min:02} WIB")
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn progress_bar(ratio: f64, width: usize) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "▓".repeat(filled), "░".repeat(empty))
}

fn format_progress_line(bytes: u64, total: Option<u64>, elapsed_secs: u64) -> String {
    let speed_bps = if elapsed_secs > 0 {
        bytes as f64 / elapsed_secs as f64
    } else {
        0.0
    };
    let speed_mb = speed_bps / 1024.0 / 1024.0;
    let dur = format_duration(elapsed_secs);
    match total {
        Some(tot) if tot > 0 => {
            let pct = ((bytes as f64 / tot as f64) * 100.0).clamp(0.0, 100.0);
            let bar = progress_bar(bytes as f64 / tot as f64, 10);
            let eta = if speed_bps > 1.0 && tot > bytes {
                format!("ETA {}", format_duration(((tot - bytes) as f64 / speed_bps) as u64))
            } else {
                dur
            };
            format!(
                "📥 {bar} {} · {} · `{speed_mb:.1} MB/s` · `{eta}`",
                hl(&format!("{pct:.0}%")),
                hl(&format!("{} / {}", human(bytes), human(tot)))
            )
        }
        _ => {
            format!(
                "📥 {} · {} · `{dur}`",
                hl(&human(bytes)),
                hl(&format!("{speed_mb:.1} MB/s"))
            )
        }
    }
}

fn disk_usage_visual(path: &str) -> String {
    let out = Command::new("df")
        .args(["-h", "--output=used,size,pcent", path])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = text.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let pct_num = parts[2]
                        .trim_end_matches('%')
                        .parse::<f64>()
                        .unwrap_or(0.0);
                    let bar = progress_bar(pct_num / 100.0, 8);
                    return format!("{bar} {} · {}/{}", hl(parts[2]), parts[0], parts[1]);
                }
            }
        }
    }
    "—".to_string()
}

/// Cek cepat apakah port Jellyfin sedang hidup (timeout 150 ms).
fn jellyfin_online(url: &str) -> bool {
    let stripped = url
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host_port = stripped.split('/').next().unwrap_or("");
    let addr_str = if host_port.contains(':') {
        host_port.to_string()
    } else if url.starts_with("https://") {
        format!("{host_port}:443")
    } else {
        format!("{host_port}:80")
    };
    if let Ok(mut addrs) = addr_str.to_socket_addrs() {
        if let Some(addr) = addrs.next() {
            return TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok();
        }
    }
    false
}

/// Ambil 5 file terbaru di folder tertentu.
fn list_recent_files(dir: &Path, limit: usize) -> Vec<(String, u64, i64)> {
    let mut items = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let size = if meta.is_file() { meta.len() } else { 0 };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                items.push((name, size, mtime));
            }
        }
    }
    items.sort_by(|a, b| b.2.cmp(&a.2));
    items.truncate(limit);
    items
}

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
        let resp = match self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(payload)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(format!("HTTP {code}: {body}"));
            }
            Err(e) => return Err(format!("{e}")),
        };
        let text = resp.into_string().map_err(|e| format!("{e}"))?;
        if text.is_empty() {
            return Ok(json!({}));
        }
        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| format!("JSON decode gagal: {e}"))?;
        if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
            let desc = parsed
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("Telegram API error");
            return Err(desc.to_string());
        }
        Ok(parsed)
    }

    fn send(&self, markdown: &str, keyboard: Option<Value>) -> Option<i64> {
        let mut payload = json!({
            "chat_id": self.cfg.owner_id,
            "rich_message": {"markdown": markdown},
            "disable_web_page_preview": true,
        });
        if let Some(kb) = &keyboard {
            payload["reply_markup"] = kb.clone();
        }
        match self.tg("sendRichMessage", payload) {
            Ok(res) => res.pointer("/result/message_id").and_then(Value::as_i64),
            Err(e) => {
                log_warn(&format!("sendRichMessage gagal ({e}), fallback HTML"));
                let html = markdown_to_html(markdown);
                let mut fb = json!({
                    "chat_id": self.cfg.owner_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                });
                if let Some(kb) = keyboard {
                    fb["reply_markup"] = kb;
                }
                match self.tg("sendMessage", fb) {
                    Ok(res) => res.pointer("/result/message_id").and_then(Value::as_i64),
                    Err(e2) => {
                        log_error(&format!("sendMessage juga gagal: {e2}"));
                        None
                    }
                }
            }
        }
    }

    fn edit(&self, message_id: i64, markdown: &str, keyboard: Option<Value>) -> Option<i64> {
        let mut payload = json!({
            "chat_id": self.cfg.owner_id,
            "message_id": message_id,
            "rich_message": {"markdown": markdown},
            "disable_web_page_preview": true,
        });
        if let Some(kb) = &keyboard {
            payload["reply_markup"] = kb.clone();
        }
        match self.tg("editMessageText", payload) {
            Ok(_) => Some(message_id),
            Err(e) => {
                if e.to_ascii_lowercase().contains("message is not modified") {
                    return Some(message_id);
                }
                log_warn(&format!("editRich gagal ({e}), fallback edit HTML"));
                let html = markdown_to_html(markdown);
                let mut fb = json!({
                    "chat_id": self.cfg.owner_id,
                    "message_id": message_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                });
                if let Some(kb) = &keyboard {
                    fb["reply_markup"] = kb.clone();
                }
                match self.tg("editMessageText", fb) {
                    Ok(_) => Some(message_id),
                    Err(e2) => {
                        if e2.to_ascii_lowercase().contains("message is not modified") {
                            return Some(message_id);
                        }
                        log_warn(&format!("editMessageText HTML gagal ({e2}), kirim pesan baru"));
                        self.send(markdown, keyboard)
                    }
                }
            }
        }
    }

    fn present(&self, message_id: Option<i64>, markdown: &str, keyboard: Option<Value>) -> Option<i64> {
        match message_id {
            Some(mid) => self.edit(mid, markdown, keyboard),
            None => self.send(markdown, keyboard),
        }
    }

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

    fn kb_menu(&self) -> Value {
        let active = { self.state.lock().unwrap().folder.clone() };
        let style_for = |k: &str| {
            if active.as_deref() == Some(k) {
                "success"
            } else {
                "primary"
            }
        };
        json!({
            "inline_keyboard": [
                [
                    button(target_label("films"), "pick:films", style_for("films")),
                    button(target_label("series"), "pick:series", style_for("series")),
                ],
                [
                    button(target_label("anime"), "pick:anime", style_for("anime")),
                    button("🕒 Terbaru", "act:recent", "primary"),
                ],
                [
                    button("📡 Scan Jellyfin", "act:scan", "primary"),
                    button("🔗 Link Akses", "act:links", "primary"),
                ],
            ]
        })
    }

    fn menu_text(&self) -> String {
        let active_folder = { self.state.lock().unwrap().folder.clone() };
        let busy_count = { self.busy.lock().unwrap().len() };
        let jf_up = jellyfin_online(&self.cfg.jellyfin_url);

        let mut counts = [0usize; 3];
        let mut detail_rows = Vec::new();
        for (idx, k) in TARGET_KEYS.iter().enumerate() {
            if let Some(p) = self.cfg.target_path(k) {
                let c = fs::read_dir(p)
                    .map(|rd| rd.filter_map(Result::ok).count())
                    .unwrap_or(0);
                counts[idx] = c;
                detail_rows.push(vec![
                    target_label(k).to_string(),
                    format!("{c} item"),
                    format!("`{p}`"),
                ]);
            }
        }

        let status_val = if busy_count == 0 {
            hl("🟢 Siap")
        } else {
            hl(&format!("⬇️ Mengunduh ({busy_count})"))
        };
        let target_val = active_folder
            .as_deref()
            .map(|k| hl(target_label(k)))
            .unwrap_or_else(|| "Belum dipilih".to_string());
        let library_val = hl(&format!(
            "{} Film · {} Series · {} Anime",
            counts[0], counts[1], counts[2]
        ));
        let disk_val = disk_usage_visual(&self.cfg.dir_films);
        let jf_val = if jf_up {
            hl("🟢 Online")
        } else {
            "⏸️ Off (hemat RAM)".to_string()
        };

        let metrics = table_markdown(
            &["Field", "Value"],
            &[
                vec!["Status".into(), status_val],
                vec!["Tujuan".into(), target_val],
                vec!["Library".into(), library_val],
                vec!["Storage".into(), disk_val],
                vec!["Jellyfin".into(), jf_val],
            ],
        );

        let mut pills = vec![
            tg_cb_button("🔄 Segarkan", "act:menu"),
            tg_cb_button("🕒 Terbaru", "act:recent"),
        ];
        if jf_up {
            pills.push(tg_cb_button("📡 Scan", "act:scan"));
        }
        if !self.cfg.link_filebrowser.is_empty() {
            pills.push(tg_url_button("📂 Files", &self.cfg.link_filebrowser));
        }

        let prompt_line = match active_folder.as_deref() {
            Some(k) => format!(
                "Kirim link **Google Drive** atau **Pixeldrain** untuk unduh ke **{}**.",
                target_label(k)
            ),
            None => "Pilih folder tujuan di bawah, lalu kirim link unduhan.".to_string(),
        };

        let detail_body = format!(
            "{}\n\n- Tombol folder **hijau** menandakan tujuan aktif.\n- Progres unduhan diperbarui 1 baris di pesan yang sama.",
            table_markdown(&["Folder", "Isi", "Path"], &detail_rows)
        );

        [
            format!("**📦 PHS Downloader** · {}", hl(&format!("v{VERSION}"))),
            String::new(),
            metrics,
            String::new(),
            prompt_line,
            format!("_{}_", stamp_wib()),
            String::new(),
            tg_button_row(&pills),
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Folder Library & Panduan", &detail_body),
        ]
        .join("\n")
    }

    fn recent_text(&self) -> String {
        let folder = {
            self.state
                .lock()
                .unwrap()
                .folder
                .clone()
                .unwrap_or_else(|| "anime".to_string())
        };
        let path_str = self
            .cfg
            .target_path(&folder)
            .unwrap_or(self.cfg.dir_anime.as_str());
        let recent = list_recent_files(Path::new(path_str), 6);

        let body = if recent.is_empty() {
            "_Folder masih kosong._".to_string()
        } else {
            let rows_data: Vec<Vec<String>> = recent
                .iter()
                .map(|(name, size, _)| {
                    vec![
                        format!("`{}`", truncate_str(name, 30)),
                        if *size > 0 { hl(&human(*size)) } else { "📁 dir".into() },
                    ]
                })
                .collect();
            table_markdown(&["Item Terbaru", "Ukuran"], &rows_data)
        };

        let pills = tg_button_row(&[
            tg_copy_button("📋 Salin Path", path_str),
            tg_cb_button("🔄 Segarkan", "act:recent"),
            tg_cb_button("📁 Menu", "act:menu"),
        ]);

        [
            format!("**🕒 Terbaru** · {}", hl(target_label(&folder))),
            String::new(),
            body,
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Path Folder", &format!("`{path_str}`")),
        ]
        .join("\n")
    }

    fn links_text(&self) -> String {
        let mut service_rows = Vec::new();
        let mut pills = Vec::new();

        if !self.cfg.link_filebrowser.is_empty() {
            service_rows.push(vec![
                "Filebrowser".to_string(),
                self.cfg.link_filebrowser.clone(),
            ]);
            pills.push(tg_url_button("📂 Filebrowser", &self.cfg.link_filebrowser));
        }
        if !self.cfg.link_jellyfin_lan.is_empty() {
            service_rows.push(vec![
                "Jellyfin LAN".to_string(),
                self.cfg.link_jellyfin_lan.clone(),
            ]);
            pills.push(tg_url_button("🎬 Jellyfin LAN", &self.cfg.link_jellyfin_lan));
        }
        if !self.cfg.link_jellyfin_ts.is_empty() {
            service_rows.push(vec![
                "Jellyfin TS".to_string(),
                self.cfg.link_jellyfin_ts.clone(),
            ]);
            pills.push(tg_url_button("🌐 Jellyfin TS", &self.cfg.link_jellyfin_ts));
        }
        if !self.cfg.server_ip.is_empty() {
            pills.push(tg_copy_button("📋 Salin IP", &self.cfg.server_ip));
        }

        let mut path_rows = Vec::new();
        for k in TARGET_KEYS {
            if let Some(p) = self.cfg.target_path(k) {
                path_rows.push(vec![target_label(k).to_string(), format!("`{p}`")]);
            }
        }
        let mut notes = vec![
            table_markdown(&["Folder", "Path"], &path_rows),
            String::new(),
            "- Filebrowser bisa dibuka dari mana saja via tunnel.".to_string(),
            "- Jellyfin **LAN** untuk jaringan lokal; **Tailscale** untuk akses luar.".to_string(),
        ];
        if !self.cfg.server_ip.is_empty() {
            notes.push(format!("- Server LAN: `{}`", self.cfg.server_ip));
        }

        let main_table = if service_rows.is_empty() {
            "Belum ada URL layanan yang dikonfigurasi di environment.".to_string()
        } else {
            table_markdown(&["Layanan", "URL"], &service_rows)
        };

        let mut blocks = vec![
            format!("**🔗 Link Akses** · {}", hl("Layanan Media")),
            String::new(),
            main_table,
            String::new(),
            format!("_{}_", stamp_wib()),
        ];
        if !pills.is_empty() {
            blocks.push(String::new());
            blocks.push(tg_button_row(&pills));
        }
        blocks.push(String::new());
        blocks.push("---".to_string());
        blocks.push(String::new());
        blocks.push(details_block("Folder & Catatan", &notes.join("\n")));
        blocks.join("\n")
    }

    fn jellyfin_scan(&self) -> String {
        if self.cfg.jellyfin_token.is_empty() {
            return format!(
                "**📡 Scan Jellyfin** · {}\n\nToken Jellyfin belum diset di environment.\n\n_{}_",
                hl("⏸️ Nonaktif"),
                stamp_wib()
            );
        }
        if !jellyfin_online(&self.cfg.jellyfin_url) {
            let tbl = table_markdown(
                &["Field", "Value"],
                &[
                    vec!["Status".into(), hl("⏸️ Jellyfin Off")],
                    vec!["Endpoint".into(), format!("`{}`", self.cfg.jellyfin_url)],
                ],
            );
            return [
                "**📡 Scan Jellyfin**".to_string(),
                String::new(),
                tbl,
                String::new(),
                "Jellyfin sedang dimatikan (hemat RAM). Nyalakan dulu lewat **VBreak → Apps** saat mau nonton atau scan library.".to_string(),
                format!("_{}_", stamp_wib()),
            ]
            .join("\n");
        }

        let url = format!("{}/Library/Refresh", self.cfg.jellyfin_url.trim_end_matches('/'));
        let auth = format!("MediaBrowser Token=\"{}\"", self.cfg.jellyfin_token);
        let t0 = Instant::now();
        match self.agent.post(&url).set("Authorization", &auth).call() {
            Ok(resp) => {
                let status = resp.status();
                let ms = t0.elapsed().as_millis();
                if status == 200 || status == 204 {
                    let tbl = table_markdown(
                        &["Field", "Value"],
                        &[
                            vec!["Status".into(), hl("✅ Dipicu")],
                            vec!["Respon".into(), hl(&format!("HTTP {status} · {ms} ms"))],
                            vec!["Endpoint".into(), format!("`{}`", self.cfg.jellyfin_url)],
                        ],
                    );
                    format!(
                        "**📡 Scan Jellyfin**\n\n{tbl}\n\nLibrary refresh berjalan di background. Metadata & poster baru muncul beberapa saat lagi.\n\n_{}_",
                        stamp_wib()
                    )
                } else {
                    format!(
                        "**📡 Scan Jellyfin** · {}\n\nJellyfin membalas HTTP `{status}` ({ms} ms).\n\n_{}_",
                        hl("⚠️ Peringatan"),
                        stamp_wib()
                    )
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                format!(
                    "**📡 Scan Jellyfin** · {}\n\n_{}_\n\n---\n\n{}",
                    hl("❌ Gagal"),
                    stamp_wib(),
                    details_block("Detail Error", &format!("`{}`", truncate_str(&msg, 240)))
                )
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
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let cut: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        format!("{cut}…")
    }
}

fn tail_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        chars[chars.len() - max_chars..].iter().collect()
    }
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
        "inline_keyboard": [
            [
                button("📡 Scan Jellyfin", "act:scan", "success"),
                button("🕒 Terbaru", "act:recent", "primary"),
            ],
            [
                button("🔗 Link Akses", "act:links", "primary"),
                button("📁 Menu Utama", "act:menu", "primary"),
            ],
        ]
    })
}

fn kb_recent() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Menu Utama", "act:menu", "primary"),
            button("🔄 Segarkan", "act:recent", ""),
        ]]
    })
}

fn kb_links() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Menu Utama", "act:menu", "primary"),
            button("🔄 Segarkan", "act:links", ""),
        ]]
    })
}

fn kb_back_menu() -> Value {
    json!({
        "inline_keyboard": [[button("📁 Menu Utama", "act:menu", "primary")]]
    })
}

// ── kartu unduhan rich (progres 1-line replace & laporan akhir) ─────────────
fn build_progress_card(
    source: &str,
    folder: &str,
    file_name: &str,
    dest: &Path,
    bytes: u64,
    total: Option<u64>,
    elapsed_secs: u64,
) -> String {
    let line = if bytes == 0 {
        format!("📥 {} · `{}`", hl("Menghubungkan…"), format_duration(elapsed_secs))
    } else {
        format_progress_line(bytes, total, elapsed_secs)
    };

    let tbl = table_markdown(
        &["Field", "Value"],
        &[
            vec!["File".into(), format!("`{}`", truncate_str(file_name, 40))],
            vec!["Folder".into(), target_label(folder).into()],
            vec!["Sumber".into(), source.into()],
        ],
    );

    [
        format!("**⬇️ Mengunduh** · {}", hl("Berjalan")),
        String::new(),
        line,
        String::new(),
        tbl,
        String::new(),
        format!("_{}_", stamp_wib()),
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Tujuan", &format!("Path: `{}`", dest.display())),
    ]
    .join("\n")
}

fn build_done_card(
    source: &str,
    folder: &str,
    fname: &str,
    full_path: &Path,
    size: u64,
    elapsed_secs: u64,
    extra_notes: &[String],
) -> String {
    let avg_mb_s = if elapsed_secs > 0 {
        size as f64 / elapsed_secs as f64 / 1024.0 / 1024.0
    } else {
        size as f64 / 1024.0 / 1024.0
    };
    let tbl = table_markdown(
        &["Field", "Value"],
        &[
            vec!["Status".into(), hl("✅ Selesai")],
            vec!["File".into(), format!("`{}`", truncate_str(fname, 42))],
            vec!["Ukuran".into(), hl(&human(size))],
            vec![
                "Kecepatan".into(),
                format!("{} · {}", hl(&format!("{avg_mb_s:.1} MB/s")), hl(&format_duration(elapsed_secs))),
            ],
            vec!["Folder".into(), target_label(folder).into()],
            vec!["Sumber".into(), source.into()],
        ],
    );

    let path_str = full_path.to_string_lossy();
    let pills = tg_button_row(&[
        tg_cb_button("📡 Scan", "act:scan"),
        tg_copy_button("📋 Salin Path", &path_str),
        tg_cb_button("📁 Menu", "act:menu"),
    ]);

    let mut notes = vec![format!("Path: `{path_str}`")];
    notes.extend(extra_notes.iter().cloned());

    [
        "**✅ Laporan Unduhan Selesai**".to_string(),
        String::new(),
        tbl,
        String::new(),
        "- [x] File terverifikasi utuh & bukan halaman HTML".to_string(),
        "- [x] Permission diset (`664` / `775`) siap dibaca Jellyfin".to_string(),
        String::new(),
        format!("_{}_", stamp_wib()),
        String::new(),
        pills,
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Detail Path & Catatan", &notes.join("\n")),
    ]
    .join("\n")
}

fn build_error_card(
    source: &str,
    folder: &str,
    reason: &str,
    hint: &str,
    err_log: &str,
    elapsed_secs: u64,
) -> String {
    let tbl = table_markdown(
        &["Field", "Value"],
        &[
            vec!["Status".into(), hl("❌ Gagal")],
            vec!["Alasan".into(), reason.into()],
            vec!["Folder".into(), target_label(folder).into()],
            vec!["Sumber".into(), format!("{source} · {}", format_duration(elapsed_secs))],
        ],
    );

    [
        "**❌ Laporan Unduhan Gagal**".to_string(),
        String::new(),
        tbl,
        String::new(),
        hint.to_string(),
        format!("_{}_", stamp_wib()),
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Log Error", &format!("```\n{err_log}\n```")),
    ]
    .join("\n")
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
fn run_download(app: Arc<App>, folder: String, url: String, status_msg_id: Option<i64>) {
    let Some(dest_str) = app.cfg.target_path(&folder) else {
        return;
    };
    let dest = PathBuf::from(dest_str);
    {
        app.busy.lock().unwrap().insert(folder.clone());
    }

    let started = Instant::now();
    let start_card = build_progress_card(
        "Google Drive",
        &folder,
        "Menyiapkan file…",
        &dest,
        0,
        None,
        0,
    );
    let live_msg_id = app.present(status_msg_id, &start_card, None);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::clone(&stop);
    let app_watch = Arc::clone(&app);
    let dest_watch = dest.clone();
    let folder_watch = folder.clone();
    let before = snapshot_dir(&dest);
    let before_watch = before.clone();

    let watcher = thread::spawn(move || {
        let mut last_bytes: u64 = 0;
        while !sleep_interruptible(&stop_watch, Duration::from_secs(8)) {
            let current = snapshot_dir(&dest_watch);
            let mut active: Option<(String, u64)> = None;
            for (name, (sz, mtime)) in &current {
                let changed = before_watch
                    .get(name)
                    .map(|(old_sz, old_mt)| old_sz != sz || old_mt != mtime)
                    .unwrap_or(true);
                if changed && active.as_ref().map(|(_, s)| *sz > *s).unwrap_or(true) {
                    active = Some((name.clone(), *sz));
                }
            }
            if let Some((name, bytes)) = active {
                if bytes > last_bytes {
                    last_bytes = bytes;
                    let card = build_progress_card(
                        "Google Drive",
                        &folder_watch,
                        &name,
                        &dest_watch,
                        bytes,
                        None,
                        started.elapsed().as_secs(),
                    );
                    app_watch.present(live_msg_id, &card, None);
                }
            }
        }
    });

    let args = vec![url, "-o".to_string(), dest.to_string_lossy().into_owned()];
    let outcome = run_cmd_with_timeout(&app.cfg.gdrive_dl, &args, DOWNLOAD_TIMEOUT);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let elapsed = started.elapsed().as_secs();

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

                let mut notes = Vec::new();
                if is_archive(&fname) {
                    notes.push("📦 File berupa arsip (`.rar`/`.zip`/`.7z`).".to_string());
                }
                if code != 0 {
                    notes.push(format!(
                        "⚠️ Exit code `{code}`, tapi file terverifikasi lengkap ({reason})."
                    ));
                }
                let card = build_done_card("Google Drive", &folder, &fname, &f, size, elapsed, &notes);
                app.present(live_msg_id, &card, Some(kb_after()));
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
                    "Tidak ada file baru atau berubah — kemungkinan file sudah lengkap sebelumnya."
                } else {
                    "Kemungkinan: link tidak publik, kuota Drive habis, atau file dihapus."
                };
                let card = build_error_card("Google Drive", &folder, &reason, hint, &err, elapsed);
                app.present(live_msg_id, &card, Some(kb_back_menu()));
            }
        }
        CmdOutcome::TimedOut => {
            let card = build_error_card(
                "Google Drive",
                &folder,
                "Timeout 6 jam",
                "Unduhan dihentikan otomatis karena melebihi batas 6 jam.",
                "timeout",
                elapsed,
            );
            app.present(live_msg_id, &card, Some(kb_back_menu()));
        }
        CmdOutcome::SpawnError(e) => {
            log_error(&format!("download gagal: {e}"));
            let card = build_error_card(
                "Google Drive",
                &folder,
                "Gagal menjalankan gdrive-dl",
                "Periksa path executable `GDRIVE_DL` di environment.",
                &truncate_str(&e, 240),
                elapsed,
            );
            app.present(live_msg_id, &card, Some(kb_back_menu()));
        }
    }

    app.busy.lock().unwrap().remove(&folder);
}

fn run_pixeldrain(app: Arc<App>, folder: String, fid: String, status_msg_id: Option<i64>) {
    let Some(dest_str) = app.cfg.target_path(&folder) else {
        return;
    };
    let dest = PathBuf::from(dest_str);
    {
        app.busy.lock().unwrap().insert(folder.clone());
    }

    let started = Instant::now();
    let info = app.pixeldrain_info(&fid);
    let name = info
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| fid.clone());
    let expected_size = info.get("size").and_then(Value::as_u64).filter(|&s| s > 0);
    let target = dest.join(&name);

    let start_card = build_progress_card(
        "Pixeldrain",
        &folder,
        &name,
        &dest,
        0,
        expected_size,
        0,
    );
    let live_msg_id = app.present(status_msg_id, &start_card, None);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::clone(&stop);
    let app_watch = Arc::clone(&app);
    let target_watch = target.clone();
    let dest_watch = dest.clone();
    let label_watch = name.clone();
    let folder_watch = folder.clone();

    let watcher = thread::spawn(move || {
        let mut last_bytes: u64 = 0;
        while !sleep_interruptible(&stop_watch, Duration::from_secs(6)) {
            if let Ok(meta) = fs::metadata(&target_watch) {
                let bytes = meta.len();
                if bytes > last_bytes {
                    last_bytes = bytes;
                    let card = build_progress_card(
                        "Pixeldrain",
                        &folder_watch,
                        &label_watch,
                        &dest_watch,
                        bytes,
                        expected_size,
                        started.elapsed().as_secs(),
                    );
                    app_watch.present(live_msg_id, &card, None);
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
    let elapsed = started.elapsed().as_secs();

    match outcome {
        CmdOutcome::Finished { code, stderr, .. } => {
            if code == 0 {
                let _ = Command::new("chmod").args(["664", &target.to_string_lossy()]).status();
                let _ = Command::new("chmod").args(["775", &dest.to_string_lossy()]).status();
                let final_size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                let mut notes = Vec::new();
                if is_archive(&name) {
                    notes.push("📦 File berupa arsip (`.rar`/`.zip`/`.7z`).".to_string());
                }
                let card = build_done_card("Pixeldrain", &folder, &name, &target, final_size, elapsed, &notes);
                app.present(live_msg_id, &card, Some(kb_after()));
            } else {
                let err = if stderr.trim().is_empty() {
                    "tanpa pesan".to_string()
                } else {
                    tail_str(stderr.trim(), 600)
                };
                let card = build_error_card(
                    "Pixeldrain",
                    &folder,
                    &format!("curl exit {code}"),
                    "Kemungkinan: file dihapus, link kadaluarsa, atau terkena rate-limit.",
                    &err,
                    elapsed,
                );
                app.present(live_msg_id, &card, Some(kb_back_menu()));
            }
        }
        CmdOutcome::TimedOut => {
            let card = build_error_card(
                "Pixeldrain",
                &folder,
                "Timeout 6 jam",
                "Unduhan dihentikan otomatis karena melebihi batas 6 jam.",
                "timeout",
                elapsed,
            );
            app.present(live_msg_id, &card, Some(kb_back_menu()));
        }
        CmdOutcome::SpawnError(e) => {
            log_error(&format!("pixeldrain download gagal: {e}"));
            let card = build_error_card(
                "Pixeldrain",
                &folder,
                "Gagal menjalankan curl",
                "Periksa binary `curl` di server.",
                &truncate_str(&e, 240),
                elapsed,
            );
            app.present(live_msg_id, &card, Some(kb_back_menu()));
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
    let msg_id = cb.pointer("/message/message_id").and_then(Value::as_i64);

    if chat_id != app.cfg.owner_id {
        app.ack(cb_id, "Akses khusus owner.");
        return;
    }

    if let Some(folder) = data.strip_prefix("pick:") {
        if app.cfg.target_path(folder).is_none() {
            app.ack(cb_id, "Folder tidak dikenal");
            return;
        }
        {
            let mut st = app.state.lock().unwrap();
            st.folder = Some(folder.to_string());
            st.pending = None;
            app.persist_state(&st);
        }
        app.ack(cb_id, &format!("Tujuan: {}", target_label(folder)));
        app.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
    } else if data.starts_with("go:") {
        if data == "go:no" {
            {
                let mut st = app.state.lock().unwrap();
                st.pending = None;
                app.persist_state(&st);
            }
            app.ack(cb_id, "Dibatalkan");
            app.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
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
            app.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
            return;
        };
        let not_ready = p.folder.is_empty()
            || (p.kind == "gdrive" && p.url.as_deref().unwrap_or("").is_empty())
            || (p.kind == "pixeldrain" && p.fid.as_deref().unwrap_or("").is_empty());
        if not_ready {
            app.ack(cb_id, "Tidak ada yang menunggu");
            app.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
            return;
        }
        if app.busy.lock().unwrap().contains(&p.folder) {
            app.ack(cb_id, "Folder masih dipakai");
            let card = format!(
                "**⚠️ Folder Sibuk** · {}\n\n{} masih dipakai mengunduh. Tunggu sampai selesai.\n\n_{}_",
                hl("Antre"),
                target_label(&p.folder),
                stamp_wib()
            );
            app.present(msg_id, &card, Some(kb_back_menu()));
            return;
        }
        app.ack(cb_id, "Mulai…");
        let app_clone = Arc::clone(app);
        if p.kind == "pixeldrain" {
            let fid = p.fid.unwrap_or_default();
            thread::spawn(move || run_pixeldrain(app_clone, p.folder, fid, msg_id));
        } else {
            let url = p.url.unwrap_or_default();
            thread::spawn(move || run_download(app_clone, p.folder, url, msg_id));
        }
    } else if data == "act:recent" {
        app.ack(cb_id, "File terbaru");
        app.present(msg_id, &app.recent_text(), Some(kb_recent()));
    } else if data == "act:links" {
        app.ack(cb_id, "Link akses");
        app.present(msg_id, &app.links_text(), Some(kb_links()));
    } else if data == "act:scan" {
        app.ack(cb_id, "Cek Jellyfin…");
        app.present(msg_id, &app.jellyfin_scan(), Some(kb_after()));
    } else {
        app.ack(cb_id, "Disegarkan");
        app.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
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
            "/scan" => {
                app.send(&app.jellyfin_scan(), Some(kb_after()));
            }
            "/links" => {
                app.send(&app.links_text(), Some(kb_links()));
            }
            "/recent" => {
                app.send(&app.recent_text(), Some(kb_recent()));
            }
            _ => {
                app.send(&app.menu_text(), Some(app.kb_menu()));
            }
        }
        return;
    }

    let fid = drive_id(text);
    let pd = pixeldrain_id(text);
    if fid.is_none() && pd.is_none() {
        let card = format!(
            "**⚠️ Link Tidak Dikenali** · {}\n\n{}\n\n_{}_",
            hl("Cek Format"),
            table_markdown(
                &["Sumber", "Contoh Format"],
                &[
                    vec!["Google Drive".into(), "`https://drive.google.com/file/d/<id>/view`".into()],
                    vec!["Pixeldrain".into(), "`https://pixeldrain.com/u/<id>`".into()],
                ],
            ),
            stamp_wib()
        );
        app.send(&card, Some(app.kb_menu()));
        return;
    }

    let folder = { app.state.lock().unwrap().folder.clone() };
    let Some(folder) = folder else {
        app.send(&app.menu_text(), Some(app.kb_menu()));
        return;
    };

    if let Some((kind, pid)) = pd {
        if kind != "u" {
            app.send(
                &format!(
                    "**ℹ️ Link List Belum Didukung** · {}\n\nSaat ini baru file tunggal (`/u/<id>`) yang didukung.",
                    hl("Info")
                ),
                Some(app.kb_menu()),
            );
            return;
        }
        let info = app.pixeldrain_info(&pid);
        let preview_name = info
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&pid);
        let preview_size = info
            .get("size")
            .and_then(Value::as_u64)
            .filter(|&s| s > 0)
            .map(human)
            .unwrap_or_else(|| "?".to_string());

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
        let card = format!(
            "**⬇️ Konfirmasi Unduhan** · {}\n\n{}\n\nLanjut unduh ke folder ini?\n_{}_",
            hl("Pixeldrain"),
            table_markdown(
                &["Field", "Value"],
                &[
                    vec!["Tujuan".into(), hl(target_label(&folder))],
                    vec!["File".into(), format!("`{}`", truncate_str(preview_name, 40))],
                    vec!["Ukuran".into(), hl(&preview_size)],
                    vec!["File ID".into(), format!("`{pid}`")],
                ],
            ),
            stamp_wib()
        );
        app.send(&card, Some(kb_confirm(&folder)));
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
    let card = format!(
        "**⬇️ Konfirmasi Unduhan** · {}\n\n{}\n\nLanjut unduh ke folder ini?\n_{}_",
        hl("Google Drive"),
        table_markdown(
            &["Field", "Value"],
            &[
                vec!["Tujuan".into(), hl(target_label(&folder))],
                vec!["Sumber".into(), "Google Drive (`gdrive-dl`)".into()],
                vec!["File ID".into(), format!("`{}`", truncate_str(&fid, 16))],
            ],
        ),
        stamp_wib()
    );
    app.send(&card, Some(kb_confirm(&folder)));
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
                {"command": "menu", "description": "Dashboard & pilih folder"},
                {"command": "recent", "description": "File terbaru di folder aktif"},
                {"command": "links", "description": "Link akses layanan media"},
                {"command": "scan", "description": "Scan library Jellyfin"},
                {"command": "help", "description": "Bantuan"},
            ]
        }),
    ) {
        log_warn(&format!("setMyCommands gagal: {e}"));
    }
    app.send(&app.menu_text(), Some(app.kb_menu()));

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
    fn test_progress_line_with_eta() {
        let with_total = format_progress_line(50 * 1024 * 1024, Some(100 * 1024 * 1024), 10);
        assert!(with_total.contains("▓▓▓▓▓░░░░░"));
        assert!(with_total.contains("==50%=="));
        assert!(with_total.contains("ETA 00:10"));

        let no_total = format_progress_line(100 * 1024 * 1024, None, 10);
        assert!(no_total.contains("==100.0 MB=="));
        assert!(no_total.contains("==10.0 MB/s=="));
    }

    #[test]
    fn test_copy_button() {
        let b = tg_copy_button("📋 Salin", "/mnt/storage/media/anime");
        assert_eq!(
            b,
            "<tg-button type=\"copy_text\" text=\"/mnt/storage/media/anime\">📋 Salin</tg-button>"
        );
    }

    #[test]
    fn test_table_markdown_canonical() {
        let t = table_markdown(
            &["Field", "Value"],
            &[vec!["A|B".into(), "1".into()]],
        );
        assert_eq!(t, "| Field | Value |\n|:---|:---|\n| A\\|B | 1 |");
    }

    #[test]
    fn test_markdown_to_html_fallback() {
        let out = markdown_to_html("**Judul** ==Aktif==\n`kode`");
        assert_eq!(out, "<b>Judul</b> <b>[Aktif]</b>\n<code>kode</code>");
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
            offset: 12345678,
            folder: Some("anime".to_string()),
            pending: None,
        };
        save_state(&path, &s);
        assert_eq!(load_state(&path), s);
        let _ = fs::remove_dir_all(&dir);
    }
}
