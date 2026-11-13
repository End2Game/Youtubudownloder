#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Rounding, Stroke, TextStyle};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Theme { Dark, Light, Midnight, Forest }

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Mode { Video, Audio }

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    theme: Theme,
    accent: [u8; 3],
    out_dir: String,
    mode: Mode,
    quality: u32,
    audio_fmt: String,
    max_parallel: usize,
    playlist: bool,
    subs: bool,
    thumb: bool,
    limit: String,
    auto_update: bool,
    sort_type: bool,
    by_channel: bool,
    name_tpl: u8,
    notify: bool,
    watch_clip: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            accent: [90, 140, 255],
            out_dir: "downloads".into(),
            mode: Mode::Video,
            quality: 1080,
            audio_fmt: "mp3".into(),
            max_parallel: 2,
            playlist: false,
            subs: false,
            thumb: true,
            limit: String::new(),
            auto_update: true,
            sort_type: true,
            by_channel: false,
            name_tpl: 0,
            notify: true,
            watch_clip: false,
        }
    }
}

#[derive(Clone, PartialEq)]
enum Status { Queued, Running, Paused, Done, Failed(String), Canceled }

#[derive(Clone)]
struct Item {
    id: u64,
    url: String,
    title: String,
    status: Status,
    percent: f32,
    speed: String,
    speed_bps: f64,
    eta: String,
    size: String,
    tag: String,
    child: Option<Arc<Mutex<Child>>>,
    cancel: bool,
    pause: bool,
    dur: String,
    res: String,
    domain: String,
    file: Option<String>,
    dir: String,
    removing: Option<f64>,
}

impl Item {
    fn new(id: u64, url: String, tag: String, dir: String) -> Self {
        Self {
            id, url, title: String::new(), status: Status::Queued, percent: 0.0,
            speed: String::new(), speed_bps: 0.0, eta: String::new(), size: String::new(),
            tag, child: None, cancel: false, pause: false,
            dur: String::new(), res: String::new(), domain: String::new(), file: None, dir, removing: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedItem { url: String, title: String, tag: String, dir: String, percent: f32, state: u8, err: String }

const NAME_TPLS: [(&str, &str); 3] = [
    ("Назва", "%(title).150s"),
    ("Назва [id]", "%(title).130s [%(id)s]"),
    ("Канал — Назва", "%(uploader).60s - %(title).110s"),
];

fn target_dir(s: &Settings) -> String {
    let sub = if !s.sort_type { "" } else if s.mode == Mode::Video { "/Відео" } else { "/Музика" };
    format!("{}{}", s.out_dir.trim_end_matches(['/', '\\']), sub)
}

fn is_na(s: &str) -> bool {
    let s = s.trim();
    s.is_empty() || s == "None" || s.contains("NA") || s.contains("N/A")
}

fn extract_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|t| {
            let t = t.strip_prefix("URL=").unwrap_or(t);
            if t.starts_with("http://") || t.starts_with("https://") { Some(t.to_string()) } else { None }
        })
        .collect()
}

fn is_url(t: &str) -> bool {
    (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace)
}

fn domain_of(url: &str) -> String {
    let u = url.split("://").nth(1).unwrap_or(url);
    u.split('/').next().unwrap_or(u).trim_start_matches("www.").to_string()
}

fn notify(title: &str, body: &str) {
    let (t, b) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new().appname("Grab").summary(&t).body(&b).show();
    });
}

type Shared = Arc<Mutex<Vec<Item>>>;

fn with_item(items: &Shared, id: u64, f: impl FnOnce(&mut Item)) {
    if let Some(it) = items.lock().unwrap().iter_mut().find(|i| i.id == id) {
        f(it);
    }
}

fn tool_cmd(name: impl AsRef<OsStr>) -> Command {
    let mut c = Command::new(name);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // без чорного вікна
    }
    c
}

fn has_tool(name: impl AsRef<OsStr>) -> bool {
    tool_cmd(name).arg("--version").output().is_ok()
}

// ---------- Вбудовані компоненти (yt-dlp + ffmpeg) ----------

fn exe_name(n: &str) -> String {
    if cfg!(windows) { format!("{n}.exe") } else { n.to_string() }
}

fn bin_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("Grab").join("bin")
}

fn ytdlp_path() -> PathBuf {
    let p = bin_dir().join(exe_name("yt-dlp"));
    if p.exists() { p } else { PathBuf::from("yt-dlp") }
}

fn ffmpeg_dir() -> Option<PathBuf> {
    let d = bin_dir();
    if d.join(exe_name("ffmpeg")).exists() { Some(d) } else { None }
}

fn ytdlp_url() -> &'static str {
    if cfg!(windows) {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
    } else if cfg!(target_os = "macos") {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos"
    } else {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux"
    }
}

fn ffmpeg_url() -> Option<(&'static str, &'static str)> {
    if cfg!(windows) {
        Some(("https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip", "zip"))
    } else if cfg!(target_os = "macos") {
        None
    } else {
        Some(("https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz", "tar.xz"))
    }
}

#[derive(Default)]
struct Install { busy: bool, msg: String, progress: f32, err: Option<String>, finished: bool }
type InstallState = Arc<Mutex<Install>>;

fn set_inst(st: &InstallState, f: impl FnOnce(&mut Install)) {
    f(&mut st.lock().unwrap());
}

fn download(url: &str, dest: &Path, st: &InstallState, ctx: &egui::Context, label: &str, lo: f32, hi: f32) -> Result<(), String> {
    set_inst(st, |s| s.msg = label.to_string());
    let resp = ureq::get(url).call().map_err(|e| format!("{label}: {e}"))?;
    let total: f64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let mut r = resp.into_reader();
    let mut f = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 65536];
    let mut got = 0f64;
    loop {
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as f64;
        if total > 0.0 {
            let p = lo + (hi - lo) * (got / total) as f32;
            set_inst(st, |s| s.progress = p);
            ctx.request_repaint();
        }
    }
    Ok(())
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) { return Some(f); }
        } else if p.file_name().map(|n| n.to_string_lossy() == name).unwrap_or(false) {
            return Some(p);
        }
    }
    None
}

#[cfg(unix)]
fn make_exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn make_exec(_: &Path) {}

fn install_all(st: &InstallState, ctx: &egui::Context) -> Result<(), String> {
    let dir = bin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let need_y = !dir.join(exe_name("yt-dlp")).exists() && !has_tool("yt-dlp");
    let need_f = !dir.join(exe_name("ffmpeg")).exists() && !has_tool("ffmpeg");

    if need_y {
        let dest = dir.join(exe_name("yt-dlp"));
        download(ytdlp_url(), &dest, st, ctx, "Завантажую yt-dlp...", 0.0, if need_f { 0.2 } else { 1.0 })?;
        make_exec(&dest);
    }
    if need_f {
        let (url, ext) = ffmpeg_url().ok_or("На macOS встанови ffmpeg вручну: brew install ffmpeg")?;
        let pkg = dir.join(format!("ffmpeg_pkg.{ext}"));
        download(url, &pkg, st, ctx, "Завантажую ffmpeg (~100 МБ)...", if need_y { 0.2 } else { 0.0 }, 0.95)?;
        set_inst(st, |s| s.msg = "Розпаковую ffmpeg...".into());
        let tmp = dir.join("tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let ok = tool_cmd("tar").arg("-xf").arg(&pkg).arg("-C").arg(&tmp)
            .status().map(|s| s.success()).unwrap_or(false);
        if !ok { return Err("Не вдалося розпакувати ffmpeg (потрібна команда tar)".into()); }
        for n in ["ffmpeg", "ffprobe"] {
            if let Some(p) = find_file(&tmp, &exe_name(n)) {
                let dest = dir.join(exe_name(n));
                std::fs::copy(&p, &dest).map_err(|e| e.to_string())?;
                make_exec(&dest);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&pkg);
    }
    Ok(())
}

fn start_install(st: InstallState, ctx: egui::Context) {
    if st.lock().unwrap().busy { return; }
    set_inst(&st, |s| { s.busy = true; s.err = None; s.progress = 0.0; s.msg = "Підготовка...".into(); });
    std::thread::spawn(move || {
        let r = install_all(&st, &ctx);
        set_inst(&st, |s| { s.busy = false; s.finished = true; s.err = r.err(); });
        ctx.request_repaint();
    });
}

fn parse_speed(s: &str) -> f64 {
    let s = s.trim().trim_end_matches("/s");
    let idx = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let num: f64 = s[..idx].trim().parse().unwrap_or(0.0);
    let mult = match s[idx..].to_lowercase().as_str() {
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        _ => 1.0,
    };
    num * mult
}

fn fmt_speed(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.2} MiB/s", bps / 1048576.0)
    } else {
        format!("{:.0} KiB/s", bps / 1024.0)
    }
}

fn run_download(id: u64, url: String, s: Settings, items: Shared, ctx: egui::Context) {
    let tmp = std::env::temp_dir().join(format!("grab-{}-{}.txt", std::process::id(), id));
    let _ = std::fs::remove_file(&tmp);
    let chan = if s.by_channel { "/%(uploader|Різне)s" } else { "" };
    let tpl = NAME_TPLS.get(s.name_tpl as usize).map(|t| t.1).unwrap_or(NAME_TPLS[0].1);
    let mut cmd = tool_cmd(ytdlp_path());
    cmd.args([
        "--newline",
        "--no-colors",
        "--progress-template",
        "download:PROG|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s|%(progress._total_bytes_str)s|%(info.duration_string)s|%(info.width)sx%(info.height)s|%(info.webpage_url_domain)s",
        "-o",
    ])
    .arg(format!("{}{}/{}.%(ext)s", target_dir(&s), chan, tpl))
    .args(["--windows-filenames", "--print-to-file", "after_move:%(filepath)s"])
    .arg(&tmp)
    .arg("--embed-metadata");
    if let Some(d) = ffmpeg_dir() {
        cmd.arg("--ffmpeg-location").arg(d);
    }
    if s.thumb {
        cmd.arg("--embed-thumbnail");
    }
    if !s.playlist {
        cmd.arg("--no-playlist");
    }
    if s.subs && s.mode == Mode::Video {
        cmd.args(["--write-subs", "--embed-subs", "--sub-langs", "uk,ru,en"]);
    }
    if !s.limit.trim().is_empty() {
        cmd.args(["--limit-rate", s.limit.trim()]);
    }
    match s.mode {
        Mode::Video => {
            let q = s.quality;
            cmd.args(["-f", &format!("bv*[height<={q}]+ba/b[height<={q}]"), "--merge-output-format", "mp4"]);
        }
        Mode::Audio => {
            // Лише аудіо-доріжка: відео взагалі не завантажується
            cmd.args(["-f", "bestaudio/best", "-x", "--audio-format", &s.audio_fmt, "--audio-quality", "0"]);
        }
    }
    cmd.arg(&url).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            with_item(&items, id, |it| it.status = Status::Failed(format!("Не запустився yt-dlp: {e}")));
            ctx.request_repaint();
            return;
        }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let child = Arc::new(Mutex::new(child));
    with_item(&items, id, |it| {
        it.child = Some(child.clone());
        if it.pause || it.cancel { let _ = child.lock().unwrap().kill(); }
    });

    let last_err = Arc::new(Mutex::new(String::new()));
    let le = last_err.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().flatten() {
            if line.contains("ERROR") {
                *le.lock().unwrap() = line;
            }
        }
    });

    for line in BufReader::new(stdout).lines().flatten() {
        if let Some(rest) = line.strip_prefix("PROG|") {
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() >= 4 {
                let pct = p[0].trim().trim_end_matches('%').parse::<f32>().unwrap_or(0.0);
                let extra: Option<(String, String, String)> = if p.len() >= 7 {
                    Some((p[4].trim().to_string(), p[5].trim().to_string(), p[6].trim().to_string()))
                } else { None };
                with_item(&items, id, |it| {
                    if let Some((d, r, dom)) = extra {
                        if !is_na(&d) { it.dur = d; }
                        if !is_na(&r) { it.res = r; }
                        if !dom.is_empty() && !dom.starts_with("NA") && dom != "None" { it.domain = dom; }
                    }
                    it.percent = pct;
                    it.speed = p[1].trim().to_string();
                    it.speed_bps = parse_speed(p[1]);
                    it.eta = p[2].trim().to_string();
                    it.size = p[3].trim().to_string();
                });
            }
        } else if let Some(path) = line.strip_prefix("[download] Destination:") {
            let name = std::path::Path::new(path.trim())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            with_item(&items, id, |it| { it.title = name; it.percent = 0.0; });
        }
        ctx.request_repaint();
    }

    let ok = child.lock().unwrap().wait().map(|s| s.success()).unwrap_or(false);
    let err = last_err.lock().unwrap().clone();
    let file = std::fs::read_to_string(&tmp)
        .ok()
        .and_then(|t| t.lines().filter(|l| !l.trim().is_empty()).last().map(|l| l.trim().to_string()));
    let _ = std::fs::remove_file(&tmp);
    with_item(&items, id, |it| {
        it.child = None;
        it.speed_bps = 0.0;
        it.speed.clear();
        it.eta.clear();
        if file.is_some() { it.file = file; }
        it.status = if it.pause && !ok {
            Status::Paused
        } else if it.cancel {
            Status::Canceled
        } else if ok {
            it.percent = 100.0;
            Status::Done
        } else {
            Status::Failed(if err.is_empty() { "Помилка завантаження".into() } else { err })
        };
    });
    ctx.request_repaint();
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "End2Game/Youtubudownloder";
const ACCENTS: [[u8; 3]; 5] = [[90, 140, 255], [229, 87, 155], [62, 194, 143], [245, 166, 35], [160, 110, 255]];

#[derive(Clone, Copy, PartialEq)]
enum Tab { Queue, History, Settings }

#[derive(Clone, Copy, PartialEq)]
enum Filter { All, Active, Done, Failed }

#[derive(Clone, Serialize, Deserialize)]
struct HistEntry { title: String, url: String, tag: String }

#[derive(Clone)]
enum UpStage {
    Idle,
    Checking,
    UpToDate,
    CheckFailed(String),
    Available { ver: String, url: String },
    Downloading(f32),
    Failed(String),
    Closing,
}
type UpState = Arc<Mutex<UpStage>>;

fn asset_name() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("grab-windows-x64.exe")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("grab-linux-x64")
    } else {
        None
    }
}

fn parse_ver(s: &str) -> Vec<u32> {
    let mut v: Vec<u32> = s
        .trim()
        .trim_start_matches(|c| c == 'v' || c == 'V')
        .split('.')
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
        .collect();
    while v.len() < 3 { v.push(0); }
    v
}

fn is_newer(remote: &str, local: &str) -> bool {
    parse_ver(remote) > parse_ver(local)
}

/// Видаляє залишок старої версії після оновлення
fn cleanup_old_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
        let _ = std::fs::remove_file(exe.with_extension("new"));
    }
}

fn check_update(st: UpState, ctx: egui::Context) {
    let Some(asset) = asset_name() else { return };
    *st.lock().unwrap() = UpStage::Checking;
    std::thread::spawn(move || {
        let res = (|| -> Result<Option<(String, String)>, String> {
            let body = ureq::get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
                .set("User-Agent", "grab-updater")
                .set("Accept", "application/vnd.github+json")
                .timeout(Duration::from_secs(15))
                .call()
                .map_err(|e| e.to_string())?
                .into_string()
                .map_err(|e| e.to_string())?;
            let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let tag = v["tag_name"].as_str().unwrap_or("").to_string();
            if !is_newer(&tag, VERSION) {
                return Ok(None);
            }
            let url = v["assets"]
                .as_array()
                .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(asset)))
                .and_then(|x| x["browser_download_url"].as_str())
                .map(String::from)
                .ok_or_else(|| "У релізі ще немає файлу для цієї системи".to_string())?;
            Ok(Some((tag, url)))
        })();
        *st.lock().unwrap() = match res {
            Ok(Some((ver, url))) => UpStage::Available { ver, url },
            Ok(None) => UpStage::UpToDate,
            Err(e) => UpStage::CheckFailed(e),
        };
        ctx.request_repaint();
    });
}

fn install_update(st: UpState, ctx: egui::Context, url: String) {
    std::thread::spawn(move || {
        let res = (|| -> Result<(), String> {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let new = exe.with_extension("new");
            let old = exe.with_extension("old");
            let resp = ureq::get(&url)
                .set("User-Agent", "grab-updater")
                .call()
                .map_err(|e| format!("Не вдалося завантажити оновлення: {e}"))?;
            let total: f32 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let mut reader = resp.into_reader();
            let mut f = std::fs::File::create(&new).map_err(|e| format!("Не вдалося записати файл: {e}"))?;
            let mut buf = [0u8; 65536];
            let mut got = 0f32;
            loop {
                let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 { break; }
                f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                got += n as f32;
                if total > 0.0 {
                    *st.lock().unwrap() = UpStage::Downloading((got / total).min(1.0));
                    ctx.request_repaint();
                }
            }
            drop(f);
            if got < 1_000_000.0 {
                let _ = std::fs::remove_file(&new);
                return Err("Файл оновлення пошкоджений або неповний".to_string());
            }
            make_exec(&new);
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&exe, &old).map_err(|e| format!("Не вдалося замінити програму: {e}"))?;
            if let Err(e) = std::fs::rename(&new, &exe) {
                let _ = std::fs::rename(&old, &exe);
                return Err(format!("Не вдалося замінити програму: {e}"));
            }
            Command::new(&exe).spawn().map_err(|e| format!("Не вдалося перезапустити: {e}"))?;
            Ok(())
        })();
        *st.lock().unwrap() = match res {
            Ok(()) => UpStage::Closing,
            Err(e) => UpStage::Failed(e),
        };
        ctx.request_repaint();
    });
}

struct App {
    s: Settings,
    input: String,
    items: Shared,
    next_id: u64,
    ytdlp_ok: bool,
    ffmpeg_ok: bool,
    install: InstallState,
    tab: Tab,
    history: Vec<HistEntry>,
    recorded: HashSet<u64>,
    up: UpState,
    filter: Filter,
    menu: Option<(u64, egui::Pos2)>,
    notice: Option<(String, f64)>,
    last_clip: String,
    clip_check: f64,
    was_busy: bool,
    base_done: usize,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let s: Settings = cc.storage.and_then(|st| eframe::get_value(st, "settings")).unwrap_or_default();
        let history: Vec<HistEntry> = cc.storage.and_then(|st| eframe::get_value(st, "history")).unwrap_or_default();
        setup_fonts(&cc.egui_ctx);
        cleanup_old_exe();
        let up: UpState = Arc::new(Mutex::new(UpStage::Idle));
        check_update(up.clone(), cc.egui_ctx.clone());
        let ytdlp_ok = has_tool(ytdlp_path());
        let ffmpeg_ok = ffmpeg_dir().is_some() || has_tool("ffmpeg");
        let install: InstallState = Arc::new(Mutex::new(Install::default()));
        if !ytdlp_ok || !ffmpeg_ok {
            start_install(install.clone(), cc.egui_ctx.clone()); // автоустановка при першому запуску
        }
        // відновлення черги після перезапуску
        let saved: Vec<SavedItem> = cc.storage.and_then(|st| eframe::get_value(st, "queue")).unwrap_or_default();
        let mut restored = Vec::new();
        let mut next_id = 1u64;
        for sv in saved {
            let mut it = Item::new(next_id, sv.url, sv.tag, sv.dir);
            it.title = sv.title;
            it.percent = sv.percent;
            it.status = match sv.state { 0 => Status::Queued, 1 => Status::Paused, _ => Status::Failed(sv.err) };
            restored.push(it);
            next_id += 1;
        }
        let last_clip = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()).unwrap_or_default().trim().to_string();
        Self {
            s,
            input: String::new(),
            items: Arc::new(Mutex::new(restored)),
            next_id,
            filter: Filter::All,
            menu: None,
            notice: None,
            last_clip,
            clip_check: 0.0,
            was_busy: false,
            base_done: 0,
            ytdlp_ok,
            ffmpeg_ok,
            install,
            tab: Tab::Queue,
            history,
            recorded: HashSet::new(),
            up,
        }
    }

    fn accent(&self) -> Color32 {
        let c = Color32::from_rgb(self.s.accent[0], self.s.accent[1], self.s.accent[2]);
        if self.s.theme == Theme::Light { mix(c, Color32::BLACK, 0.25) } else { c }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        let mut v = if self.s.theme == Theme::Light { egui::Visuals::light() } else { egui::Visuals::dark() };
        match self.s.theme {
            Theme::Midnight => {
                v.panel_fill = Color32::from_rgb(10, 12, 26);
                v.window_fill = Color32::from_rgb(14, 16, 32);
                v.extreme_bg_color = Color32::from_rgb(5, 6, 15);
            }
            Theme::Forest => {
                v.panel_fill = Color32::from_rgb(16, 27, 21);
                v.window_fill = Color32::from_rgb(20, 33, 26);
                v.extreme_bg_color = Color32::from_rgb(9, 17, 13);
            }
            _ => {}
        }
        let a = self.accent();
        let light = self.s.theme == Theme::Light;
        let base = v.panel_fill;
        // яскравий основний текст і помітний приглушений
        let text = if light { Color32::from_rgb(18, 22, 34) } else { Color32::from_rgb(238, 241, 250) };
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.fg_stroke.color = text;
        }
        // у egui приглушений текст = середина між текстом і цим кольором
        v.widgets.noninteractive.weak_bg_fill = mix(base, text, 0.40);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, mix(base, text, 0.20));
        let r = Rounding::same(8.0);

        // вибрані кнопки: насичений фон + світла рамка й текст
        v.selection.bg_fill = mix(base, a, if light { 0.30 } else { 0.45 });
        v.selection.stroke = Stroke::new(1.5_f32, if light { mix(a, Color32::BLACK, 0.35) } else { mix(a, Color32::WHITE, 0.70) });
        v.hyperlink_color = a;
        v.window_rounding = Rounding::same(12.0);
        v.menu_rounding = Rounding::same(8.0);

        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.rounding = r;
        }
        v.widgets.inactive.bg_fill = mix(base, text, 0.14);
        v.widgets.inactive.weak_bg_fill = mix(base, text, 0.12);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, mix(base, text, 0.28));
        v.widgets.hovered.bg_fill = mix(base, a, 0.30);
        v.widgets.hovered.weak_bg_fill = mix(base, a, 0.25);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, a);
        v.widgets.active.bg_fill = mix(base, a, 0.50);
        v.widgets.active.weak_bg_fill = mix(base, a, 0.45);
        v.widgets.active.bg_stroke = Stroke::new(1.0_f32, a);
        ctx.set_visuals(v);

        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.slider_width = 120.0;
            s.text_styles.insert(TextStyle::Heading, FontId::new(26.0, f_bold()));
            s.text_styles.insert(TextStyle::Body, FontId::new(15.5, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Button, FontId::new(15.5, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Small, FontId::new(13.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace));
        });
    }

    fn add_list(&mut self, urls: Vec<String>) -> (usize, usize) {
        let tag = match self.s.mode {
            Mode::Video => format!("MP4 {}p", self.s.quality),
            Mode::Audio => self.s.audio_fmt.to_uppercase(),
        };
        let dir = target_dir(&self.s);
        let mut items = self.items.lock().unwrap();
        // не додаємо те, що вже в черзі / качається / на паузі, та дублі всередині пачки
        let mut seen: HashSet<String> = items
            .iter()
            .filter(|i| matches!(i.status, Status::Queued | Status::Running | Status::Paused))
            .map(|i| i.url.clone())
            .collect();
        let (mut added, mut skipped) = (0, 0);
        for u in urls {
            let u = u.trim().to_string();
            if u.is_empty() { continue; }
            if !seen.insert(u.clone()) { skipped += 1; continue; }
            items.push(Item::new(self.next_id, u, tag.clone(), dir.clone()));
            self.next_id += 1;
            added += 1;
        }
        (added, skipped)
    }

    fn add_urls(&mut self) -> (usize, usize) {
        let urls: Vec<String> = self.input.lines().map(|l| l.to_string()).collect();
        self.input.clear();
        self.add_list(urls)
    }

    fn start_queued(&self, ctx: &egui::Context) {
        if !self.ytdlp_ok || !self.ffmpeg_ok { return; }
        let mut to_start = Vec::new();
        {
            let mut items = self.items.lock().unwrap();
            let mut running = items.iter().filter(|i| i.status == Status::Running).count();
            for it in items.iter_mut() {
                if running >= self.s.max_parallel { break; }
                if it.status == Status::Queued && it.removing.is_none() {
                    it.status = Status::Running;
                    it.pause = false;
                    it.cancel = false;
                    to_start.push((it.id, it.url.clone()));
                    running += 1;
                }
            }
        }
        for (id, url) in to_start {
            let (s, items, ctx) = (self.s.clone(), self.items.clone(), ctx.clone());
            std::thread::spawn(move || run_download(id, url, s, items, ctx));
        }
    }
}


// ---------- Шрифти ----------

fn f_bold() -> FontFamily { FontFamily::Name("bold".into()) }
fn f_mono_bold() -> FontFamily { FontFamily::Name("mono-bold".into()) }
fn f_semi() -> FontFamily { FontFamily::Name("semi".into()) }

/// Справжня жирність шрифту (egui `.strong()` лише підсвічує колір, а не робить текст жирним)
trait Weight {
    fn b(self) -> RichText;
    fn semi(self) -> RichText;
}
impl Weight for RichText {
    fn b(self) -> RichText { self.family(f_bold()) }
    fn semi(self) -> RichText { self.family(f_semi()) }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut d = egui::FontDefinitions::default();
    let add = |d: &mut egui::FontDefinitions, name: &str, bytes: &'static [u8]| {
        d.font_data.insert(name.to_owned(), egui::FontData::from_static(bytes));
    };
    add(&mut d, "inter", include_bytes!("../assets/fonts/Inter-Regular.ttf"));
    add(&mut d, "inter-semi", include_bytes!("../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut d, "inter-bold", include_bytes!("../assets/fonts/Inter-Bold.ttf"));
    add(&mut d, "jb-mono", include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"));
    add(&mut d, "jb-mono-bold", include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"));

    // стандартні шрифти egui лишаються запасними (емодзі та символи)
    let prop_fallback = d.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_fallback = d.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &str, rest: &[String]| {
        let mut v = vec![first.to_owned()];
        v.extend(rest.iter().cloned());
        v
    };
    d.families.insert(FontFamily::Proportional, with("inter", &prop_fallback));
    d.families.insert(FontFamily::Monospace, with("jb-mono", &mono_fallback));
    d.families.insert(f_semi(), with("inter-semi", &prop_fallback));
    d.families.insert(f_bold(), with("inter-bold", &prop_fallback));
    d.families.insert(FontFamily::Name("mono-bold".into()), with("jb-mono-bold", &mono_fallback));
    ctx.set_fonts(d);
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Колір тексту, що добре читається на заданому фоні
fn on_color(c: Color32) -> Color32 {
    let lum = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    if lum > 150.0 { Color32::from_rgb(20, 22, 30) } else { Color32::WHITE }
}

/// Кольорова (залита) кнопка з ефектами наведення/натискання
fn color_button(ui: &mut egui::Ui, label: &str, color: Color32, height: f32) -> egui::Response {
    ui.scope(|ui| {
        let fg = on_color(color);
        let w = &mut ui.style_mut().visuals.widgets;
        w.inactive.weak_bg_fill = color;
        w.hovered.weak_bg_fill = mix(color, Color32::WHITE, 0.15);
        w.active.weak_bg_fill = mix(color, Color32::BLACK, 0.15);
        for st in [&mut w.inactive, &mut w.hovered, &mut w.active] {
            st.bg_stroke = Stroke::NONE;
            st.fg_stroke.color = fg;
        }
        ui.add(egui::Button::new(RichText::new(label).b()).min_size(egui::vec2(0.0, height)))
    })
    .inner
}

/// Звичайна (приглушена) кнопка
fn soft_button(ui: &mut egui::Ui, label: &str, height: f32) -> egui::Response {
    ui.add(egui::Button::new(label).min_size(egui::vec2(0.0, height)))
}

fn chip(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    ui.add(egui::Button::new(label).selected(on).rounding(14.0).min_size(egui::vec2(0.0, 28.0)))
}

fn icon_btn(ui: &mut egui::Ui, icon: &str, tip: &str, color: Option<Color32>) -> egui::Response {
    let mut t = RichText::new(icon).size(15.0);
    if let Some(c) = color { t = t.color(c); }
    ui.add(egui::Button::new(t).min_size(egui::vec2(32.0, 32.0))).on_hover_text(tip)
}

fn section(ui: &mut egui::Ui, accent: Color32, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text.to_uppercase()).small().b().color(accent));
}

fn nav_item(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool, accent: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(104.0, 66.0), egui::Sense::click());
    let base = ui.visuals().panel_fill;
    if selected || resp.hovered() {
        let fill = if selected { mix(base, accent, 0.22) } else { mix(base, ui.visuals().text_color(), 0.06) };
        ui.painter().rect_filled(rect, 14.0, fill);
        if selected { ui.painter().rect_stroke(rect, 14.0, Stroke::new(1.0_f32, mix(base, accent, 0.6))); }
    }
    let fg = if selected { accent } else { ui.visuals().weak_text_color() };
    let fg2 = if selected { ui.visuals().text_color() } else { fg };
    ui.painter().text(rect.center() - egui::vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, icon, FontId::proportional(24.0), fg);
    ui.painter().text(rect.center() + egui::vec2(0.0, 18.0), egui::Align2::CENTER_CENTER, label, FontId::new(12.5, f_semi()), fg2);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let base = ui.visuals().panel_fill;
    egui::Frame::none()
        .fill(mix(base, color, 0.22))
        .stroke(Stroke::new(1.0_f32, mix(base, color, 0.5)))
        .rounding(14.0)
        .inner_margin(egui::Margin::symmetric(10.0, 4.0))
        .show(ui, |ui| { ui.label(RichText::new(text).semi().size(13.0).color(color)); });
}

fn info_chip(ui: &mut egui::Ui, label: &str, value: &str, vcol: Option<Color32>) {
    let base = ui.visuals().panel_fill;
    let text = ui.visuals().text_color();
    egui::Frame::none()
        .stroke(Stroke::new(1.0_f32, mix(base, text, 0.30)))
        .rounding(7.0)
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).size(12.0).weak());
                let mut v = RichText::new(value).family(FontFamily::Monospace).size(12.5);
                if let Some(c) = vcol { v = v.color(c); }
                ui.label(v);
            });
        });
}

fn open_folder(path: &str) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(target_os = "windows")]
    let prog = "explorer";
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let prog = "xdg-open";
    let _ = Command::new(prog).arg(path).spawn();
}

fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    { let _ = tool_cmd("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn(); }
    #[cfg(target_os = "macos")]
    { let _ = Command::new("open").arg(url).spawn(); }
    #[cfg(all(unix, not(target_os = "macos")))]
    { let _ = Command::new("xdg-open").arg(url).spawn(); }
}

fn open_path(path: &str) {
    #[cfg(target_os = "windows")]
    { let _ = tool_cmd("cmd").args(["/C", "start", "", path]).spawn(); }
    #[cfg(target_os = "macos")]
    { let _ = Command::new("open").arg(path).spawn(); }
    #[cfg(all(unix, not(target_os = "macos")))]
    { let _ = Command::new("xdg-open").arg(path).spawn(); }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "settings", &self.s);
        eframe::set_value(storage, "history", &self.history);
        let saved: Vec<SavedItem> = self.items.lock().unwrap().iter().filter_map(|i| {
            if i.removing.is_some() { return None; }
            let state = match &i.status {
                Status::Queued | Status::Running => 0,
                Status::Paused => 1,
                Status::Failed(_) => 2,
                _ => return None,
            };
            let err = if let Status::Failed(e) = &i.status { e.clone() } else { String::new() };
            Some(SavedItem { url: i.url.clone(), title: i.title.clone(), tag: i.tag.clone(), dir: i.dir.clone(), percent: i.percent, state, err })
        }).collect();
        eframe::set_value(storage, "queue", &saved);
    }

    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        let fin = std::mem::take(&mut self.install.lock().unwrap().finished);
        if fin {
            self.ytdlp_ok = has_tool(ytdlp_path());
            self.ffmpeg_ok = ffmpeg_dir().is_some() || has_tool("ffmpeg");
        }
        self.apply_theme(ctx);
        self.start_queued(ctx);
        let accent = self.accent();
        let light = self.s.theme == Theme::Light;
        let green = if light { Color32::from_rgb(14, 130, 66) } else { Color32::from_rgb(90, 220, 135) };
        let red = if light { Color32::from_rgb(190, 34, 44) } else { Color32::from_rgb(255, 105, 105) };
        let snapshot: Vec<Item> = self.items.lock().unwrap().clone();

        let now = ctx.input(|i| i.time);

        // гарячі клавіші
        let (k_add, k_paste, k_t1, k_t2, k_t3) = ctx.input_mut(|i| (
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter),
            i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::V),
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num1),
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num2),
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num3),
        ));
        if k_t1 { self.tab = Tab::Queue; }
        if k_t2 { self.tab = Tab::History; }
        if k_t3 { self.tab = Tab::Settings; }

        // перетягування файлів (.txt / .url) із посиланнями
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        let mut dropped_urls: Vec<String> = Vec::new();
        for f in &dropped {
            let text = if let Some(p) = &f.path {
                match std::fs::metadata(p) {
                    Ok(m) if m.len() < 1_000_000 => std::fs::read_to_string(p).ok(),
                    _ => None,
                }
            } else if let Some(b) = &f.bytes {
                Some(String::from_utf8_lossy(b).to_string())
            } else { None };
            if let Some(t) = text { dropped_urls.extend(extract_urls(&t)); }
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_overlay")));
            let r = ctx.screen_rect();
            painter.rect_filled(r, 0.0, Color32::from_black_alpha(170));
            painter.text(r.center(), egui::Align2::CENTER_CENTER, "⬇  Відпусти файл із посиланнями (.txt, .url)", FontId::new(22.0, f_bold()), Color32::WHITE);
        }

        // автопідхоплення посилання з буфера обміну
        if self.s.watch_clip && ctx.input(|i| i.focused) {
            if now - self.clip_check > 0.8 {
                self.clip_check = now;
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if let Ok(t) = cb.get_text() {
                        let t = t.trim().to_string();
                        if t != self.last_clip {
                            self.last_clip = t.clone();
                            if is_url(&t) && !self.input.lines().any(|l| l.trim() == t) {
                                if !self.input.is_empty() && !self.input.ends_with('\n') { self.input.push('\n'); }
                                self.input.push_str(&t);
                                self.notice = Some(("🔗 Посилання з буфера додано в поле".to_string(), now + 4.0));
                            }
                        }
                    }
                }
            }
            ctx.request_repaint_after(Duration::from_millis(800));
        }

        // сповіщення, коли вся черга завершилась
        {
            let busy_now = self.items.lock().unwrap().iter().any(|i| matches!(i.status, Status::Running | Status::Queued));
            if busy_now && !self.was_busy { self.base_done = self.recorded.len(); }
            if self.was_busy && !busy_now && self.s.notify {
                let delta = self.recorded.len().saturating_sub(self.base_done);
                if delta > 0 { notify("Grab — готово", &format!("Завершено завантажень: {delta}")); }
            }
            self.was_busy = busy_now;
        }

        // історія завершених завантажень
        for it in &snapshot {
            if it.status == Status::Done && self.recorded.insert(it.id) {
                let title = if it.title.is_empty() { it.url.clone() } else { it.title.clone() };
                self.history.insert(0, HistEntry { title, url: it.url.clone(), tag: it.tag.clone() });
            }
        }
        self.history.truncate(100);

        // автооновлення
        let stage = self.up.lock().unwrap().clone();
        let busy = snapshot.iter().any(|i| matches!(i.status, Status::Running | Status::Queued));
        let mut start_up: Option<String> = None;
        if let UpStage::Available { url, .. } = &stage {
            if self.s.auto_update && !busy {
                start_up = Some(url.clone());
            }
        }
        if matches!(stage, UpStage::Closing) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // ---- ліва панель: навігація ----
        let n_active = snapshot.iter().filter(|i| matches!(i.status, Status::Queued | Status::Running)).count();
        let side_fill = mix(ctx.style().visuals.panel_fill, Color32::BLACK, 0.18);
        egui::SidePanel::left("nav")
            .exact_width(204.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(side_fill).inner_margin(egui::Margin::symmetric(14.0, 16.0)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("🌐 Grab").size(25.0).b().color(accent));
                    ui.label(RichText::new("DOWNLOAD MANAGER").size(9.0).strong().weak());
                });
                ui.add_space(22.0);
                ui.label(RichText::new("БІБЛІОТЕКА").size(10.0).strong().weak());
                ui.add_space(7.0);
                let q_label = if n_active > 0 { format!("Черга ({n_active})") } else { "Черга".to_string() };
                for (t, icon, label) in [
                    (Tab::Queue, "▤", q_label),
                    (Tab::History, "◷", "Завершені".to_string()),
                    (Tab::Settings, "⚙", "Налаштування".to_string()),
                ] {
                    if nav_item(ui, icon, &label, self.tab == t, accent).clicked() { self.tab = t; }
                    ui.add_space(5.0);
                }
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(12.0);
                ui.label(RichText::new("ЗБЕРЕЖЕННЯ").size(10.0).strong().weak());
                ui.add_space(7.0);
                ui.label(RichText::new("📁  Папка завантажень").size(12.0));
                ui.label(RichText::new(self.s.out_dir.as_str()).size(10.0).weak().italics());
                ui.add_space(10.0);
                ui.label(RichText::new("ФОРМАТ").size(10.0).strong().weak());
                ui.label(RichText::new(if self.s.mode == Mode::Video { format!("Відео · {}p", self.s.quality) } else { format!("Аудіо · {}", self.s.audio_fmt) }).size(12.0).color(accent));
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("v{VERSION}")).monospace().size(11.0).weak());
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        let ring = ui.visuals().text_color();
                        for c in ACCENTS.iter() {
                            let col = Color32::from_rgb(c[0], c[1], c[2]);
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                            ui.painter().circle_filled(rect.center(), 8.0, col);
                            if self.s.accent == *c {
                                ui.painter().circle_stroke(rect.center(), 10.0, Stroke::new(2.0_f32, ring));
                            }
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                self.s.accent = *c;
                            }
                        }
                    });
                });
            });

        // ---- нижня панель: статистика ----
        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
            let active = snapshot.iter().filter(|i| i.status == Status::Running).count();
            let queued = snapshot.iter().filter(|i| i.status == Status::Queued).count();
            let paused = snapshot.iter().filter(|i| i.status == Status::Paused).count();
            let done = snapshot.iter().filter(|i| i.status == Status::Done).count();
            let failed = snapshot.iter().filter(|i| matches!(i.status, Status::Failed(_))).count();
            let total: f64 = snapshot.iter().map(|i| i.speed_bps).sum();
            let weak = ui.visuals().weak_text_color();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("▶ Активні: {active}")).color(if active > 0 { accent } else { weak }));
                ui.label(format!("⏳ У черзі: {queued}"));
                if paused > 0 { ui.label(format!("⏸ Пауза: {paused}")); }
                ui.label(RichText::new(format!("✔ Готово: {done}")).color(if done > 0 { green } else { weak }));
                ui.label(RichText::new(format!("⚠ Помилок: {failed}")).color(if failed > 0 { red } else { weak }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("v{VERSION}")).monospace().size(12.0).color(weak));
                    ui.label(RichText::new(format!("Загальна швидкість: {}", fmt_speed(total))).monospace().size(13.0).color(accent));
                });
            });
        });

        let mut cancel_ids = Vec::new();
        let mut pause_ids: Vec<u64> = Vec::new();
        let mut menu_req: Option<(u64, egui::Pos2)> = None;
        let mut resume_ids: Vec<u64> = Vec::new();
        let mut remove_ids = Vec::new();
        let mut redo_urls: Vec<String> = Vec::new();
        let mut add = false;
        let mut clear = false;
        let mut start_inst = false;
        let paste = false;
        let mut paste_add = false;
        let mut recheck = false;
        add |= k_add;
        paste_add |= k_paste;
        let out_dir = self.s.out_dir.clone();

        // ---- центральна панель ----
        let central_frame = egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(18.0));
        egui::CentralPanel::default().frame(central_frame).show(ctx, |ui| {
            let base = ui.visuals().panel_fill;
            let text_col = ui.visuals().text_color();
            let card_fill = mix(base, text_col, 0.07);
            let card_border = mix(base, text_col, 0.22);

            // банер оновлення програми
            match &stage {
                UpStage::Available { ver, url } => {
                    egui::Frame::none()
                        .fill(mix(base, accent, 0.18))
                        .stroke(Stroke::new(1.0_f32, accent))
                        .rounding(10.0)
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("🚀 Доступна нова версія {ver}")).b());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if color_button(ui, "⬇ Оновити й перезапустити", accent, 32.0).clicked() {
                                        start_up = Some(url.clone());
                                    }
                                });
                            });
                        });
                    ui.add_space(6.0);
                }
                UpStage::Downloading(p) => {
                    egui::Frame::none()
                        .fill(mix(base, accent, 0.18))
                        .stroke(Stroke::new(1.0_f32, accent))
                        .rounding(10.0)
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("🚀 Оновлення програми…").b());
                            ui.add(egui::ProgressBar::new(*p).show_percentage().fill(accent).desired_height(12.0).animate(true));
                        });
                    ui.add_space(6.0);
                }
                UpStage::Failed(e) => {
                    ui.colored_label(red, format!("⚠ Оновлення не вдалося: {e}"));
                }
                _ => {}
            }

            match self.tab {
                Tab::Queue => {
                    let amber = if self.s.theme == Theme::Light { Color32::from_rgb(176, 92, 0) } else { Color32::from_rgb(255, 184, 56) };
                    if !self.ytdlp_ok || !self.ffmpeg_ok {
                        let (busy_i, msg, prog, err) = {
                            let i = self.install.lock().unwrap();
                            (i.busy, i.msg.clone(), i.progress, i.err.clone())
                        };
                        egui::Frame::group(ui.style()).rounding(10.0).inner_margin(12.0).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("⚙ Потрібні компоненти: yt-dlp та ffmpeg").b());
                            if busy_i {
                                ui.add(egui::ProgressBar::new(prog).show_percentage().fill(accent).desired_height(14.0).animate(true));
                                ui.label(msg);
                            } else {
                                if let Some(e) = err { ui.colored_label(red, e); }
                                if color_button(ui, "⬇ Встановити автоматично", accent, 34.0).clicked() { start_inst = true; }
                            }
                        });
                    }

                    // Заголовок у стилі IDM
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Усі завантаження").size(25.0).strong());
                            ui.label(RichText::new("Керування файлами та активними завантаженнями").size(12.0).weak());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if soft_button(ui, "⋯  Дії", 34.0).clicked() { self.tab = Tab::Settings; }
                        });
                    });
                    ui.add_space(12.0);
                    egui::Frame::none().fill(mix(base, text_col, 0.045)).stroke(Stroke::new(1.0, card_border)).rounding(12.0).inner_margin(egui::Margin::symmetric(10.0, 8.0)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if color_button(ui, "＋  Додати URL", accent, 36.0).clicked() { paste_add = true; }
                            ui.separator();
                            if soft_button(ui, "▶  Продовжити", 34.0).clicked() {
                                for it in &snapshot { if it.status == Status::Paused { resume_ids.push(it.id); } }
                            }
                            if soft_button(ui, "Ⅱ  Пауза", 34.0).clicked() {
                                for it in &snapshot { if it.status == Status::Running { pause_ids.push(it.id); } }
                            }
                            if soft_button(ui, "■  Стоп", 34.0).clicked() {
                                for it in &snapshot { if matches!(it.status, Status::Running | Status::Queued) { cancel_ids.push(it.id); } }
                            }
                            if soft_button(ui, "🗑", 34.0).clicked() { clear = true; }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(format!("{} файлів", snapshot.len())).weak().size(11.0));
                            });
                        });
                    });
                    ui.add_space(10.0);

                    ui.add(
                        egui::TextEdit::multiline(&mut self.input)
                            .hint_text("🔗 Встав посилання на відео або плейлист (кожне з нового рядка) — або перетягни сюди .txt")
                            .desired_rows(1)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        if color_button(ui, "📋 Вставити й качати", accent, 38.0).clicked() { paste_add = true; }
                        if soft_button(ui, "➕ Додати в чергу", 38.0).clicked() { add = true; }
                        if let Some((msg, until)) = &self.notice {
                            if ui.input(|i| i.time) < *until {
                                ui.label(RichText::new(msg.as_str()).color(accent));
                            }
                        }
                    });

                    ui.horizontal_wrapped(|ui| {
                        let video = self.s.mode == Mode::Video;
                        let b1 = egui::Button::new("🎬 Відео").selected(video).rounding(8.0).min_size(egui::vec2(100.0, 32.0));
                        if ui.add(b1).clicked() { self.s.mode = Mode::Video; }
                        let b2 = egui::Button::new("🎵 Музика").selected(!video).rounding(8.0).min_size(egui::vec2(100.0, 32.0));
                        if ui.add(b2).clicked() { self.s.mode = Mode::Audio; }
                        ui.add_space(8.0);
                        if video {
                            for (q, l) in [(480u32, "480p"), (720, "720p"), (1080, "1080p"), (1440, "1440p"), (2160, "4K")] {
                                if chip(ui, l, self.s.quality == q).clicked() { self.s.quality = q; }
                            }
                        } else {
                            for f in ["mp3", "m4a", "opus", "flac", "wav"] {
                                if chip(ui, f, self.s.audio_fmt == f).clicked() { self.s.audio_fmt = f.to_string(); }
                            }
                        }
                        ui.add_space(8.0);
                        if chip(ui, "📃 Плейлист", self.s.playlist).clicked() { self.s.playlist = !self.s.playlist; }
                        if video && chip(ui, "💬 Субтитри", self.s.subs).clicked() { self.s.subs = !self.s.subs; }
                        if chip(ui, "🖼 Обкладинка", self.s.thumb).clicked() { self.s.thumb = !self.s.thumb; }
                    });
                    ui.add_space(6.0);

                    // фільтри
                    let n_all = snapshot.len();
                    let n_act = snapshot.iter().filter(|i| matches!(i.status, Status::Queued | Status::Running | Status::Paused)).count();
                    let n_done = snapshot.iter().filter(|i| i.status == Status::Done).count();
                    let n_fail = snapshot.iter().filter(|i| matches!(i.status, Status::Failed(_) | Status::Canceled)).count();
                    ui.horizontal(|ui| {
                        for (f, label) in [
                            (Filter::All, format!("Усі ({n_all})")),
                            (Filter::Active, format!("Активні ({n_act})")),
                            (Filter::Done, format!("Завершені ({n_done})")),
                            (Filter::Failed, format!("Невдалі ({n_fail})")),
                        ] {
                            let b = egui::Button::new(RichText::new(label).semi()).selected(self.filter == f).rounding(16.0).min_size(egui::vec2(0.0, 30.0));
                            if ui.add(b).clicked() { self.filter = f; }
                        }
                    });
                    ui.add_space(4.0);
                    ui.separator();

                    let flt = self.filter;
                    let visible: Vec<&Item> = snapshot.iter().filter(|i| match flt {
                        Filter::All => true,
                        Filter::Active => matches!(i.status, Status::Queued | Status::Running | Status::Paused),
                        Filter::Done => i.status == Status::Done,
                        Filter::Failed => matches!(i.status, Status::Failed(_) | Status::Canceled),
                    }).collect();

                    if visible.is_empty() {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("⬇").size(48.0).color(accent));
                            ui.label(RichText::new(if snapshot.is_empty() { "Черга порожня" } else { "Тут нічого немає" }).size(18.0).b());
                            if snapshot.is_empty() {
                                ui.label(RichText::new("Встав посилання вище й натисни «Вставити й качати»").weak());
                            }
                        });
                    }

                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for it in visible {
                            let stripe = match &it.status {
                                Status::Done => green,
                                Status::Paused => amber,
                                Status::Failed(_) | Status::Canceled => red,
                                _ => accent,
                            };
                            let mut draw_card = |ui: &mut egui::Ui| -> f32 {
                            let card = egui::Frame::none()
                                .fill(card_fill)
                                .stroke(Stroke::new(1.0_f32, card_border))
                                .rounding(14.0)
                                .inner_margin(12.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        // плитка-іконка
                                        let video = it.tag.starts_with("MP4");
                                        let (tint, icon) = match &it.status {
                                            Status::Done => (green, "✔"),
                                            Status::Paused => (amber, "⏸"),
                                            Status::Failed(_) | Status::Canceled => (red, "⚠"),
                                            _ => if video { (accent, "▶") } else { (Color32::from_rgb(62, 194, 143), "🎵") },
                                        };
                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(104.0, 70.0), egui::Sense::hover());
                                        ui.painter().rect_filled(rect, 12.0, mix(card_fill, tint, 0.22));
                                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, FontId::proportional(26.0), tint);

                                        let right_w = 200.0;
                                        let col_w = (ui.available_width() - right_w - 16.0).max(200.0);
                                        ui.vertical(|ui| {
                                            ui.set_width(col_w);
                                            let name = if it.title.is_empty() { &it.url } else { &it.title };
                                            ui.add(egui::Label::new(RichText::new(name).semi().size(16.0)).truncate());
                                            ui.horizontal(|ui| {
                                                let dom = if it.domain.is_empty() { domain_of(&it.url) } else { it.domain.clone() };
                                                ui.label(RichText::new(format!("🌐 {dom}")).size(12.5).weak());
                                                if ui.small_button("📋").on_hover_text("Скопіювати посилання").clicked() {
                                                    if let Ok(mut cb) = arboard::Clipboard::new() { let _ = cb.set_text(it.url.clone()); }
                                                }
                                            });
                                            ui.horizontal_wrapped(|ui| {
                                                ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                                                info_chip(ui, "Розмір", if it.size.is_empty() { "—" } else { &it.size }, None);
                                                if !it.dur.is_empty() { info_chip(ui, "Тривалість", &it.dur, None); }
                                                if !it.res.is_empty() { info_chip(ui, "Роздільність", &it.res, None); }
                                                info_chip(ui, "Формат", it.tag.split_whitespace().next().unwrap_or(""), Some(accent));
                                            });
                                            let bar_col = match &it.status {
                                                Status::Done => green,
                                                Status::Paused => amber,
                                                Status::Failed(_) | Status::Canceled => red,
                                                _ => accent,
                                            };
                                            ui.horizontal(|ui| {
                                                let pw = (ui.available_width() - 60.0).max(80.0);
                                                ui.add(
                                                    egui::ProgressBar::new(it.percent / 100.0)
                                                        .fill(bar_col)
                                                        .desired_width(pw)
                                                        .desired_height(10.0)
                                                        .animate(it.status == Status::Running),
                                                );
                                                ui.label(RichText::new(format!("{:.0}%", it.percent)).family(f_mono_bold()).size(15.0));
                                            });
                                            let st: RichText = match &it.status {
                                                Status::Queued => RichText::new("У черзі").weak(),
                                                Status::Running if it.speed.is_empty() => RichText::new("Підготовка…").color(accent),
                                                Status::Running => RichText::new(format!("{}  •  {}  •  ETA {}", it.size, it.speed, it.eta)).monospace().size(12.5).color(accent),
                                                Status::Paused => RichText::new(format!("Призупинено на {:.0}%", it.percent)).color(amber),
                                                Status::Done => RichText::new("Завершено").color(green),
                                                Status::Canceled => RichText::new("Скасовано").weak(),
                                                Status::Failed(e) => RichText::new(format!("⚠ {e}")).color(red),
                                            };
                                            ui.add(egui::Label::new(st).truncate());
                                        });

                                        // права колонка: статус і кнопки
                                        ui.vertical(|ui| {
                                            ui.set_width(right_w);
                                            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                                                let weak_c = ui.visuals().weak_text_color();
                                                match &it.status {
                                                    Status::Queued => pill(ui, "⏳ У черзі", weak_c),
                                                    Status::Running => pill(ui, "⬇ Завантажується", accent),
                                                    Status::Paused => pill(ui, "⏸ Призупинено", amber),
                                                    Status::Done => pill(ui, "✔ Готово", green),
                                                    Status::Failed(_) => pill(ui, "⚠ Помилка", red),
                                                    Status::Canceled => pill(ui, "✖ Скасовано", weak_c),
                                                }
                                                ui.add_space(6.0);
                                                let has_file = it.status == Status::Done && it.file.as_ref().map_or(false, |f| Path::new(f).exists());
                                                let n_btn = match &it.status {
                                                    Status::Queued | Status::Running | Status::Paused => 3,
                                                    Status::Done => if has_file { 3 } else { 2 },
                                                    _ => 2,
                                                };
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 6.0;
                                                    match &it.status {
                                                        Status::Queued | Status::Running => {
                                                            if icon_btn(ui, "⏸", "Пауза", None).clicked() { pause_ids.push(it.id); }
                                                            if icon_btn(ui, "✖", "Скасувати", Some(red)).clicked() { cancel_ids.push(it.id); }
                                                        }
                                                        Status::Paused => {
                                                            if icon_btn(ui, "▶", "Продовжити", Some(green)).clicked() { resume_ids.push(it.id); }
                                                            if icon_btn(ui, "✖", "Скасувати", Some(red)).clicked() { cancel_ids.push(it.id); }
                                                        }
                                                        Status::Done => {
                                                            if has_file && icon_btn(ui, "▶", "Відкрити файл", None).clicked() {
                                                                if let Some(f) = &it.file { open_path(f); }
                                                            }
                                                            if icon_btn(ui, "📂", "Відкрити папку", None).clicked() {
                                                                let d = if it.dir.is_empty() { out_dir.clone() } else { it.dir.clone() };
                                                                open_folder(&d);
                                                            }
                                                        }
                                                        Status::Failed(_) | Status::Canceled => {
                                                            if icon_btn(ui, "🔄", "Повторити", Some(accent)).clicked() { resume_ids.push(it.id); }
                                                        }
                                                    }
                                                    if icon_btn(ui, "🗑", "Видалити", None).clicked() { remove_ids.push(it.id); }
                                                });
                                                let _ = n_btn;
                                            });
                                        });
                                    });
                                });
                            let rc = card.response.rect;
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(rc.min + egui::vec2(0.0, 12.0), egui::vec2(4.0, (rc.height() - 24.0).max(8.0))),
                                2.0,
                                stripe,
                            );
                            // правий клік — контекстне меню
                            if ui.rect_contains_pointer(rc) && ui.input(|i| i.pointer.secondary_clicked()) {
                                if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) { menu_req = Some((it.id, pos)); }
                            }
                            rc.height()
                            };
                            let id_h = egui::Id::new(("card_h", it.id));
                            if let Some(t0) = it.removing {
                                // згортання + затухання під час видалення
                                let t = (((now - t0) / 0.25) as f32).clamp(0.0, 1.0);
                                let e = t * t;
                                let h_prev: f32 = ui.ctx().data(|d| d.get_temp(id_h)).unwrap_or(80.0);
                                let w = ui.available_width();
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(w, (h_prev * (1.0 - e)).max(0.0)), egui::Sense::hover());
                                let mut child = ui.child_ui(rect, egui::Layout::top_down(egui::Align::Min), None);
                                child.set_clip_rect(rect.intersect(ui.clip_rect()));
                                child.set_opacity(1.0 - e);
                                draw_card(&mut child);
                                ui.add_space(4.0 * (1.0 - e));
                                ui.ctx().request_repaint();
                            } else {
                                // поява: плавний виїзд і проявлення
                                let first = ui.ctx().data_mut(|d| *d.get_temp_mut_or_insert_with(egui::Id::new(("card_seen", it.id)), || now));
                                let t = (((now - first) / 0.3) as f32).clamp(0.0, 1.0);
                                let e = 1.0 - (1.0 - t).powi(3);
                                if t < 1.0 {
                                    ui.ctx().request_repaint();
                                    ui.add_space((1.0 - e) * 14.0);
                                }
                                let h = ui.scope(|ui| { ui.set_opacity(e); draw_card(ui) }).inner;
                                ui.ctx().data_mut(|d| d.insert_temp(id_h, h));
                                ui.add_space(4.0);
                            }
                        }
                    });
                }

                Tab::History => {
                    ui.label(RichText::new("Історія завантажень").size(26.0).b());
                    if self.history.is_empty() {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("🕘").size(48.0).color(accent));
                            ui.label(RichText::new("Тут з'являться завершені завантаження").weak());
                        });
                    } else {
                        if soft_button(ui, "🗑 Очистити історію", 32.0).clicked() { self.history.clear(); }
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            for h in &self.history {
                                egui::Frame::none()
                                    .fill(card_fill)
                                    .stroke(Stroke::new(1.0_f32, card_border))
                                    .rounding(12.0)
                                    .inner_margin(10.0)
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            let col_w = (ui.available_width() - 60.0).max(150.0);
                                            ui.vertical(|ui| {
                                                ui.set_width(col_w);
                                                ui.label(RichText::new(&h.title).semi().size(15.5));
                                                ui.label(RichText::new(format!("{}  •  {}", h.tag, h.url)).small().weak());
                                            });
                                            if icon_btn(ui, "🔁", "Завантажити ще раз", None).clicked() {
                                                redo_urls.push(h.url.clone());
                                            }
                                        });
                                    });
                                ui.add_space(2.0);
                            }
                        });
                    }
                }

                Tab::Settings => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.label(RichText::new("Налаштування").size(26.0).b());

                        section(ui, accent, "Вигляд");
                        egui::ComboBox::from_label("Тема")
                            .selected_text(match self.s.theme {
                                Theme::Dark => "Темна", Theme::Light => "Світла",
                                Theme::Midnight => "Опівніч", Theme::Forest => "Ліс",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.s.theme, Theme::Dark, "Темна");
                                ui.selectable_value(&mut self.s.theme, Theme::Light, "Світла");
                                ui.selectable_value(&mut self.s.theme, Theme::Midnight, "Опівніч");
                                ui.selectable_value(&mut self.s.theme, Theme::Forest, "Ліс");
                            });
                        ui.horizontal(|ui| {
                            ui.color_edit_button_srgb(&mut self.s.accent);
                            ui.label("Свій акцентний колір");
                        });
                        ui.separator();

                        section(ui, accent, "Завантаження");
                        ui.add(egui::Slider::new(&mut self.s.max_parallel, 1..=6).text("Одночасно"));
                        ui.horizontal(|ui| {
                            ui.label("Ліміт швидкості");
                            ui.add(egui::TextEdit::singleline(&mut self.s.limit).hint_text("напр. 2M").desired_width(90.0));
                        });
                        ui.separator();

                        section(ui, accent, "Папка збереження");
                        ui.add(egui::TextEdit::singleline(&mut self.s.out_dir).desired_width(f32::INFINITY));
                        ui.horizontal(|ui| {
                            if soft_button(ui, "📁 Обрати...", 32.0).clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                    self.s.out_dir = p.display().to_string();
                                }
                            }
                            if soft_button(ui, "📂 Відкрити", 32.0).clicked() {
                                open_folder(&self.s.out_dir);
                            }
                        });
                        ui.separator();

                        section(ui, accent, "Файли й папки");
                        ui.checkbox(&mut self.s.sort_type, "Розкладати по папках «Відео» та «Музика»");
                        ui.checkbox(&mut self.s.by_channel, "Підпапка за назвою каналу");
                        egui::ComboBox::from_label("Шаблон назви файлу")
                            .selected_text(NAME_TPLS[(self.s.name_tpl as usize).min(NAME_TPLS.len() - 1)].0)
                            .show_ui(ui, |ui| {
                                for (i, (label, _)) in NAME_TPLS.iter().enumerate() {
                                    ui.selectable_value(&mut self.s.name_tpl, i as u8, *label);
                                }
                            });
                        ui.label(RichText::new("Недозволені символи прибираються автоматично; [id] робить назву унікальною.").small().weak());
                        ui.separator();

                        section(ui, accent, "Сповіщення й буфер обміну");
                        ui.checkbox(&mut self.s.notify, "Сповіщення, коли черга завершилась");
                        ui.checkbox(&mut self.s.watch_clip, "Автоматично підхоплювати посилання з буфера обміну");
                        ui.separator();

                        section(ui, accent, "Гарячі клавіші (на macOS замість Ctrl — ⌘)");
                        for (k, d) in [
                            ("Ctrl+Enter", "Додати посилання в чергу"),
                            ("Ctrl+Shift+V", "Вставити з буфера й качати"),
                            ("Ctrl+1 / 2 / 3", "Черга / Історія / Налаштування"),
                        ] {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(k).monospace().color(accent));
                                ui.label(d);
                            });
                        }
                        ui.separator();

                        section(ui, accent, "Оновлення");
                        ui.checkbox(&mut self.s.auto_update, "Оновлювати програму автоматично");
                        ui.horizontal(|ui| {
                            ui.label(format!("Поточна версія: v{VERSION}"));
                            if soft_button(ui, "🔄 Перевірити оновлення", 32.0).clicked() { recheck = true; }
                        });
                        match &stage {
                            UpStage::Checking => { ui.label(RichText::new("Перевіряю…").weak()); }
                            UpStage::UpToDate => { ui.colored_label(green, "✔ У тебе остання версія"); }
                            UpStage::CheckFailed(e) => { ui.label(RichText::new(format!("Не вдалося перевірити: {e}")).weak()); }
                            UpStage::Available { ver, .. } => { ui.colored_label(accent, format!("Є нова версія: {ver}")); }
                            _ => {}
                        }
                        if soft_button(ui, "🔄 Оновити yt-dlp", 32.0).clicked() {
                            std::thread::spawn(|| { let _ = tool_cmd(ytdlp_path()).arg("-U").output(); });
                        }
                    });
                }
            }
        });

        // контекстне меню картки
        if let Some((mid, mpos)) = self.menu {
            let mut close = false;
            if let Some(it) = snapshot.iter().find(|i| i.id == mid && i.removing.is_none()) {
                let area = egui::Area::new(egui::Id::new("card_menu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(mpos)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(220.0);
                            ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                                if ui.selectable_label(false, "🔗  Скопіювати посилання").clicked() {
                                    if let Ok(mut cb) = arboard::Clipboard::new() { let _ = cb.set_text(it.url.clone()); }
                                    close = true;
                                }
                                if ui.selectable_label(false, "🌐  Відкрити в браузері").clicked() {
                                    open_url(&it.url);
                                    close = true;
                                }
                                match &it.status {
                                    Status::Queued | Status::Running => {
                                        if ui.selectable_label(false, "⏸  Пауза").clicked() { pause_ids.push(it.id); close = true; }
                                    }
                                    Status::Paused => {
                                        if ui.selectable_label(false, "▶  Продовжити").clicked() { resume_ids.push(it.id); close = true; }
                                    }
                                    Status::Failed(_) | Status::Canceled => {
                                        if ui.selectable_label(false, "🔄  Повторити").clicked() { resume_ids.push(it.id); close = true; }
                                    }
                                    Status::Done => {
                                        if let Some(f) = it.file.as_ref().filter(|f| Path::new(f.as_str()).exists()) {
                                            if ui.selectable_label(false, "▶  Відкрити файл").clicked() { open_path(f); close = true; }
                                        }
                                        if ui.selectable_label(false, "📂  Відкрити папку").clicked() {
                                            let d = if it.dir.is_empty() { out_dir.clone() } else { it.dir.clone() };
                                            open_folder(&d);
                                            close = true;
                                        }
                                    }
                                }
                                ui.separator();
                                if ui.selectable_label(false, RichText::new("🗑  Видалити").color(red)).clicked() {
                                    remove_ids.push(it.id);
                                    close = true;
                                }
                            });
                        });
                    });
                let r = area.response.rect;
                let outside = ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().map_or(true, |p| !r.contains(p)));
                if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
            } else {
                close = true;
            }
            if close { self.menu = None; }
        }
        if let Some(req) = menu_req { self.menu = Some(req); }

        if paste || paste_add {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(t) = cb.get_text() {
                    let t = t.trim().to_string();
                    if !t.is_empty() && !self.input.lines().any(|l| l.trim() == t) {
                        if !self.input.is_empty() && !self.input.ends_with('\n') { self.input.push('\n'); }
                        self.input.push_str(&t);
                    }
                }
            }
        }
        if start_inst { start_install(self.install.clone(), ctx.clone()); }
        if recheck { check_update(self.up.clone(), ctx.clone()); }
        if let Some(url) = start_up {
            *self.up.lock().unwrap() = UpStage::Downloading(0.0);
            install_update(self.up.clone(), ctx.clone(), url);
        }
        if !redo_urls.is_empty() {
            for u in redo_urls {
                if !self.input.is_empty() && !self.input.ends_with('\n') { self.input.push('\n'); }
                self.input.push_str(&u);
            }
            add = true;
            self.tab = Tab::Queue;
        }
        if add || paste_add {
            let (_, sk) = self.add_urls();
            if sk > 0 { self.notice = Some((format!("Дублікатів пропущено: {sk}"), now + 4.0)); }
        }
        if !dropped_urls.is_empty() {
            let (a, sk) = self.add_list(dropped_urls);
            self.notice = Some((format!("Додано з файлу: {a}, дублікатів: {sk}"), now + 4.0));
            self.tab = Tab::Queue;
        }
        {
            let mut items = self.items.lock().unwrap();
            for id in pause_ids {
                if let Some(it) = items.iter_mut().find(|i| i.id == id) {
                    it.pause = true;
                    match &it.child {
                        Some(c) => { let _ = c.lock().unwrap().kill(); }
                        None => if it.status == Status::Queued { it.status = Status::Paused; },
                    }
                }
            }
            for id in resume_ids {
                if let Some(it) = items.iter_mut().find(|i| i.id == id) {
                    if matches!(it.status, Status::Paused | Status::Failed(_) | Status::Canceled) {
                        it.pause = false;
                        it.cancel = false;
                        it.status = Status::Queued;
                    }
                }
            }
            for id in cancel_ids {
                if let Some(it) = items.iter_mut().find(|i| i.id == id) {
                    it.cancel = true;
                    match &it.child {
                        Some(c) => { let _ = c.lock().unwrap().kill(); }
                        None => it.status = Status::Canceled,
                    }
                }
            }
            for it in items.iter_mut() {
                let rm = remove_ids.contains(&it.id)
                    || (clear && matches!(it.status, Status::Done | Status::Failed(_) | Status::Canceled));
                if rm && it.removing.is_none() {
                    it.cancel = true;
                    if let Some(c) = &it.child { let _ = c.lock().unwrap().kill(); }
                    it.removing = Some(now);
                }
            }
            items.retain(|i| i.removing.map_or(true, |t| now - t < 0.25));
        }
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1020.0, 660.0])
            .with_min_inner_size([780.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("Grab", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
