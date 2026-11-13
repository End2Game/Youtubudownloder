#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Rounding, Stroke, TextStyle};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

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
        }
    }
}

#[derive(Clone, PartialEq)]
enum Status { Queued, Running, Done, Failed(String), Canceled }

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
    child: Option<Arc<Mutex<Child>>>,
    cancel: bool,
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
    let mut cmd = tool_cmd(ytdlp_path());
    cmd.args([
        "--newline",
        "--no-colors",
        "--progress-template",
        "download:PROG|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s|%(progress._total_bytes_str)s",
        "-o",
    ])
    .arg(format!("{}/%(title)s.%(ext)s", s.out_dir))
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
    with_item(&items, id, |it| it.child = Some(child.clone()));

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
                with_item(&items, id, |it| {
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
    with_item(&items, id, |it| {
        it.child = None;
        it.speed_bps = 0.0;
        it.speed.clear();
        it.eta.clear();
        it.status = if it.cancel {
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

struct App {
    s: Settings,
    input: String,
    items: Shared,
    next_id: u64,
    ytdlp_ok: bool,
    ffmpeg_ok: bool,
    install: InstallState,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let s = cc.storage.and_then(|st| eframe::get_value(st, "settings")).unwrap_or_default();
        let ytdlp_ok = has_tool(ytdlp_path());
        let ffmpeg_ok = ffmpeg_dir().is_some() || has_tool("ffmpeg");
        let install: InstallState = Arc::new(Mutex::new(Install::default()));
        if !ytdlp_ok || !ffmpeg_ok {
            start_install(install.clone(), cc.egui_ctx.clone()); // автоустановка при першому запуску
        }
        Self {
            s,
            input: String::new(),
            items: Arc::new(Mutex::new(Vec::new())),
            next_id: 1,
            ytdlp_ok,
            ffmpeg_ok,
            install,
        }
    }

    fn accent(&self) -> Color32 {
        Color32::from_rgb(self.s.accent[0], self.s.accent[1], self.s.accent[2])
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
        let base = v.panel_fill;
        let text = v.text_color();
        let r = Rounding::same(8.0);

        v.selection.bg_fill = a;
        v.selection.stroke = Stroke::new(1.0, on_color(a));
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
        v.widgets.inactive.bg_fill = mix(base, text, 0.10);
        v.widgets.inactive.weak_bg_fill = mix(base, text, 0.08);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, mix(base, text, 0.12));
        v.widgets.hovered.bg_fill = mix(base, a, 0.30);
        v.widgets.hovered.weak_bg_fill = mix(base, a, 0.25);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, a);
        v.widgets.active.bg_fill = mix(base, a, 0.50);
        v.widgets.active.weak_bg_fill = mix(base, a, 0.45);
        v.widgets.active.bg_stroke = Stroke::new(1.0, a);
        ctx.set_visuals(v);

        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.slider_width = 120.0;
            s.text_styles.insert(TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Body, FontId::new(15.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Button, FontId::new(15.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Small, FontId::new(12.0, FontFamily::Proportional));
        });
    }

    fn add_urls(&mut self) {
        let mut items = self.items.lock().unwrap();
        for line in self.input.lines().map(str::trim).filter(|l| !l.is_empty()) {
            items.push(Item {
                id: self.next_id, url: line.to_string(), title: String::new(), status: Status::Queued,
                percent: 0.0, speed: String::new(), speed_bps: 0.0, eta: String::new(),
                size: String::new(), child: None, cancel: false,
            });
            self.next_id += 1;
        }
        self.input.clear();
    }

    fn start_queued(&self, ctx: &egui::Context) {
        if !self.ytdlp_ok || !self.ffmpeg_ok { return; }
        let mut to_start = Vec::new();
        {
            let mut items = self.items.lock().unwrap();
            let mut running = items.iter().filter(|i| i.status == Status::Running).count();
            for it in items.iter_mut() {
                if running >= self.s.max_parallel { break; }
                if it.status == Status::Queued {
                    it.status = Status::Running;
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
        ui.add(egui::Button::new(RichText::new(label).strong()).min_size(egui::vec2(0.0, height)))
    })
    .inner
}

/// Звичайна (приглушена) кнопка
fn soft_button(ui: &mut egui::Ui, label: &str, height: f32) -> egui::Response {
    ui.add(egui::Button::new(label).min_size(egui::vec2(0.0, height)))
}

fn section(ui: &mut egui::Ui, accent: Color32, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text.to_uppercase()).small().strong().color(accent));
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

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "settings", &self.s);
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
        let snapshot: Vec<Item> = self.items.lock().unwrap().clone();

        let side_frame = egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::same(14.0));
        egui::SidePanel::left("settings").default_width(280.0).min_width(260.0).frame(side_frame).show(ctx, |ui| {
            ui.label(RichText::new("⬇ Grab").size(28.0).strong().color(accent));
            ui.label(RichText::new("Налаштування").weak());
            ui.separator();

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
                ui.label("Акцентний колір");
            });
            ui.separator();

            section(ui, accent, "Завантаження");
            ui.horizontal(|ui| {
                let video = self.s.mode == Mode::Video;
                let b1 = egui::Button::new("🎬 Відео").selected(video)
                    .rounding(8.0).min_size(egui::vec2(110.0, 34.0));
                if ui.add(b1).clicked() { self.s.mode = Mode::Video; }
                let b2 = egui::Button::new("🎵 Музика").selected(!video)
                    .rounding(8.0).min_size(egui::vec2(110.0, 34.0));
                if ui.add(b2).clicked() { self.s.mode = Mode::Audio; }
            });
            if self.s.mode == Mode::Video {
                egui::ComboBox::from_label("Якість")
                    .selected_text(format!("до {}p", self.s.quality))
                    .show_ui(ui, |ui| {
                        for q in [360, 480, 720, 1080, 1440, 2160] {
                            ui.selectable_value(&mut self.s.quality, q, format!("до {q}p"));
                        }
                    });
            } else {
                egui::ComboBox::from_label("Формат")
                    .selected_text(self.s.audio_fmt.clone())
                    .show_ui(ui, |ui| {
                        for f in ["mp3", "m4a", "opus", "flac", "wav"] {
                            ui.selectable_value(&mut self.s.audio_fmt, f.to_string(), f);
                        }
                    });
            }
            ui.add(egui::Slider::new(&mut self.s.max_parallel, 1..=6).text("Одночасно"));
            ui.horizontal(|ui| {
                ui.label("Ліміт швидкості");
                ui.add(egui::TextEdit::singleline(&mut self.s.limit).hint_text("напр. 2M").desired_width(80.0));
            });
            ui.separator();

            section(ui, accent, "Опції");
            ui.checkbox(&mut self.s.playlist, "Весь плейлист");
            ui.checkbox(&mut self.s.subs, "Субтитри (uk/ru/en)");
            ui.checkbox(&mut self.s.thumb, "Обкладинка у файл");
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

            if soft_button(ui, "🔄 Оновити yt-dlp", 32.0).clicked() {
                std::thread::spawn(|| { let _ = tool_cmd(ytdlp_path()).arg("-U").output(); });
            }
        });

        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
            let active = snapshot.iter().filter(|i| i.status == Status::Running).count();
            let queued = snapshot.iter().filter(|i| i.status == Status::Queued).count();
            let done = snapshot.iter().filter(|i| i.status == Status::Done).count();
            let failed = snapshot.iter().filter(|i| matches!(i.status, Status::Failed(_))).count();
            let total: f64 = snapshot.iter().map(|i| i.speed_bps).sum();
            ui.horizontal(|ui| {
                let green = Color32::from_rgb(80, 200, 120);
                let red = Color32::from_rgb(230, 80, 80);
                ui.label(RichText::new(format!("▶ Активні: {active}")).color(if active > 0 { accent } else { ui.visuals().weak_text_color() }));
                ui.label(format!("⏳ У черзі: {queued}"));
                ui.label(RichText::new(format!("✔ Готово: {done}")).color(if done > 0 { green } else { ui.visuals().weak_text_color() }));
                ui.label(RichText::new(format!("⚠ Помилок: {failed}")).color(if failed > 0 { red } else { ui.visuals().weak_text_color() }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("Загальна швидкість: {}", fmt_speed(total))).color(accent));
                });
            });
        });

        let mut cancel_ids = Vec::new();
        let mut remove_ids = Vec::new();
        let mut add = false;
        let mut clear = false;
        let mut start_inst = false;
        let mut paste = false;

        let central_frame = egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(18.0));
        egui::CentralPanel::default().frame(central_frame).show(ctx, |ui| {
            ui.label(RichText::new("Завантажувач відео та музики").size(24.0).strong());
            if !self.ytdlp_ok || !self.ffmpeg_ok {
                let (busy, msg, prog, err) = {
                    let i = self.install.lock().unwrap();
                    (i.busy, i.msg.clone(), i.progress, i.err.clone())
                };
                egui::Frame::group(ui.style()).rounding(10.0).inner_margin(12.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("⚙ Потрібні компоненти: yt-dlp та ffmpeg").strong());
                    if busy {
                        ui.add(egui::ProgressBar::new(prog).show_percentage().fill(accent).desired_height(14.0).animate(true));
                        ui.label(msg);
                    } else {
                        if let Some(e) = err { ui.colored_label(Color32::from_rgb(230, 80, 80), e); }
                        if color_button(ui, "⬇ Встановити автоматично", accent, 34.0).clicked() { start_inst = true; }
                    }
                });
            }
            ui.label(RichText::new("Встав посилання — кожне з нового рядка").weak());
            ui.add(
                egui::TextEdit::multiline(&mut self.input)
                    .hint_text("https://www.youtube.com/watch?v=...")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal(|ui| {
                if color_button(ui, "➕ Додати в чергу", accent, 34.0).clicked() { add = true; }
                if soft_button(ui, "📋 Вставити з буфера", 34.0).clicked() { paste = true; }
                if soft_button(ui, "🗑 Очистити завершені", 34.0).clicked() { clear = true; }
            });
            ui.add_space(4.0);
            ui.separator();

            if snapshot.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("⬇").size(48.0).color(accent));
                    ui.label(RichText::new("Черга порожня").size(18.0).strong());
                    ui.label(RichText::new("Встав посилання вище й натисни «Додати в чергу»").weak());
                });
            }

            let card_fill = mix(ui.visuals().panel_fill, ui.visuals().text_color(), 0.05);
            let card_border = mix(ui.visuals().panel_fill, ui.visuals().text_color(), 0.10);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for it in &snapshot {
                    egui::Frame::none()
                        .fill(card_fill)
                        .stroke(Stroke::new(1.0, card_border))
                        .rounding(10.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let name = if it.title.is_empty() { &it.url } else { &it.title };
                            ui.label(RichText::new(name).strong());
                            ui.add(
                                egui::ProgressBar::new(it.percent / 100.0)
                                    .show_percentage()
                                    .fill(accent)
                                    .desired_height(14.0)
                                    .animate(it.status == Status::Running),
                            );
                            ui.horizontal(|ui| {
                                match &it.status {
                                    Status::Queued => { ui.label(RichText::new("⏳ У черзі").weak()); }
                                    Status::Running => {
                                        ui.label(RichText::new(format!("{}  •  {}  •  ETA {}", it.speed, it.size, it.eta)).color(accent));
                                    }
                                    Status::Done => { ui.colored_label(Color32::from_rgb(80, 200, 120), "✔ Готово"); }
                                    Status::Canceled => { ui.label(RichText::new("✖ Скасовано").weak()); }
                                    Status::Failed(e) => { ui.colored_label(Color32::from_rgb(230, 80, 80), format!("⚠ {e}")); }
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if soft_button(ui, "🗑 Видалити", 28.0).clicked() { remove_ids.push(it.id); }
                                    if matches!(it.status, Status::Queued | Status::Running)
                                        && color_button(ui, "✖ Скасувати", Color32::from_rgb(205, 75, 75), 28.0).clicked()
                                    { cancel_ids.push(it.id); }
                                });
                            });
                        });
                    ui.add_space(2.0);
                }
            });
        });

        if paste {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(t) = cb.get_text() {
                    if !self.input.is_empty() && !self.input.ends_with('\n') { self.input.push('\n'); }
                    self.input.push_str(t.trim());
                }
            }
        }
        if start_inst { start_install(self.install.clone(), ctx.clone()); }
        if add { self.add_urls(); }
        {
            let mut items = self.items.lock().unwrap();
            for id in cancel_ids {
                if let Some(it) = items.iter_mut().find(|i| i.id == id) {
                    it.cancel = true;
                    match &it.child {
                        Some(c) => { let _ = c.lock().unwrap().kill(); }
                        None => it.status = Status::Canceled,
                    }
                }
            }
            items.retain(|i| {
                if remove_ids.contains(&i.id) {
                    if let Some(c) = &i.child { let _ = c.lock().unwrap().kill(); }
                    return false;
                }
                !(clear && matches!(i.status, Status::Done | Status::Failed(_) | Status::Canceled))
            });
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
