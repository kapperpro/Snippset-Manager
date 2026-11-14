// Im Release-Build kein Konsolenfenster unter Windows anzeigen.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::collections::HashSet;
use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use arboard::Clipboard;
use chrono::{Datelike, Local, Weekday};
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, IsIconic, PostMessageW, SetForegroundWindow, ShowWindow, SW_HIDE,
    SW_RESTORE, SW_SHOW, WM_NCLBUTTONDOWN,
};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const FEEDBACK_DURATION: Duration = Duration::from_secs(1);
const MAX_LABEL_CHARS: usize = 80;
const AUTOSTART_NAME: &str = "SnippetManager";
/// Wird dem Autostart-Eintrag als Argument mitgegeben, damit die App weiß,
/// dass sie von Windows (und nicht manuell vom Nutzer) gestartet wurde.
const AUTOSTART_ARG: &str = "--autostart";
const TITLEBAR_HEIGHT: f32 = 24.0;
/// Radius der abgerundeten Fensterecken.
const WINDOW_ROUNDING: f32 = 10.0;
const HANDLE_WIDTH: f32 = 24.0;

// ---------------------------------------------------------------------------
// Datenmodell
// ---------------------------------------------------------------------------

/// Ein Snippet: sichtbare Beschreibung + versteckter Inhalt + optionale Gruppe.
///
/// Die Reihenfolge in `Config::snippets` ist zugleich die Anzeigereihenfolge.
/// Das Einlesen akzeptiert zusätzlich ältere Formate (reiner String bzw. Snippet
/// ohne Gruppe), damit eine vorhandene `config.json` weiter funktioniert.
#[derive(Clone, Serialize, Deserialize)]
#[serde(from = "SnippetRepr")]
struct Snippet {
    description: String,
    content: String,
    /// Leer = "Ohne Gruppe".
    group: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SnippetRepr {
    Current {
        description: String,
        content: String,
        #[serde(default)]
        group: String,
    },
    Legacy(String),
}

impl From<SnippetRepr> for Snippet {
    fn from(repr: SnippetRepr) -> Self {
        match repr {
            SnippetRepr::Current {
                description,
                content,
                group,
            } => Snippet {
                description,
                content,
                group,
            },
            // Alte Einträge: Beschreibung = gekürzter Text, Inhalt = kompletter Text.
            SnippetRepr::Legacy(text) => Snippet {
                description: shorten(&text, 40),
                content: text,
                group: String::new(),
            },
        }
    }
}

fn group_label(group: &str) -> &str {
    if group.is_empty() {
        "Ohne Gruppe"
    } else {
        group
    }
}

// ---------------------------------------------------------------------------
// Konfiguration / Persistenz (config.json in %APPDATA%\SnippetManager)
// ---------------------------------------------------------------------------

/// Was passiert, wenn das Fenster über das "X" geschlossen wird?
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum CloseBehavior {
    /// Beim Schließen nachfragen (Standard, also auch beim allerersten Mal).
    #[default]
    Ask,
    /// Fenster nur verstecken, Programm läuft im Tray weiter.
    MinimizeToTray,
    /// Programm komplett beenden.
    Quit,
}

/// Frei wählbare Grundfarben (je Dark-/Light-Mode). Alle weiteren Farben
/// (Hover, Rahmen, Eingabefelder …) werden daraus abgeleitet.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Palette {
    background: [u8; 3],
    surface: [u8; 3],
    text: [u8; 3],
    border: [u8; 3],
    accent: [u8; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Self::dark()
    }
}

impl Palette {
    fn dark() -> Self {
        Self {
            background: [0x16, 0x18, 0x1D],
            surface: [0x2A, 0x2F, 0x3A],
            text: [0xE8, 0xEA, 0xEE],
            border: [0x8A, 0x93, 0xA6],
            accent: [0x4C, 0x9A, 0xFF],
        }
    }

    fn light() -> Self {
        Self {
            background: [0xF4, 0xF5, 0xF7],
            surface: [0xE4, 0xE7, 0xED],
            text: [0x14, 0x17, 0x1E],
            border: [0x52, 0x5B, 0x6B],
            accent: [0x1A, 0x58, 0xCC],
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    snippets: Vec<Snippet>,
    dark_mode: bool,
    always_on_top: bool,
    autostart: bool,
    start_in_background: bool,
    close_behavior: CloseBehavior,
    palette_dark: Palette,
    palette_light: Palette,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            snippets: Vec::new(),
            dark_mode: true,
            always_on_top: false,
            autostart: false,
            start_in_background: false,
            close_behavior: CloseBehavior::Ask,
            palette_dark: Palette::dark(),
            palette_light: Palette::light(),
        }
    }
}

/// Ordner der .exe (nur noch für die Migration alter Konfigurationen und als Fallback).
fn exe_dir_config_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("config.json")))
}

/// `%APPDATA%\SnippetManager\config.json` – hier darf ein normales Programm
/// immer schreiben, auch wenn die .exe in `C:\Program Files` liegt.
fn config_path() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let dir = PathBuf::from(appdata).join("SnippetManager");
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir.join("config.json");
        }
    }
    exe_dir_config_path().unwrap_or_else(|| PathBuf::from("config.json"))
}

/// Übernimmt eine `config.json` aus dem Programmordner (alte Versionen),
/// falls im neuen Ordner noch keine existiert.
fn migrate_legacy_config(new_path: &PathBuf) {
    if new_path.exists() {
        return;
    }
    if let Some(old) = exe_dir_config_path() {
        if old != *new_path && old.exists() {
            let _ = std::fs::copy(old, new_path);
        }
    }
}

fn load_config(path: &PathBuf) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Autostart (Windows-Registry: HKCU\...\Run) – nur auf ausdrücklichen Wunsch
// des Nutzers (Checkbox in den Einstellungen, standardmäßig AUS).
// ---------------------------------------------------------------------------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn autostart_is_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|key| key.get_value::<String, _>(AUTOSTART_NAME))
        .is_ok()
}

/// Legt den Autostart-Eintrag an bzw. entfernt ihn. Der Pfad wird in
/// Anführungszeichen geschrieben, damit auch Pfade mit Leerzeichen
/// (z. B. `C:\Program Files\...`) zuverlässig funktionieren.
fn set_autostart_registry(on: bool) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(|e| e.to_string())?;

    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = format!("\"{}\" {}", exe.display(), AUTOSTART_ARG);
        key.set_value(AUTOSTART_NAME, &command)
            .map_err(|e| e.to_string())
    } else {
        match key.delete_value(AUTOSTART_NAME) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Icon (programmatisch erzeugt, keine externe Datei nötig)
// ---------------------------------------------------------------------------

fn icon_rgba() -> (Vec<u8>, u32, u32) {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let in_clip = (11..21).contains(&x) && (2..7).contains(&y);
            let in_line = (10..22).contains(&x) && matches!(y, 12 | 13 | 16 | 17 | 20 | 21);
            let in_paper = (8..24).contains(&x) && (8..26).contains(&y);
            let in_board = (5..27).contains(&x) && (4..29).contains(&y);

            let px: [u8; 4] = if in_clip {
                [40, 50, 70, 255]
            } else if in_line {
                [52, 120, 198, 255]
            } else if in_paper {
                [255, 255, 255, 255]
            } else if in_board {
                [52, 120, 198, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba.extend_from_slice(&px);
        }
    }
    (rgba, SIZE, SIZE)
}

// ---------------------------------------------------------------------------
// Fenstersteuerung direkt über die Windows-API
//
// Ein verstecktes eframe-Fenster verarbeitet keine Frames mehr. Befehle wie
// `ViewportCommand::Visible(true)` kämen dort deshalb erst an, wenn ein anderes
// Ereignis die Event-Loop weckt. Darum zeigen/verstecken wir das Fenster direkt
// per Win32 – das funktioniert sofort, unabhängig von egui.
//
// Verwendet werden ausschließlich die vier gewöhnlichen Fenster-Funktionen
// IsIconic / ShowWindow / SetForegroundWindow – keine globalen Hooks, keine
// Eingabe-Überwachung, keine Prozess-Injektion.
// ---------------------------------------------------------------------------

/// HWND des Hauptfensters (0 = unbekannt).
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

fn show_window(ctx: &egui::Context) {
    let h = MAIN_HWND.load(Ordering::SeqCst);
    if h != 0 {
        let hwnd = h as HWND;
        // SAFETY: Aufruf mit dem gültigen Fenster-Handle dieses Prozesses.
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
            SetForegroundWindow(hwnd);
        }
    }
    ctx.request_repaint();
}

fn hide_window() {
    let h = MAIN_HWND.load(Ordering::SeqCst);
    if h != 0 {
        // SAFETY: siehe oben.
        unsafe {
            ShowWindow(h as HWND, SW_HIDE);
        }
    }
}

/// Breite des Streifens am Fensterrand, in dem die Größe geändert werden kann.
const RESIZE_MARGIN: f32 = 6.0;
// Hit-Test-Codes von Windows (für den nativen Größenänderungs-Modus).
const HT_LEFT: usize = 10;
const HT_RIGHT: usize = 11;
const HT_TOP: usize = 12;
const HT_TOPLEFT: usize = 13;
const HT_TOPRIGHT: usize = 14;
const HT_BOTTOM: usize = 15;
const HT_BOTTOMLEFT: usize = 16;
const HT_BOTTOMRIGHT: usize = 17;

/// Schneidet die Fensterform mit abgerundeten Ecken zu (`SetWindowRgn`).
/// Funktioniert unabhängig von Transparenz und Grafiktreiber. Wird nur neu
/// gesetzt, wenn sich die Fenstergröße geändert hat.
fn update_window_region(ctx: &egui::Context, last: &mut (i32, i32)) {
    let h = MAIN_HWND.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let hwnd = h as HWND;
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: gültiges Fenster-Handle dieses Prozesses; die Region geht danach in den
    // Besitz des Systems über und darf nicht freigegeben werden.
    unsafe {
        if GetWindowRect(hwnd, &mut r) == 0 {
            return;
        }
        let (w, hgt) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || hgt <= 0 || (w, hgt) == *last {
            return;
        }
        *last = (w, hgt);
        let d = (WINDOW_ROUNDING * ctx.pixels_per_point() * 2.0).round() as i32;
        let rgn = CreateRoundRectRgn(0, 0, w + 1, hgt + 1, d, d);
        SetWindowRgn(hwnd, rgn, 1);
    }
}

/// Größe ändern am Fensterrand: Das randlose Fenster hat keinen Windows-Rahmen,
/// deshalb wird der Rand hier selbst erkannt und der native Größenmodus gestartet.
fn handle_resize_edges(ctx: &egui::Context) {
    let h = MAIN_HWND.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let rect = ctx.screen_rect();
    let left = pos.x < rect.left() + RESIZE_MARGIN;
    let right = pos.x > rect.right() - RESIZE_MARGIN;
    let top = pos.y < rect.top() + RESIZE_MARGIN;
    let bottom = pos.y > rect.bottom() - RESIZE_MARGIN;

    let (ht, icon) = match (left, right, top, bottom) {
        (true, _, true, _) => (HT_TOPLEFT, egui::CursorIcon::ResizeNwSe),
        (_, true, true, _) => (HT_TOPRIGHT, egui::CursorIcon::ResizeNeSw),
        (true, _, _, true) => (HT_BOTTOMLEFT, egui::CursorIcon::ResizeNeSw),
        (_, true, _, true) => (HT_BOTTOMRIGHT, egui::CursorIcon::ResizeNwSe),
        (true, _, _, _) => (HT_LEFT, egui::CursorIcon::ResizeHorizontal),
        (_, true, _, _) => (HT_RIGHT, egui::CursorIcon::ResizeHorizontal),
        (_, _, true, _) => (HT_TOP, egui::CursorIcon::ResizeVertical),
        (_, _, _, true) => (HT_BOTTOM, egui::CursorIcon::ResizeVertical),
        _ => return,
    };
    ctx.set_cursor_icon(icon);

    if ctx.input(|i| i.pointer.primary_pressed()) {
        // SAFETY: gültiges Fenster-Handle; startet den Standard-Größenmodus von Windows.
        unsafe {
            ReleaseCapture();
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let lparam = (((pt.y as isize) & 0xFFFF) << 16) | ((pt.x as isize) & 0xFFFF);
            PostMessageW(h as HWND, WM_NCLBUTTONDOWN, ht, lparam);
        }
    }
}

// ---------------------------------------------------------------------------
// Tray-Icon + Event-Handler
// ---------------------------------------------------------------------------

thread_local! {
    /// Das Tray-Icon lebt im Main-Thread (dort läuft auch die Windows-Message-Loop).
    static TRAY: RefCell<Option<TrayIcon>> = RefCell::new(None);
}

/// Entfernt das Icon sofort aus dem Infobereich (verhindert "Geister-Icons").
///
/// Das `TrayIcon` wird bewusst NICHT gedroppt, sondern nur versteckt und dann
/// "vergessen": Ein Drop während des Programmendes (Thread-Local-Destruktoren)
/// lässt `muda` abstürzen ("RefCell already borrowed"). Das Betriebssystem
/// räumt alle Ressourcen beim Prozessende ohnehin selbst auf.
fn remove_tray() {
    TRAY.with(|t| {
        if let Ok(mut guard) = t.try_borrow_mut() {
            if let Some(tray) = guard.take() {
                let _ = tray.set_visible(false);
                std::mem::forget(tray);
            }
        }
    });
}

/// Beendet das Programm sofort. Alles Wichtige ist bereits gespeichert
/// (config.json wird bei jeder Änderung geschrieben).
fn quit_now() -> ! {
    remove_tray();
    std::process::exit(0)
}

/// Muss auf dem Main-Thread aufgerufen werden. Die Handler werden per Callback
/// aus der Windows-Message-Loop aufgerufen und arbeiten direkt über Win32.
fn create_tray(ctx: &egui::Context) -> Result<(), Box<dyn Error>> {
    let menu = Menu::new();
    let open_item = MenuItem::new("Öffnen", true, None);
    let quit_item = MenuItem::new("Beenden", true, None);
    menu.append(&open_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();

    let ctx_menu = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            show_window(&ctx_menu);
        } else if event.id == quit_id {
            quit_now();
        }
    }));

    let ctx_tray = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            show_window(&ctx_tray);
        }
    }));

    let (rgba, w, h) = icon_rgba();
    let icon = Icon::from_rgba(rgba, w, h)?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false) // Linksklick = Fenster öffnen, Menü nur per Rechtsklick
        .with_tooltip("Snippet-Manager")
        .with_icon(icon)
        .build()?;

    TRAY.with(|t| *t.borrow_mut() = Some(tray));
    Ok(())
}

fn window_hwnd(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Optik: kontrastreiche Farben für Dark- und Light-Mode
// ---------------------------------------------------------------------------

/// Mischt zwei Farben (t = 0 → a, t = 1 → b).
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let l = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn build_visuals(dark: bool, pal: &Palette) -> egui::Visuals {
    use egui::{Color32, Stroke};

    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let c = |a: [u8; 3]| Color32::from_rgb(a[0], a[1], a[2]);
    let panel = c(pal.background);
    let text = c(pal.text);
    let btn = c(pal.surface);
    let border = c(pal.border);
    let accent = c(pal.accent);

    let window = mix(panel, text, 0.05);
    let input = if dark {
        mix(panel, Color32::BLACK, 0.45)
    } else {
        mix(panel, Color32::WHITE, 0.7)
    };
    let strong = if dark {
        mix(text, Color32::WHITE, 0.6)
    } else {
        mix(text, Color32::BLACK, 0.6)
    };
    let soft_border = mix(border, panel, 0.5);
    let btn_hover = mix(btn, text, 0.12);
    let btn_active = mix(btn, text, 0.22);

    v.panel_fill = panel;
    v.window_fill = window;
    v.extreme_bg_color = input;
    v.faint_bg_color = btn;
    v.window_stroke = Stroke::new(1.0_f32, border);
    v.hyperlink_color = accent;
    v.selection.bg_fill = accent;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, soft_border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);

    v.widgets.inactive.bg_fill = btn;
    v.widgets.inactive.weak_bg_fill = btn;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text);

    v.widgets.hovered.bg_fill = btn_hover;
    v.widgets.hovered.weak_bg_fill = btn_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.5_f32, accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, strong);

    v.widgets.active.bg_fill = btn_active;
    v.widgets.active.weak_bg_fill = btn_active;
    v.widgets.active.bg_stroke = Stroke::new(1.5_f32, accent);
    v.widgets.active.fg_stroke = Stroke::new(2.0_f32, strong);

    v.widgets.open.bg_fill = btn_active;
    v.widgets.open.weak_bg_fill = btn_active;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, border);
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, strong);

    v
}

/// Setzt beide Farbschemata (aus den eigenen Farben) und wählt das aktive.
fn apply_theme(ctx: &egui::Context, cfg: &Config) {
    ctx.set_visuals_of(egui::Theme::Dark, build_visuals(true, &cfg.palette_dark));
    ctx.set_visuals_of(egui::Theme::Light, build_visuals(false, &cfg.palette_light));
    ctx.set_theme(if cfg.dark_mode {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
}

// ---------------------------------------------------------------------------
// Kleine eigene Widgets (ohne Sonderzeichen-Schriften: alles wird gezeichnet)
// ---------------------------------------------------------------------------

/// Winziger Titelleisten-Button: `close = true` → "X", sonst Minimieren-Strich.
fn title_button(ui: &mut egui::Ui, close: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 18.0), egui::Sense::click());
    let hovered = resp.hovered();
    let visuals = ui.visuals();
    let hover_fill = if close {
        egui::Color32::from_rgb(200, 45, 45)
    } else {
        visuals.widgets.hovered.bg_fill
    };
    let color = if hovered && close {
        egui::Color32::WHITE
    } else if hovered {
        visuals.widgets.hovered.fg_stroke.color
    } else {
        visuals.widgets.inactive.fg_stroke.color
    };

    let painter = ui.painter();
    if hovered {
        painter.rect_filled(rect, 3.0, hover_fill);
    }
    let stroke = egui::Stroke::new(1.5_f32, color);
    let c = rect.center();
    if close {
        let d = 4.0;
        painter.line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], stroke);
        painter.line_segment([c + egui::vec2(-d, d), c + egui::vec2(d, -d)], stroke);
    } else {
        painter.line_segment(
            [c + egui::vec2(-4.5, 3.0), c + egui::vec2(4.5, 3.0)],
            stroke,
        );
    }
    resp.on_hover_text(if close { "Schließen" } else { "Minimieren" })
}

/// Drag-Handle (drei waagerechte Striche, "☰") zum Verschieben per Maus.
/// Wird gezeichnet statt als Zeichen ausgegeben, damit es keine Schriftart braucht.
fn drag_handle(ui: &mut egui::Ui, dragging: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(HANDLE_WIDTH, 28.0),
        egui::Sense::click_and_drag(),
    );
    let v = ui.visuals();
    let active = resp.hovered() || dragging;
    let (fill, stroke, color) = if dragging {
        (
            v.widgets.active.bg_fill,
            v.widgets.active.bg_stroke,
            v.widgets.active.fg_stroke.color,
        )
    } else if active {
        (
            v.widgets.hovered.bg_fill,
            v.widgets.hovered.bg_stroke,
            v.widgets.hovered.fg_stroke.color,
        )
    } else {
        (
            v.widgets.inactive.bg_fill,
            v.widgets.inactive.bg_stroke,
            v.widgets.inactive.fg_stroke.color,
        )
    };

    let painter = ui.painter();
    painter.rect(rect, 3.0, fill, stroke);
    let c = rect.center();
    for dy in [-5.0_f32, 0.0, 5.0] {
        painter.line_segment(
            [c + egui::vec2(-6.0, dy), c + egui::vec2(6.0, dy)],
            egui::Stroke::new(1.8_f32, color),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::Grab)
}

// ---------------------------------------------------------------------------
// Uhr, Countdown, Stoppuhr
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum CountdownState {
    Idle,
    Running(Instant),
    Paused(Duration),
    Done,
}

struct Countdown {
    hours: u32,
    minutes: u32,
    seconds: u32,
    state: CountdownState,
}

impl Countdown {
    fn new() -> Self {
        Self {
            hours: 0,
            minutes: 5,
            seconds: 0,
            state: CountdownState::Idle,
        }
    }

    fn configured(&self) -> Duration {
        Duration::from_secs(
            self.hours as u64 * 3600 + self.minutes as u64 * 60 + self.seconds as u64,
        )
    }
}

#[derive(Default)]
struct Stopwatch {
    running_since: Option<Instant>,
    accumulated: Duration,
}

impl Stopwatch {
    fn elapsed(&self) -> Duration {
        self.accumulated
            + self
                .running_since
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO)
    }
}

fn fmt_hms(total_secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        total_secs / 3600,
        (total_secs % 3600) / 60,
        total_secs % 60
    )
}

/// Restzeit aufgerundet auf volle Sekunden (0 erst, wenn wirklich abgelaufen).
fn ceil_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

fn weekday_de(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Montag",
        Weekday::Tue => "Dienstag",
        Weekday::Wed => "Mittwoch",
        Weekday::Thu => "Donnerstag",
        Weekday::Fri => "Freitag",
        Weekday::Sat => "Samstag",
        Weekday::Sun => "Sonntag",
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Taschenrechner (wissenschaftlich)
//
// Die Eingabe wird als Ausdruck aufgebaut (z. B. `2+3*sqrt(9)`) und beim Drücken von
// "=" von einem kleinen Parser mit korrekter Rechenreihenfolge ausgewertet:
// Klammern, Funktionen und Fakultät, dann Potenz, dann × ÷ mod, dann + -.
// Während der Eingabe wird das Ergebnis live als Vorschau berechnet.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum CalcErr {
    Syntax,
    DivZero,
    Domain,
}

impl CalcErr {
    fn message(self) -> &'static str {
        match self {
            CalcErr::Syntax => "Ungültige Eingabe",
            CalcErr::DivZero => "Division durch 0 nicht möglich",
            CalcErr::Domain => "Ungültiger Wert für diese Funktion",
        }
    }
}

type CalcRes = Result<f64, CalcErr>;

fn factorial(x: f64) -> CalcRes {
    if x < 0.0 || (x - x.round()).abs() > 1e-9 || x > 170.0 {
        return Err(CalcErr::Domain);
    }
    let n = x.round() as u32;
    let mut r = 1.0_f64;
    for k in 2..=n {
        r *= k as f64;
    }
    Ok(r)
}

fn apply_func(name: &str, x: f64, deg: bool) -> CalcRes {
    let to_rad = |v: f64| if deg { v.to_radians() } else { v };
    let from_rad = |v: f64| if deg { v.to_degrees() } else { v };
    let r = match name {
        "sqrt" => {
            if x < 0.0 {
                return Err(CalcErr::Domain);
            }
            x.sqrt()
        }
        "cbrt" => x.cbrt(),
        "ln" => {
            if x <= 0.0 {
                return Err(CalcErr::Domain);
            }
            x.ln()
        }
        "log" => {
            if x <= 0.0 {
                return Err(CalcErr::Domain);
            }
            x.log10()
        }
        "log2" => {
            if x <= 0.0 {
                return Err(CalcErr::Domain);
            }
            x.log2()
        }
        "abs" => x.abs(),
        "exp" => x.exp(),
        "sin" => to_rad(x).sin(),
        "cos" => to_rad(x).cos(),
        "tan" => {
            if deg && ((x - 90.0) % 180.0).abs() < 1e-9 {
                return Err(CalcErr::Domain);
            }
            to_rad(x).tan()
        }
        "asin" => {
            if !(-1.0..=1.0).contains(&x) {
                return Err(CalcErr::Domain);
            }
            from_rad(x.asin())
        }
        "acos" => {
            if !(-1.0..=1.0).contains(&x) {
                return Err(CalcErr::Domain);
            }
            from_rad(x.acos())
        }
        "atan" => from_rad(x.atan()),
        _ => return Err(CalcErr::Syntax),
    };
    Ok(r)
}

struct Parser {
    c: Vec<char>,
    pos: usize,
    deg: bool,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek() == Some(' ') {
            self.pos += 1;
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(k, ch)| self.c.get(self.pos + k) == Some(&ch))
    }

    fn expect(&mut self, ch: char) -> Result<(), CalcErr> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(CalcErr::Syntax)
        }
    }

    /// Summe/Differenz
    fn expr(&mut self) -> CalcRes {
        let mut v = self.term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    v += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    v -= self.term()?;
                }
                _ => break,
            }
        }
        Ok(v)
    }

    /// Produkt/Quotient/Rest
    fn term(&mut self) -> CalcRes {
        let mut v = self.unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    v *= self.unary()?;
                }
                Some('/') => {
                    self.pos += 1;
                    let d = self.unary()?;
                    if d == 0.0 {
                        return Err(CalcErr::DivZero);
                    }
                    v /= d;
                }
                Some('m') if self.starts_with("mod") => {
                    self.pos += 3;
                    let d = self.unary()?;
                    if d == 0.0 {
                        return Err(CalcErr::DivZero);
                    }
                    v %= d;
                }
                _ => break,
            }
        }
        Ok(v)
    }

    /// Vorzeichen (bindet schwächer als ^: -2^2 = -4)
    fn unary(&mut self) -> CalcRes {
        self.skip_ws();
        match self.peek() {
            Some('-') => {
                self.pos += 1;
                Ok(-self.unary()?)
            }
            Some('+') => {
                self.pos += 1;
                self.unary()
            }
            _ => self.power(),
        }
    }

    /// Potenz (rechtsassoziativ: 2^3^2 = 2^9)
    fn power(&mut self) -> CalcRes {
        let base = self.postfix()?;
        self.skip_ws();
        if self.peek() == Some('^') {
            self.pos += 1;
            let exp = self.unary()?;
            let r = base.powf(exp);
            if !r.is_finite() {
                return Err(CalcErr::Domain);
            }
            Ok(r)
        } else {
            Ok(base)
        }
    }

    /// Fakultät (n!)
    fn postfix(&mut self) -> CalcRes {
        let mut v = self.primary()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('!') {
                self.pos += 1;
                v = factorial(v)?;
            } else {
                break;
            }
        }
        Ok(v)
    }

    fn primary(&mut self) -> CalcRes {
        self.skip_ws();
        match self.peek() {
            Some(ch) if ch.is_ascii_digit() || ch == '.' => self.number(),
            Some('π') => {
                self.pos += 1;
                Ok(std::f64::consts::PI)
            }
            Some('(') => {
                self.pos += 1;
                let v = self.expr()?;
                self.expect(')')?;
                Ok(v)
            }
            Some(ch) if ch.is_ascii_alphabetic() => {
                let start = self.pos;
                while matches!(self.peek(), Some(d) if d.is_ascii_alphanumeric()) {
                    self.pos += 1;
                }
                let name: String = self.c[start..self.pos].iter().collect();
                if name == "e" {
                    return Ok(std::f64::consts::E);
                }
                self.expect('(')?;
                let arg = self.expr()?;
                self.expect(')')?;
                apply_func(&name, arg, self.deg)
            }
            _ => Err(CalcErr::Syntax),
        }
    }

    fn number(&mut self) -> CalcRes {
        let start = self.pos;
        while matches!(self.peek(), Some(d) if d.is_ascii_digit() || d == '.') {
            self.pos += 1;
        }
        // Exponent-Schreibweise (entsteht bei sehr großen/kleinen Ergebnissen): 1e20, 1e-7
        if self.peek() == Some('e') {
            let n1 = self.c.get(self.pos + 1).copied();
            let n2 = self.c.get(self.pos + 2).copied();
            let digit = |o: Option<char>| matches!(o, Some(d) if d.is_ascii_digit());
            if digit(n1) || (matches!(n1, Some('+') | Some('-')) && digit(n2)) {
                self.pos += 2;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
        }
        let text: String = self.c[start..self.pos].iter().collect();
        text.parse::<f64>().map_err(|_| CalcErr::Syntax)
    }
}

fn evaluate_expr(expr: &str, degrees: bool) -> CalcRes {
    let mut p = Parser {
        c: expr.chars().collect(),
        pos: 0,
        deg: degrees,
    };
    let v = p.expr()?;
    p.skip_ws();
    if p.pos != p.c.len() {
        return Err(CalcErr::Syntax);
    }
    if !v.is_finite() {
        return Err(CalcErr::Domain);
    }
    // Rundungsrauschen (sin(π) = 1,2e-16) wird zu 0.
    Ok(if v.abs() < 1e-12 { 0.0 } else { v })
}

/// Zahl für die Anzeige: bis zu 10 Nachkommastellen, ohne überflüssige Nullen,
/// mit Komma. (0,1 + 0,2 ergibt so sauber "0,3".)
fn fmt_number(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let a = v.abs();
    let text = if !(1e-9..1e15).contains(&a) {
        format!("{v:e}")
    } else {
        let mut t = format!("{v:.10}");
        while t.ends_with('0') {
            t.pop();
        }
        if t.ends_with('.') {
            t.pop();
        }
        t
    };
    text.replace('.', ",")
}

/// Wie `fmt_number`, aber mit Punkt (für den internen Ausdruck).
fn fmt_plain(v: f64) -> String {
    fmt_number(v).replace(',', ".")
}

/// Ausdruck für die Anzeige aufbereiten.
fn display_expr(expr: &str) -> String {
    expr.replace('*', "×").replace('/', "÷").replace('.', ",")
}

fn open_parens(s: &str) -> i32 {
    s.chars().fold(0, |n, c| match c {
        '(' => n + 1,
        ')' => n - 1,
        _ => n,
    })
}

fn is_plain_number(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.')
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum CalcKey {
    Digit(char),
    Dot,
    /// + - * / ^
    Op(char),
    Mod,
    Equals,
    Clear,
    Back,
    Negate,
    Second,
    AngleMode,
    Pi,
    E,
    Ans,
    Open,
    Close,
    /// Funktion, die den letzten Operanden umschließt: sqrt(…), ln(…), sin(…) …
    Func(&'static str),
    /// x², x³
    Power(u8),
    Reciprocal,
    Factorial,
    /// 10^x, 2^x
    BasePow(u8),
}

impl CalcKey {
    fn label(self, deg: bool) -> String {
        match self {
            CalcKey::Digit(c) => c.to_string(),
            CalcKey::Dot => ",".to_string(),
            CalcKey::Op('*') => "×".to_string(),
            CalcKey::Op('/') => "÷".to_string(),
            CalcKey::Op('^') => "x^y".to_string(),
            CalcKey::Op(c) => c.to_string(),
            CalcKey::Mod => "mod".to_string(),
            CalcKey::Equals => "=".to_string(),
            CalcKey::Clear => "C".to_string(),
            CalcKey::Back => "Entf".to_string(),
            CalcKey::Negate => "+/-".to_string(),
            CalcKey::Second => "2nd".to_string(),
            CalcKey::AngleMode => (if deg { "DEG" } else { "RAD" }).to_string(),
            CalcKey::Pi => "π".to_string(),
            CalcKey::E => "e".to_string(),
            CalcKey::Ans => "Ans".to_string(),
            CalcKey::Open => "(".to_string(),
            CalcKey::Close => ")".to_string(),
            CalcKey::Func("abs") => "|x|".to_string(),
            CalcKey::Func("exp") => "e^x".to_string(),
            CalcKey::Func(name) => name.to_string(),
            CalcKey::Power(2) => "x²".to_string(),
            CalcKey::Power(3) => "x³".to_string(),
            CalcKey::Power(n) => format!("x^{n}"),
            CalcKey::Reciprocal => "1/x".to_string(),
            CalcKey::Factorial => "n!".to_string(),
            CalcKey::BasePow(b) => format!("{b}^x"),
        }
    }
}

/// Eine Taste mit Normal- und "2nd"-Belegung.
#[derive(Clone, Copy)]
struct Key {
    normal: CalcKey,
    second: CalcKey,
}

const fn k(a: CalcKey) -> Key {
    Key {
        normal: a,
        second: a,
    }
}

const fn k2(a: CalcKey, b: CalcKey) -> Key {
    Key {
        normal: a,
        second: b,
    }
}

const fn d(c: char) -> Key {
    k(CalcKey::Digit(c))
}

const CALC_ROWS: [[Key; 5]; 8] = [
    [
        k(CalcKey::AngleMode),
        k2(CalcKey::Func("sin"), CalcKey::Func("asin")),
        k2(CalcKey::Func("cos"), CalcKey::Func("acos")),
        k2(CalcKey::Func("tan"), CalcKey::Func("atan")),
        k(CalcKey::Ans),
    ],
    [
        k(CalcKey::Second),
        k(CalcKey::Pi),
        k(CalcKey::E),
        k(CalcKey::Clear),
        k(CalcKey::Back),
    ],
    [
        k2(CalcKey::Power(2), CalcKey::Power(3)),
        k(CalcKey::Reciprocal),
        k(CalcKey::Func("abs")),
        k(CalcKey::Func("exp")),
        k(CalcKey::Mod),
    ],
    [
        k2(CalcKey::Func("sqrt"), CalcKey::Func("cbrt")),
        k(CalcKey::Open),
        k(CalcKey::Close),
        k(CalcKey::Factorial),
        k(CalcKey::Op('/')),
    ],
    [
        k(CalcKey::Op('^')),
        d('7'),
        d('8'),
        d('9'),
        k(CalcKey::Op('*')),
    ],
    [
        k2(CalcKey::BasePow(10), CalcKey::BasePow(2)),
        d('4'),
        d('5'),
        d('6'),
        k(CalcKey::Op('-')),
    ],
    [
        k2(CalcKey::Func("log"), CalcKey::Func("log2")),
        d('1'),
        d('2'),
        d('3'),
        k(CalcKey::Op('+')),
    ],
    [
        k(CalcKey::Func("ln")),
        k(CalcKey::Negate),
        d('0'),
        k(CalcKey::Dot),
        k(CalcKey::Equals),
    ],
];

const MAX_DIGITS: usize = 15;
const BACKSPACE_TOKENS: [&str; 14] = [
    "sqrt(", "cbrt(", "asin(", "acos(", "atan(", "log2(", "sin(", "cos(", "tan(", "abs(", "exp(",
    "log(", "ln(", " mod ",
];

struct Calculator {
    /// Interner Ausdruck mit Punkt als Dezimaltrenner, `*` und `/`.
    expr: String,
    /// Zeile "Ausdruck =" nach einer Berechnung.
    history: String,
    /// `true`: `expr` ist ein fertiges Ergebnis (nächste Ziffer beginnt neu).
    after_eval: bool,
    error: Option<CalcErr>,
    second: bool,
    degrees: bool,
    last_result: Option<String>,
}

impl Calculator {
    fn new() -> Self {
        Self {
            expr: String::new(),
            history: String::new(),
            after_eval: false,
            error: None,
            second: false,
            degrees: true,
            last_result: None,
        }
    }

    fn clear(&mut self) {
        self.expr.clear();
        self.history.clear();
        self.after_eval = false;
        self.error = None;
    }

    /// Vor jeder Eingabe: Fehler zurücksetzen, nach "=" entweder neu beginnen
    /// (`keep_result = false`, z. B. bei Ziffern) oder mit dem Ergebnis weiterrechnen.
    fn begin(&mut self, keep_result: bool) {
        if self.error.is_some() {
            self.clear();
            return;
        }
        if self.after_eval {
            if !keep_result {
                self.expr.clear();
            }
            self.after_eval = false;
            self.history.clear();
        }
    }

    fn ends_with_operand(&self) -> bool {
        matches!(
            self.expr.chars().last(),
            Some(c) if c.is_ascii_digit() || matches!(c, '.' | ')' | '!' | 'π' | 'e')
        )
    }

    /// Anfang (Zeichenindex) des letzten Operanden: Zahl, Konstante oder
    /// Klammer-/Funktionsausdruck samt `!` und einem führenden unären Minus.
    fn operand_start(&self) -> Option<usize> {
        let c: Vec<char> = self.expr.chars().collect();
        let mut i = c.len();
        while i > 0 && c[i - 1] == '!' {
            i -= 1;
        }
        if i == 0 {
            return None;
        }
        let last = c[i - 1];
        let mut s;
        if last == ')' {
            let mut depth = 0;
            let mut j = i;
            loop {
                if j == 0 {
                    return None;
                }
                j -= 1;
                if c[j] == ')' {
                    depth += 1;
                } else if c[j] == '(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            s = j;
            while s > 0 && c[s - 1].is_ascii_alphanumeric() {
                s -= 1;
            }
        } else if last.is_ascii_digit() || last == '.' {
            s = i;
            while s > 0 && (c[s - 1].is_ascii_digit() || c[s - 1] == '.') {
                s -= 1;
            }
            let num = |ch: char| ch.is_ascii_digit() || ch == '.';
            if s >= 2 && c[s - 1] == 'e' && num(c[s - 2]) {
                s -= 1;
                while s > 0 && num(c[s - 1]) {
                    s -= 1;
                }
            } else if s >= 3 && matches!(c[s - 1], '+' | '-') && c[s - 2] == 'e' && num(c[s - 3]) {
                s -= 2;
                while s > 0 && num(c[s - 1]) {
                    s -= 1;
                }
            }
        } else if last == 'π' || last == 'e' {
            s = i - 1;
        } else {
            return None;
        }
        if s > 0
            && c[s - 1] == '-'
            && (s == 1 || matches!(c[s - 2], '(' | '+' | '-' | '*' | '/' | '^' | ' '))
        {
            s -= 1;
        }
        Some(s)
    }

    /// Ersetzt den letzten Operanden durch `f(operand)`. `false`, wenn keiner da ist.
    fn transform_operand(&mut self, f: impl FnOnce(&str) -> String) -> bool {
        let Some(s) = self.operand_start() else {
            return false;
        };
        let chars: Vec<char> = self.expr.chars().collect();
        let prefix: String = chars[..s].iter().collect();
        let operand: String = chars[s..].iter().collect();
        self.expr = format!("{prefix}{}", f(&operand));
        true
    }

    fn trailing_number_run(&self) -> String {
        let rev: Vec<char> = self
            .expr
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        rev.into_iter().rev().collect()
    }

    fn digit(&mut self, d: char) {
        self.begin(false);
        if matches!(self.expr.chars().last(), Some(')' | '!' | 'π' | 'e')) {
            self.expr.push('*');
        }
        let run = self.trailing_number_run();
        if run == "0" {
            self.expr.pop();
        } else if run.chars().filter(|c| c.is_ascii_digit()).count() >= MAX_DIGITS {
            return;
        }
        self.expr.push(d);
    }

    fn dot(&mut self) {
        self.begin(false);
        if matches!(self.expr.chars().last(), Some(')' | '!' | 'π' | 'e')) {
            self.expr.push('*');
        }
        let run = self.trailing_number_run();
        if run.contains('.') {
            return;
        }
        if run.is_empty() {
            self.expr.push('0');
        }
        self.expr.push('.');
    }

    fn strip_trailing_operators(s: &mut String) {
        loop {
            let trimmed = s.trim_end().to_string();
            *s = trimmed;
            if s.ends_with("mod") {
                s.truncate(s.len() - 3);
                continue;
            }
            match s.chars().last() {
                Some(c) if "+-*/^".contains(c) => {
                    s.pop();
                }
                _ => break,
            }
        }
    }

    fn push_operator(&mut self, text: &str) {
        self.begin(true);
        Self::strip_trailing_operators(&mut self.expr);
        if self.expr.is_empty() {
            if text != "-" {
                self.expr.push('0');
            }
            self.expr.push_str(text);
            return;
        }
        if self.expr.ends_with('(') {
            if text == "-" {
                self.expr.push('-');
            }
            return;
        }
        self.expr.push_str(text);
    }

    fn open_paren(&mut self) {
        self.begin(false);
        if self.ends_with_operand() {
            self.expr.push('*');
        }
        self.expr.push('(');
    }

    fn close_paren(&mut self) {
        self.begin(true);
        if open_parens(&self.expr) > 0 && self.ends_with_operand() {
            self.expr.push(')');
        }
    }

    fn constant(&mut self, c: char) {
        self.begin(false);
        if self.ends_with_operand() {
            self.expr.push('*');
        }
        self.expr.push(c);
    }

    fn answer(&mut self) {
        self.begin(false);
        let Some(ans) = self.last_result.clone() else {
            return;
        };
        if self.ends_with_operand() {
            self.expr.push('*');
        }
        if ans.starts_with('-') {
            self.expr.push_str(&format!("({ans})"));
        } else {
            self.expr.push_str(&ans);
        }
    }

    fn function(&mut self, name: &str) {
        self.begin(true);
        if !self.transform_operand(|o| format!("{name}({o})")) {
            self.expr.push_str(name);
            self.expr.push('(');
        }
    }

    fn power(&mut self, n: u8) {
        self.begin(true);
        self.transform_operand(|o| {
            if o.starts_with('-') {
                format!("({o})^{n}")
            } else {
                format!("{o}^{n}")
            }
        });
    }

    fn reciprocal(&mut self) {
        self.begin(true);
        self.transform_operand(|o| {
            if o.chars().all(|c| c.is_ascii_digit() || c == '.') {
                format!("(1/{o})")
            } else {
                format!("(1/({o}))")
            }
        });
    }

    fn factorial_key(&mut self) {
        self.begin(true);
        self.transform_operand(|o| {
            if o.starts_with('-') {
                format!("({o})!")
            } else {
                format!("{o}!")
            }
        });
    }

    fn base_pow(&mut self, b: u8) {
        self.begin(true);
        if !self.transform_operand(|o| format!("{b}^({o})")) {
            self.expr.push_str(&format!("{b}^("));
        }
    }

    fn negate(&mut self) {
        self.begin(true);
        self.transform_operand(|o| match o.strip_prefix('-') {
            Some(rest) => rest.to_string(),
            None => format!("-{o}"),
        });
    }

    fn backspace(&mut self) {
        if self.error.is_some() {
            self.clear();
            return;
        }
        self.after_eval = false;
        self.history.clear();
        for token in BACKSPACE_TOKENS {
            if self.expr.ends_with(token) {
                let new_len = self.expr.len() - token.len();
                self.expr.truncate(new_len);
                return;
            }
        }
        self.expr.pop();
    }

    /// Ausdruck ohne hängende Operatoren, mit geschlossenen Klammern.
    fn closed_expr(&self) -> String {
        let mut e = self.expr.clone();
        Self::strip_trailing_operators(&mut e);
        for _ in 0..open_parens(&e).max(0) {
            e.push(')');
        }
        e
    }

    fn equals(&mut self) {
        if self.error.is_some() || self.after_eval || self.expr.is_empty() {
            return;
        }
        let e = self.closed_expr();
        if e.is_empty() {
            return;
        }
        match evaluate_expr(&e, self.degrees) {
            Ok(v) => {
                let result = fmt_plain(v);
                self.history = format!("{} =", display_expr(&e));
                self.expr = result.clone();
                self.last_result = Some(result);
                self.after_eval = true;
            }
            Err(err) => self.error = Some(err),
        }
    }

    /// Live-Ergebnis (Vorschau), solange noch nicht "=" gedrückt wurde.
    fn preview(&self) -> Option<String> {
        if self.error.is_some() || self.after_eval || self.expr.is_empty() {
            return None;
        }
        let e = self.closed_expr();
        if e.is_empty() || is_plain_number(&e) {
            return None;
        }
        evaluate_expr(&e, self.degrees).ok().map(fmt_number)
    }

    fn press(&mut self, key: CalcKey) {
        match key {
            CalcKey::Digit(d) => self.digit(d),
            CalcKey::Dot => self.dot(),
            CalcKey::Op(c) => self.push_operator(&c.to_string()),
            CalcKey::Mod => self.push_operator(" mod "),
            CalcKey::Equals => self.equals(),
            CalcKey::Clear => self.clear(),
            CalcKey::Back => self.backspace(),
            CalcKey::Negate => self.negate(),
            CalcKey::Second => self.second = !self.second,
            CalcKey::AngleMode => self.degrees = !self.degrees,
            CalcKey::Pi => self.constant('π'),
            CalcKey::E => self.constant('e'),
            CalcKey::Ans => self.answer(),
            CalcKey::Open => self.open_paren(),
            CalcKey::Close => self.close_paren(),
            CalcKey::Func(name) => self.function(name),
            CalcKey::Power(n) => self.power(n),
            CalcKey::Reciprocal => self.reciprocal(),
            CalcKey::Factorial => self.factorial_key(),
            CalcKey::BasePow(b) => self.base_pow(b),
        }
    }

    /// Eingabe über die Tastatur (Zeichen aus `Event::Text`).
    fn press_char(&mut self, c: char) {
        match c {
            '0'..='9' => self.digit(c),
            ',' | '.' => self.dot(),
            '+' | '-' | '*' | '/' | '^' => self.push_operator(&c.to_string()),
            '(' => self.open_paren(),
            ')' => self.close_paren(),
            '=' => self.equals(),
            '!' => self.factorial_key(),
            '%' => self.push_operator(" mod "),
            _ => {}
        }
    }
}

/// Eine Taste des Rechners zeichnen. Gibt die gedrückte Taste zurück (je nach
/// "2nd"-Zustand die Normal- oder Zweitbelegung).
fn calc_button(
    ui: &mut egui::Ui,
    key: Key,
    calc: &Calculator,
    size: egui::Vec2,
    func_fill: egui::Color32,
    accent: egui::Color32,
) -> Option<CalcKey> {
    let resolved = if calc.second { key.second } else { key.normal };
    let text = egui::RichText::new(resolved.label(calc.degrees)).size(15.0);
    let button = match resolved {
        CalcKey::Equals => {
            egui::Button::new(text.color(egui::Color32::WHITE).strong()).fill(accent)
        }
        CalcKey::Second if calc.second => {
            egui::Button::new(text.color(egui::Color32::WHITE).strong()).fill(accent)
        }
        CalcKey::Digit(_) | CalcKey::Dot | CalcKey::Negate => egui::Button::new(text),
        _ => egui::Button::new(text).fill(func_fill),
    };
    if ui.add_sized(size, button).clicked() {
        Some(resolved)
    } else {
        None
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Snippets,
    Clock,
    Calculator,
    Settings,
}

/// Zustand des Dialogs "Neu" / "Bearbeiten".
struct EditState {
    /// `None` = neues Snippet.
    index: Option<usize>,
    description: String,
    content: String,
    group: String,
    just_opened: bool,
}

struct SnippetApp {
    cfg: Config,
    path: PathBuf,
    tab: Tab,
    editing: Option<EditState>,
    /// Laufender Drag-Vorgang: (Gruppenname, Position des gezogenen Snippets in der Gruppe).
    drag: Option<(String, usize)>,
    /// Eingeklappte Gruppen (nur während der Laufzeit, nicht gespeichert).
    collapsed: HashSet<String>,
    clipboard: Option<Clipboard>,
    copied: Option<(usize, Instant)>,
    confirm_delete_all: bool,
    show_close_dialog: bool,
    remember_close_choice: bool,
    status: Option<String>,
    countdown: Countdown,
    stopwatch: Stopwatch,
    calc: Calculator,
    palette_dirty: bool,
    /// Zuletzt angewendete Fenstergröße (für die abgerundete Fensterform).
    region_size: (i32, i32),
    /// `true`, wenn Tray-Icon UND Fenster-Handle verfügbar sind. Nur dann darf
    /// das Fenster versteckt werden (sonst wäre die App nicht mehr erreichbar).
    has_tray: bool,
}

impl SnippetApp {
    fn new(cc: &eframe::CreationContext<'_>, cfg: Config, path: PathBuf) -> Self {
        apply_theme(&cc.egui_ctx, &cfg);

        let mut status = None;

        // Autostart-Eintrag mit dem gespeicherten Zustand abgleichen
        // (aktualisiert auch den Pfad, falls die .exe verschoben wurde).
        if cfg.autostart {
            if let Err(e) = set_autostart_registry(true) {
                status = Some(format!("Autostart konnte nicht gesetzt werden: {e}"));
            }
        } else if autostart_is_enabled() {
            let _ = set_autostart_registry(false);
        }

        // Fenster-Handle merken (für Show/Hide per Win32).
        let hwnd_ok = match window_hwnd(cc) {
            Some(h) => {
                MAIN_HWND.store(h, Ordering::SeqCst);
                true
            }
            None => {
                status = Some("Fenster-Handle nicht verfügbar – Tray deaktiviert.".to_string());
                false
            }
        };

        let tray_ok = hwnd_ok
            && match create_tray(&cc.egui_ctx) {
                Ok(()) => true,
                Err(e) => {
                    status = Some(format!("Tray-Icon konnte nicht erstellt werden: {e}"));
                    false
                }
            };

        Self {
            cfg,
            path,
            tab: Tab::Snippets,
            editing: None,
            drag: None,
            collapsed: HashSet::new(),
            clipboard: Clipboard::new().ok(),
            copied: None,
            confirm_delete_all: false,
            show_close_dialog: false,
            remember_close_choice: true,
            status,
            countdown: Countdown::new(),
            stopwatch: Stopwatch::default(),
            calc: Calculator::new(),
            palette_dirty: false,
            region_size: (0, 0),
            has_tray: tray_ok,
        }
    }

    fn save(&mut self) {
        let result = serde_json::to_string_pretty(&self.cfg)
            .map_err(|e| e.to_string())
            .and_then(|json| std::fs::write(&self.path, json).map_err(|e| e.to_string()));
        if let Err(e) = result {
            self.status = Some(format!("Speichern fehlgeschlagen: {e}"));
        }
    }

    /// Snippet-Indizes je Gruppe, Gruppen in der Reihenfolge ihres ersten Auftretens.
    fn grouped(&self) -> Vec<(String, Vec<usize>)> {
        let mut out: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, s) in self.cfg.snippets.iter().enumerate() {
            match out.iter_mut().find(|(g, _)| *g == s.group) {
                Some((_, v)) => v.push(i),
                None => out.push((s.group.clone(), vec![i])),
            }
        }
        out
    }

    /// Verschiebt ein Snippet innerhalb seiner Gruppe und speichert sofort.
    ///
    /// `idxs`: Positionen der Gruppe im Gesamt-Vektor, `from_pos`: Position des
    /// gezogenen Snippets in dieser Liste, `slot`: Einfügemarke (0 = vor dem ersten,
    /// `idxs.len()` = hinter dem letzten Eintrag). Die belegten Plätze im Vektor
    /// bleiben dieselben, nur die Snippets der Gruppe tauschen ihre Reihenfolge –
    /// Snippets anderer Gruppen werden nicht angefasst.
    fn reorder_in_group(&mut self, idxs: &[usize], from_pos: usize, slot: usize) {
        if from_pos >= idxs.len() || slot > idxs.len() {
            return;
        }
        let to_pos = if slot > from_pos { slot - 1 } else { slot };
        if to_pos == from_pos {
            return;
        }
        let mut items: Vec<Snippet> = idxs.iter().map(|&i| self.cfg.snippets[i].clone()).collect();
        let moved = items.remove(from_pos);
        items.insert(to_pos, moved);
        for (&vec_index, snippet) in idxs.iter().zip(items) {
            self.cfg.snippets[vec_index] = snippet;
        }
        self.copied = None;
        self.editing = None;
        self.save(); // Reihenfolge sofort in die config.json schreiben
    }

    fn delete_snippet(&mut self, index: usize) {
        if index < self.cfg.snippets.len() {
            self.cfg.snippets.remove(index);
            self.copied = None;
            self.editing = None; // Indizes haben sich verschoben
            self.save();
        }
    }

    fn start_new(&mut self, group: String) {
        self.editing = Some(EditState {
            index: None,
            description: String::new(),
            content: String::new(),
            group,
            just_opened: true,
        });
    }

    fn start_editing(&mut self, index: usize) {
        if let Some(s) = self.cfg.snippets.get(index) {
            self.editing = Some(EditState {
                index: Some(index),
                description: s.description.clone(),
                content: s.content.clone(),
                group: s.group.clone(),
                just_opened: true,
            });
        }
    }

    fn copy_snippet(&mut self, index: usize) {
        let Some(text) = self.cfg.snippets.get(index).map(|s| s.content.clone()) else {
            return;
        };
        if self.clipboard.is_none() {
            self.clipboard = Clipboard::new().ok();
        }
        match self.clipboard.as_mut().map(|cb| cb.set_text(text)) {
            Some(Ok(())) => {
                self.copied = Some((index, Instant::now()));
                self.status = None;
            }
            Some(Err(e)) => self.status = Some(format!("Kopieren fehlgeschlagen: {e}")),
            None => self.status = Some("Zwischenablage nicht verfügbar.".to_string()),
        }
    }

    fn set_autostart(&mut self, on: bool) {
        let result = set_autostart_registry(on);
        match result {
            Ok(()) => {
                self.cfg.autostart = on;
                self.status = None;
                self.save();
            }
            Err(e) => self.status = Some(format!("Autostart-Änderung fehlgeschlagen: {e}")),
        }
    }

    // ----- Custom-Titelleiste -----------------------------------------------

    fn ui_titlebar(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(ctx.style().visuals.widgets.inactive.bg_fill)
            .rounding(egui::Rounding {
                nw: WINDOW_ROUNDING,
                ne: WINDOW_ROUNDING,
                sw: 0.0,
                se: 0.0,
            })
            .inner_margin(egui::Margin::symmetric(10.0, 0.0));

        egui::TopBottomPanel::top("titlebar")
            .exact_height(TITLEBAR_HEIGHT)
            .frame(frame)
            .show(ctx, |ui| {
                // Die ganze Leiste ist Griff zum Verschieben des Fensters.
                // (Kein Doppelklick-Maximieren: Es gibt bewusst keinen Vollbild-Modus.)
                let drag = ui.interact(
                    ui.max_rect(),
                    egui::Id::new("titlebar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Snippet-Manager").small().strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if title_button(ui, true).clicked() {
                            // Läuft über die normale Schließen-Logik (Tray/Rückfrage/Beenden).
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if title_button(ui, false).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    });
                });
            });
    }

    // ----- Tab: Snippets ----------------------------------------------------

    fn ui_snippets(&mut self, ui: &mut egui::Ui) {
        // --- Kopfzeile: nur der Button "Neu" ---
        ui.horizontal_wrapped(|ui| {
            if ui.button("Neu").clicked() {
                self.start_new(String::new());
            }
        });
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);

        if self.cfg.snippets.is_empty() {
            ui.weak("Noch keine Snippets gespeichert.");
            ui.weak("Mit „Neu“ eines anlegen.");
            return;
        }

        let dark = ui.visuals().dark_mode;
        let ok_color = if dark {
            egui::Color32::from_rgb(90, 220, 130)
        } else {
            egui::Color32::from_rgb(10, 130, 50)
        };
        let delete_color = if dark {
            egui::Color32::from_rgb(255, 110, 110)
        } else {
            egui::Color32::from_rgb(190, 30, 30)
        };
        let accent = ui.visuals().selection.bg_fill;

        let groups = self.grouped();
        // Bei nur einer unbenannten Gruppe keine Überschrift anzeigen.
        let show_headers = !(groups.len() == 1 && groups[0].0.is_empty());
        let copied = self.copied.filter(|(_, t)| t.elapsed() < FEEDBACK_DURATION);
        let drag = self.drag.clone();
        let pointer_y = ui.input(|i| i.pointer.latest_pos()).map(|p| p.y);

        let mut to_copy: Option<usize> = None;
        let mut to_edit: Option<usize> = None;
        let mut to_delete: Option<usize> = None;
        let mut toggle_group: Option<String> = None;
        let mut new_in_group: Option<String> = None;
        let mut drag_start: Option<(String, usize)> = None;
        let mut drag_released = false;
        let mut drop_slot: Option<usize> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (gname, idxs) in groups.iter() {
                    let collapsed = show_headers && self.collapsed.contains(gname);

                    if show_headers {
                        let sign = if collapsed { "+" } else { "−" };
                        let title = egui::RichText::new(format!(
                            "{sign}  {} ({})",
                            group_label(gname),
                            idxs.len()
                        ))
                        .strong();
                        let resp = ui.add_sized(
                            [ui.available_width(), 26.0],
                            egui::Button::new(title)
                                .fill(ui.visuals().widgets.noninteractive.bg_stroke.color)
                                .stroke(ui.visuals().widgets.inactive.bg_stroke),
                        );
                        if resp.clicked() {
                            toggle_group = Some(gname.clone());
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Neues Snippet in dieser Gruppe").clicked() {
                                new_in_group = Some(gname.clone());
                                ui.close_menu();
                            }
                        });
                    }

                    if collapsed {
                        continue;
                    }

                    for (p, &i) in idxs.iter().enumerate() {
                        let rect = ui
                            .push_id(i, |ui| {
                                ui.horizontal(|ui| {
                                    if show_headers {
                                        ui.add_space(10.0);
                                    }
                                    let spacing = ui.spacing().item_spacing.x;
                                    let button_width =
                                        (ui.available_width() - HANDLE_WIDTH - spacing).max(20.0);

                                    let is_copied = matches!(copied, Some((idx, _)) if idx == i);
                                    let label = if is_copied {
                                        egui::RichText::new("Kopiert!").color(ok_color).strong()
                                    } else {
                                        egui::RichText::new(shorten(
                                            &self.cfg.snippets[i].description,
                                            MAX_LABEL_CHARS,
                                        ))
                                    };

                                    let response = ui
                                        .add_sized([button_width, 28.0], egui::Button::new(label))
                                        .on_hover_text("Linksklick: kopieren · Rechtsklick: Menü");
                                    if response.clicked() {
                                        to_copy = Some(i);
                                    }
                                    response.context_menu(|ui| {
                                        if ui.button("Kopieren").clicked() {
                                            to_copy = Some(i);
                                            ui.close_menu();
                                        }
                                        if ui.button("Bearbeiten").clicked() {
                                            to_edit = Some(i);
                                            ui.close_menu();
                                        }
                                        ui.separator();
                                        let delete_label =
                                            egui::RichText::new("Löschen").color(delete_color);
                                        if ui.button(delete_label).clicked() {
                                            to_delete = Some(i);
                                            ui.close_menu();
                                        }
                                    });

                                    // Drag-Handle ganz rechts.
                                    let is_dragged = matches!(
                                        &drag,
                                        Some((g, from)) if g == gname && *from == p
                                    );
                                    let handle = drag_handle(ui, is_dragged);
                                    if handle.drag_started() {
                                        drag_start = Some((gname.clone(), p));
                                    }
                                    if handle.drag_stopped() {
                                        drag_released = true;
                                    }
                                })
                                .response
                                .rect
                            })
                            .inner;

                        // Während des Ziehens: Quelle abdunkeln, Einfügemarke zeigen.
                        if let (Some((dg, from)), Some(py)) = (&drag, pointer_y) {
                            if dg == gname {
                                if *from == p {
                                    ui.painter().rect_filled(
                                        rect,
                                        3.0,
                                        accent.linear_multiply(0.35),
                                    );
                                }
                                let half = ui.spacing().item_spacing.y / 2.0;
                                let top = rect.top() - half;
                                let bottom = rect.bottom() + half;
                                let first = p == 0;
                                let last = p + 1 == idxs.len();
                                if (py >= top && py < bottom)
                                    || (first && py < top)
                                    || (last && py >= bottom)
                                {
                                    let before = py < rect.center().y;
                                    drop_slot = Some(if before { p } else { p + 1 });
                                    let y = if before { top } else { bottom };
                                    ui.painter().line_segment(
                                        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                                        egui::Stroke::new(2.5_f32, accent),
                                    );
                                }
                            }
                        }
                    }
                    ui.add_space(4.0);
                }
            });

        // --- Drag beendet: neue Reihenfolge übernehmen und speichern ---
        if drag_released {
            if let (Some((g, from)), Some(slot)) = (drag.as_ref(), drop_slot) {
                if let Some((_, idxs)) = groups.iter().find(|(n, _)| n == g) {
                    self.reorder_in_group(idxs, *from, slot);
                }
            }
            self.drag = None;
        } else if let Some(start) = drag_start {
            self.drag = Some(start);
        } else if self.drag.is_some() && !ui.input(|i| i.pointer.any_down()) {
            // Sicherheitsnetz: Maustaste ist oben, aber kein Release-Ereignis angekommen.
            self.drag = None;
        }
        if self.drag.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }

        if let Some(g) = toggle_group {
            if !self.collapsed.remove(&g) {
                self.collapsed.insert(g);
            }
        }
        if let Some(g) = new_in_group {
            self.start_new(g);
        } else if let Some(i) = to_delete {
            self.delete_snippet(i);
        } else if let Some(i) = to_edit {
            self.start_editing(i);
        } else if let Some(i) = to_copy {
            self.copy_snippet(i);
        }
    }

    // ----- Dialog "Neu" / "Bearbeiten" (Pop-up) -----------------------------

    fn ui_edit_window(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.editing.take() else {
            return;
        };

        let existing_groups: Vec<String> = self
            .grouped()
            .into_iter()
            .map(|(g, _)| g)
            .filter(|g| !g.is_empty())
            .collect();

        let screen = ctx.screen_rect();
        let width = (screen.width() - 40.0).clamp(140.0, 300.0);
        let content_height = (screen.height() - 230.0).clamp(50.0, 220.0);

        let mut keep_open = true;
        let mut save = false;
        let title = if edit.index.is_some() {
            "Snippet bearbeiten"
        } else {
            "Neues Snippet"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(width);

                ui.label("Beschreibung");
                let desc_resp = ui.add(
                    egui::TextEdit::singleline(&mut edit.description)
                        .hint_text("z. B. E-Mail-Signatur")
                        .desired_width(f32::INFINITY),
                );
                if edit.just_opened {
                    desc_resp.request_focus();
                    edit.just_opened = false;
                }

                ui.add_space(4.0);
                ui.label("Inhalt (wird beim Klick kopiert)");
                egui::ScrollArea::vertical()
                    .id_salt("edit_content_scroll")
                    .max_height(content_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut edit.content)
                                .hint_text("Der Text, der in die Zwischenablage kopiert wird …")
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                    });

                ui.add_space(4.0);
                ui.label("Gruppe (optional)");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.group)
                            .hint_text("Ohne Gruppe")
                            .desired_width((width - 90.0).max(60.0)),
                    );
                    if !existing_groups.is_empty() {
                        egui::ComboBox::from_id_salt("group_pick")
                            .selected_text("wählen")
                            .width(70.0)
                            .show_ui(ui, |ui| {
                                for g in &existing_groups {
                                    if ui.selectable_label(edit.group == *g, g).clicked() {
                                        edit.group = g.clone();
                                    }
                                }
                            });
                    }
                });

                ui.add_space(6.0);
                let valid = !edit.description.trim().is_empty() && !edit.content.trim().is_empty();
                ui.horizontal_wrapped(|ui| {
                    let label = if edit.index.is_some() {
                        "Speichern"
                    } else {
                        "Hinzufügen"
                    };
                    if ui.add_enabled(valid, egui::Button::new(label)).clicked() {
                        save = true;
                    }
                    if ui.button("Abbrechen").clicked() {
                        keep_open = false;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            keep_open = false;
        }

        if save {
            let snippet = Snippet {
                description: edit.description.trim().to_string(),
                content: edit.content.clone(),
                group: edit.group.trim().to_string(),
            };
            match edit.index {
                Some(i) => {
                    if let Some(s) = self.cfg.snippets.get_mut(i) {
                        *s = snippet;
                    }
                }
                None => self.cfg.snippets.push(snippet),
            }
            self.copied = None;
            self.status = None;
            self.save();
            keep_open = false;
        }

        if keep_open {
            self.editing = Some(edit);
        }
    }

    // ----- Schließen: Tray oder Beenden -------------------------------------

    fn hide_to_tray(&mut self) {
        hide_window();
        self.confirm_delete_all = false;
        self.show_close_dialog = false;
    }

    /// Abfrage, ob das Programm beendet oder in den Tray minimiert werden soll.
    fn ui_close_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_close_dialog {
            return;
        }

        let width = (ctx.screen_rect().width() - 40.0).clamp(140.0, 300.0);

        // Some(true) = in den Tray, Some(false) = beenden
        let mut choice: Option<bool> = None;
        let mut cancel = false;

        egui::Window::new("Programm schließen")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.label(
                    "Soll der Snippet-Manager beendet werden oder im Hintergrund \
                     (System Tray) weiterlaufen?",
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.remember_close_choice, "Auswahl merken");
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("In den Tray").clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Beenden").clicked() {
                        choice = Some(false);
                    }
                    if ui.button("Abbrechen").clicked() {
                        cancel = true;
                    }
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }

        if let Some(to_tray) = choice {
            if self.remember_close_choice {
                self.cfg.close_behavior = if to_tray {
                    CloseBehavior::MinimizeToTray
                } else {
                    CloseBehavior::Quit
                };
                self.save();
            }
            self.show_close_dialog = false;
            if to_tray {
                self.hide_to_tray();
            } else {
                quit_now();
            }
        } else if cancel {
            self.show_close_dialog = false;
        }
    }

    // ----- Tab: Uhr & Timer -------------------------------------------------

    /// Erkennt einen abgelaufenen Countdown und plant das nächste Neuzeichnen.
    /// Läuft in jedem Frame, unabhängig vom sichtbaren Tab.
    fn tick_timers(&mut self, ctx: &egui::Context) {
        if let CountdownState::Running(end) = self.countdown.state {
            let now = Instant::now();
            if now >= end {
                self.countdown.state = CountdownState::Done;
                // Blinkt in der Taskleiste, falls das Fenster nicht im Vordergrund ist.
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            } else {
                ctx.request_repaint_after((end - now).min(Duration::from_millis(250)));
            }
        }

        if self.tab == Tab::Clock {
            let interval = if self.stopwatch.running_since.is_some() {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(200)
            };
            ctx.request_repaint_after(interval);
        }
    }

    fn ui_clock(&mut self, ui: &mut egui::Ui) {
        let now = Local::now();
        let dark = ui.visuals().dark_mode;
        let warn_color = if dark {
            egui::Color32::from_rgb(255, 110, 110)
        } else {
            egui::Color32::from_rgb(190, 30, 30)
        };

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // --- Aktuelle Uhrzeit ---
                let size = (ui.available_width() / 6.0).clamp(18.0, 40.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(now.format("%H:%M:%S").to_string())
                            .size(size)
                            .monospace()
                            .strong(),
                    );
                    ui.label(format!(
                        "{}, {}",
                        weekday_de(now.weekday()),
                        now.format("%d.%m.%Y")
                    ));
                });

                ui.add_space(6.0);
                ui.separator();

                // --- Countdown ---
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Countdown").heading());
                let state = self.countdown.state;
                match state {
                    CountdownState::Idle => {
                        ui.horizontal_wrapped(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.countdown.hours)
                                    .range(0..=99)
                                    .suffix(" h"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut self.countdown.minutes)
                                    .range(0..=59)
                                    .suffix(" min"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut self.countdown.seconds)
                                    .range(0..=59)
                                    .suffix(" s"),
                            );
                        });
                        let total = self.countdown.configured();
                        if ui
                            .add_enabled(!total.is_zero(), egui::Button::new("Start"))
                            .clicked()
                        {
                            self.countdown.state = CountdownState::Running(Instant::now() + total);
                        }
                    }
                    CountdownState::Running(end) => {
                        let remaining = end.saturating_duration_since(Instant::now());
                        ui.label(
                            egui::RichText::new(fmt_hms(ceil_secs(remaining)))
                                .size(size)
                                .monospace()
                                .strong(),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Pause").clicked() {
                                self.countdown.state = CountdownState::Paused(remaining);
                            }
                            if ui.button("Zurücksetzen").clicked() {
                                self.countdown.state = CountdownState::Idle;
                            }
                        });
                    }
                    CountdownState::Paused(remaining) => {
                        ui.label(
                            egui::RichText::new(fmt_hms(ceil_secs(remaining)))
                                .size(size)
                                .monospace()
                                .weak(),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Weiter").clicked() {
                                self.countdown.state =
                                    CountdownState::Running(Instant::now() + remaining);
                            }
                            if ui.button("Zurücksetzen").clicked() {
                                self.countdown.state = CountdownState::Idle;
                            }
                        });
                    }
                    CountdownState::Done => {
                        ui.label(
                            egui::RichText::new("Zeit abgelaufen!")
                                .color(warn_color)
                                .strong()
                                .size(size * 0.6),
                        );
                        if ui.button("OK").clicked() {
                            self.countdown.state = CountdownState::Idle;
                        }
                    }
                }

                ui.add_space(6.0);
                ui.separator();

                // --- Stoppuhr ---
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Stoppuhr").heading());
                let elapsed = self.stopwatch.elapsed();
                let tenths = elapsed.subsec_millis() / 100;
                ui.label(
                    egui::RichText::new(format!("{}.{}", fmt_hms(elapsed.as_secs()), tenths))
                        .size(size * 0.8)
                        .monospace()
                        .strong(),
                );
                ui.horizontal_wrapped(|ui| {
                    if self.stopwatch.running_since.is_some() {
                        if ui.button("Stopp").clicked() {
                            self.stopwatch.accumulated = self.stopwatch.elapsed();
                            self.stopwatch.running_since = None;
                        }
                    } else if ui.button("Start").clicked() {
                        self.stopwatch.running_since = Some(Instant::now());
                    }
                    let can_reset = !self.stopwatch.elapsed().is_zero();
                    if ui
                        .add_enabled(can_reset, egui::Button::new("Zurücksetzen"))
                        .clicked()
                    {
                        self.stopwatch = Stopwatch::default();
                    }
                });
            });
    }

    // ----- Tab: Taschenrechner ----------------------------------------------

    fn handle_calc_keys(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    for c in text.chars() {
                        self.calc.press_char(c);
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat,
                    ..
                } => match key {
                    egui::Key::Enter if !repeat => self.calc.press(CalcKey::Equals),
                    egui::Key::Backspace => self.calc.press(CalcKey::Back),
                    egui::Key::Escape | egui::Key::Delete => self.calc.press(CalcKey::Clear),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    fn ui_calculator(&mut self, ui: &mut egui::Ui) {
        // Anzeige-Texte vorbereiten.
        let small = if let Some(err) = self.calc.error {
            err.message().to_string()
        } else if self.calc.after_eval {
            display_expr(&self.calc.history)
        } else if let Some(p) = self.calc.preview() {
            format!("= {p}")
        } else {
            String::new()
        };
        let big = if self.calc.error.is_some() {
            "Fehler".to_string()
        } else if self.calc.expr.is_empty() {
            "0".to_string()
        } else {
            display_expr(&self.calc.expr)
        };

        let accent = ui.visuals().selection.bg_fill;
        let func_fill = ui.visuals().window_fill;
        let mut pressed: Option<CalcKey> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let avail = ui.available_width();
                let sp = 5.0;
                let inner = (avail - 18.0).max(10.0);

                // --- Anzeige: Ergebnis-/Verlaufszeile + Eingabe ---
                egui::Frame::none()
                    .fill(ui.visuals().extreme_bg_color)
                    .stroke(ui.visuals().widgets.inactive.bg_stroke)
                    .rounding(4.0)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                    .show(ui, |ui| {
                        ui.set_width(inner);
                        ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                            let small_line = if small.is_empty() {
                                " "
                            } else {
                                small.as_str()
                            };
                            ui.label(egui::RichText::new(small_line).small().weak());

                            // Schriftgröße an Länge/Breite anpassen, sehr lange
                            // Ausdrücke zeigen nur das Ende.
                            let len = big.chars().count().max(1);
                            let mut size = (avail / 9.0).clamp(18.0, 30.0);
                            if len as f32 * 0.62 * size > inner {
                                size = (inner / (0.62 * len as f32)).max(13.0);
                            }
                            let max_chars = ((inner / (0.62 * size)).floor() as usize).max(4);
                            let shown = if len > max_chars {
                                let tail: String =
                                    big.chars().skip(len - (max_chars - 1)).collect();
                                format!("…{tail}")
                            } else {
                                big.clone()
                            };
                            ui.label(egui::RichText::new(shown).size(size).monospace().strong());
                        });
                    });
                ui.add_space(sp);

                // --- Tastenfeld (5 Spalten) ---
                let cw = ((avail - 4.0 * sp) / 5.0).clamp(28.0, 64.0);
                let ch = (cw * 0.55).clamp(26.0, 36.0);
                let size = egui::vec2(cw, ch);

                egui::Grid::new("calc_grid")
                    .spacing([sp, sp])
                    .show(ui, |ui| {
                        for row in CALC_ROWS {
                            for key in row {
                                if let Some(k) =
                                    calc_button(ui, key, &self.calc, size, func_fill, accent)
                                {
                                    pressed = Some(k);
                                }
                            }
                            ui.end_row();
                        }
                    });

                ui.add_space(6.0);
                ui.weak(
                    "Tastatur: Ziffern, + - * / ^ ( ) ! %, Enter = Ergebnis, \
                     Rücktaste, Esc = C",
                );
            });

        if let Some(key) = pressed {
            self.calc.press(key);
        }
    }

    // ----- Tab: Einstellungen -----------------------------------------------

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let delete_color = if ui.visuals().dark_mode {
            egui::Color32::from_rgb(255, 110, 110)
        } else {
            egui::Color32::from_rgb(190, 30, 30)
        };

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("Darstellung");
                let mut dark = self.cfg.dark_mode;
                if ui.checkbox(&mut dark, "Dark Mode").changed() {
                    self.cfg.dark_mode = dark;
                    apply_theme(&ctx, &self.cfg);
                    self.save();
                }

                // Eigene Farben für den gerade aktiven Modus.
                ui.add_space(4.0);
                ui.label(if dark {
                    "Farben (Dark Mode)"
                } else {
                    "Farben (Light Mode)"
                });
                let mut colors_changed = false;
                {
                    let pal = if dark {
                        &mut self.cfg.palette_dark
                    } else {
                        &mut self.cfg.palette_light
                    };
                    egui::Grid::new("palette_grid")
                        .num_columns(2)
                        .spacing([10.0, 4.0])
                        .show(ui, |ui| {
                            for (label, col) in [
                                ("Hintergrund", &mut pal.background),
                                ("Buttons", &mut pal.surface),
                                ("Text", &mut pal.text),
                                ("Rahmen", &mut pal.border),
                                ("Akzent", &mut pal.accent),
                            ] {
                                ui.label(label);
                                colors_changed |= egui::color_edit_button_srgb(ui, col).changed();
                                ui.end_row();
                            }
                        });
                    if ui.button("Farben zurücksetzen").clicked() {
                        *pal = if dark { Palette::dark() } else { Palette::light() };
                        colors_changed = true;
                    }
                }
                if colors_changed {
                    apply_theme(&ctx, &self.cfg);
                    // Gespeichert wird erst beim Loslassen der Maus (nicht bei jedem Frame).
                    self.palette_dirty = true;
                }

                ui.add_space(10.0);
                ui.heading("Fenster");
                let mut on_top = self.cfg.always_on_top;
                if ui.checkbox(&mut on_top, "Immer im Vordergrund").changed() {
                    self.cfg.always_on_top = on_top;
                    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if on_top {
                        egui::WindowLevel::AlwaysOnTop
                    } else {
                        egui::WindowLevel::Normal
                    }));
                    self.save();
                }

                ui.add_space(6.0);
                ui.label("Beim Schließen des Fensters (X):");
                let mut close_behavior = self.cfg.close_behavior;
                let mut close_changed = false;
                close_changed |= ui
                    .radio_value(&mut close_behavior, CloseBehavior::Ask, "Jedes Mal fragen")
                    .changed();
                close_changed |= ui
                    .radio_value(
                        &mut close_behavior,
                        CloseBehavior::MinimizeToTray,
                        "In den Tray minimieren",
                    )
                    .changed();
                close_changed |= ui
                    .radio_value(&mut close_behavior, CloseBehavior::Quit, "Programm beenden")
                    .changed();
                if close_changed {
                    self.cfg.close_behavior = close_behavior;
                    self.save();
                }

                ui.add_space(10.0);
                ui.heading("System");
                let mut autostart = self.cfg.autostart;
                if ui.checkbox(&mut autostart, "Mit Windows starten").changed() {
                    self.set_autostart(autostart);
                }

                let mut background = self.cfg.start_in_background;
                if ui
                    .checkbox(&mut background, "Im Hintergrund starten (nur Tray)")
                    .changed()
                {
                    self.cfg.start_in_background = background;
                    self.save();
                }
                ui.weak("Gilt für den Start über den Windows-Autostart. Manuell gestartet öffnet sich das Fenster immer.");

                ui.add_space(10.0);
                ui.heading("Daten");
                ui.label(format!("{} Snippet(s) gespeichert", self.cfg.snippets.len()));
                ui.add_space(4.0);

                if self.confirm_delete_all {
                    ui.colored_label(delete_color, "Wirklich ALLE Snippets unwiderruflich löschen?");
                    ui.horizontal_wrapped(|ui| {
                        let yes = egui::Button::new(
                            egui::RichText::new("Ja, alles löschen")
                                .color(egui::Color32::WHITE)
                                .strong(),
                        )
                        .fill(egui::Color32::from_rgb(180, 40, 40));
                        if ui.add(yes).clicked() {
                            self.cfg.snippets.clear();
                            self.copied = None;
                            self.editing = None;
                            self.confirm_delete_all = false;
                            self.save();
                        }
                        if ui.button("Abbrechen").clicked() {
                            self.confirm_delete_all = false;
                        }
                    });
                } else if ui
                    .add_enabled(
                        !self.cfg.snippets.is_empty(),
                        egui::Button::new("Alle Snippets löschen"),
                    )
                    .clicked()
                {
                    self.confirm_delete_all = true;
                }
            });
    }
}

/// Einzeilige, auf `max_chars` gekürzte Darstellung eines Textes.
fn shorten(text: &str, max_chars: usize) -> String {
    let single_line = text.trim().replace(['\r', '\n'], " ");
    if single_line.chars().count() > max_chars {
        let cut: String = single_line.chars().take(max_chars).collect();
        format!("{cut}…")
    } else {
        single_line
    }
}

impl eframe::App for SnippetApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Fenster-X: je nach Einstellung nachfragen, in den Tray verstecken oder beenden.
        // Ohne Tray-Icon wird immer normal beendet (sonst wäre die App unerreichbar).
        if ctx.input(|i| i.viewport().close_requested()) && self.has_tray {
            match self.cfg.close_behavior {
                CloseBehavior::MinimizeToTray => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.hide_to_tray();
                }
                // Nichts abfangen: das Fenster schließt, das Programm beendet sich.
                CloseBehavior::Quit => {}
                CloseBehavior::Ask => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.show_close_dialog = true;
                }
            }
        }

        // "Kopiert!"-Feedback nach 1 Sekunde zurücksetzen.
        if let Some((_, t)) = self.copied {
            let elapsed = t.elapsed();
            if elapsed >= FEEDBACK_DURATION {
                self.copied = None;
            } else {
                ctx.request_repaint_after(FEEDBACK_DURATION - elapsed);
            }
        }

        self.tick_timers(ctx);
        update_window_region(ctx, &mut self.region_size);
        handle_resize_edges(ctx);

        // Geänderte Farben erst speichern, wenn die Maus losgelassen wurde.
        if self.palette_dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.palette_dirty = false;
            self.save();
        }

        // Tastatureingabe für den Taschenrechner (nur wenn kein Textfeld/Dialog aktiv ist).
        if self.tab == Tab::Calculator
            && self.editing.is_none()
            && !self.show_close_dialog
            && !ctx.wants_keyboard_input()
        {
            self.handle_calc_keys(ctx);
        }

        self.ui_titlebar(ctx);

        let panel_fill = ctx.style().visuals.panel_fill;
        let tabs_frame = egui::Frame::none()
            .fill(panel_fill)
            .inner_margin(egui::Margin::symmetric(8.0, 2.0));
        egui::TopBottomPanel::top("tabs")
            .frame(tabs_frame)
            .show(ctx, |ui| {
                ui.add_space(3.0);
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Snippets, "Snippets");
                    ui.selectable_value(&mut self.tab, Tab::Clock, "Uhr & Timer");
                    ui.selectable_value(&mut self.tab, Tab::Calculator, "Taschenrechner");
                    ui.selectable_value(&mut self.tab, Tab::Settings, "Einstellungen");
                });
                ui.add_space(3.0);
            });

        let central_frame = egui::Frame::none()
            .fill(panel_fill)
            .rounding(egui::Rounding {
                nw: 0.0,
                ne: 0.0,
                sw: WINDOW_ROUNDING,
                se: WINDOW_ROUNDING,
            })
            .inner_margin(egui::Margin::same(8.0));
        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ctx, |ui| {
                match self.tab {
                    Tab::Snippets => self.ui_snippets(ui),
                    Tab::Clock => self.ui_clock(ui),
                    Tab::Calculator => self.ui_calculator(ui),
                    Tab::Settings => self.ui_settings(ui),
                }

                if let Some(msg) = &self.status {
                    ui.add_space(6.0);
                    let color = if ui.visuals().dark_mode {
                        egui::Color32::from_rgb(255, 120, 120)
                    } else {
                        egui::Color32::from_rgb(190, 30, 30)
                    };
                    ui.colored_label(color, msg);
                }
            });

        // Modale Dialoge (liegen über dem Hauptfenster).
        self.ui_edit_window(ctx);
        self.ui_close_dialog(ctx);

        // Dünner Rahmen um das randlose Fenster, damit es sich vom Hintergrund abhebt.
        let border = ctx.style().visuals.widgets.inactive.bg_stroke;
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("window_border"),
        ))
        .rect_stroke(ctx.screen_rect().shrink(0.5), WINDOW_ROUNDING, border);
    }

    /// Wird beim normalen Beenden aufgerufen: Tray-Icon sauber entfernen.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        remove_tray();
    }
}

fn main() -> eframe::Result<()> {
    let path = config_path();
    migrate_legacy_config(&path);
    let cfg = load_config(&path);

    // Unsichtbar starten nur, wenn Windows uns per Autostart startet
    // UND die Einstellung "Im Hintergrund starten" aktiv ist.
    let started_by_autostart = std::env::args().any(|a| a == AUTOSTART_ARG);
    let start_hidden = cfg.start_in_background && started_by_autostart;

    let (rgba, width, height) = icon_rgba();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([440.0, 560.0])
        // Sehr kleine Mindestgröße: das Fenster lässt sich stark verkleinern.
        .with_min_inner_size([200.0, 200.0])
        // Eigene Titelleiste statt der Windows-Standardleiste
        // (in eframe 0.29 heißt `decorated: false` → `with_decorations(false)`).
        .with_decorations(false)
        .with_icon(egui::IconData {
            rgba,
            width,
            height,
        })
        .with_visible(!start_hidden);
    if cfg.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Snippet-Manager",
        options,
        Box::new(move |cc| Ok(Box::new(SnippetApp::new(cc, cfg, path)))),
    )
}
