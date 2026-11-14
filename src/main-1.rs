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
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW,
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

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    snippets: Vec<Snippet>,
    dark_mode: bool,
    always_on_top: bool,
    autostart: bool,
    start_in_background: bool,
    close_behavior: CloseBehavior,
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

fn contrast_visuals(dark: bool) -> egui::Visuals {
    use egui::{Color32, Stroke};

    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    // (Panel, Fenster, Eingabefeld, Text, kräftiger Text, Rand, schwacher Rand,
    //  Button normal, Button hover, Button aktiv, Akzent)
    let (
        panel,
        window,
        input,
        text,
        strong,
        border,
        soft_border,
        btn,
        btn_hover,
        btn_active,
        accent,
    ) = if dark {
        (
            Color32::from_rgb(0x16, 0x18, 0x1D),
            Color32::from_rgb(0x1F, 0x22, 0x29),
            Color32::from_rgb(0x0C, 0x0E, 0x11),
            Color32::from_rgb(0xE8, 0xEA, 0xEE),
            Color32::WHITE,
            Color32::from_rgb(0x8A, 0x93, 0xA6),
            Color32::from_rgb(0x4A, 0x51, 0x60),
            Color32::from_rgb(0x2A, 0x2F, 0x3A),
            Color32::from_rgb(0x38, 0x3F, 0x4E),
            Color32::from_rgb(0x48, 0x52, 0x66),
            Color32::from_rgb(0x4C, 0x9A, 0xFF),
        )
    } else {
        (
            Color32::from_rgb(0xF4, 0xF5, 0xF7),
            Color32::from_rgb(0xFF, 0xFF, 0xFF),
            Color32::WHITE,
            Color32::from_rgb(0x14, 0x17, 0x1E),
            Color32::BLACK,
            Color32::from_rgb(0x52, 0x5B, 0x6B),
            Color32::from_rgb(0xA8, 0xB0, 0xBF),
            Color32::from_rgb(0xE4, 0xE7, 0xED),
            Color32::from_rgb(0xD2, 0xD8, 0xE3),
            Color32::from_rgb(0xBC, 0xC5, 0xD6),
            Color32::from_rgb(0x1A, 0x58, 0xCC),
        )
    };

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

/// Setzt beide Farbschemata (mit besserem Kontrast) und wählt das aktive.
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.set_visuals_of(egui::Theme::Dark, contrast_visuals(true));
    ctx.set_visuals_of(egui::Theme::Light, contrast_visuals(false));
    ctx.set_theme(if dark {
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
// Taschenrechner
//
// Arbeitet wie ein einfacher Standard-Taschenrechner: Jede Rechenart wird sofort
// ausgeführt, sobald die nächste Operation oder "=" gedrückt wird (2 + 3 × 4 = 20).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum CalcKey {
    Digit(char),
    Dot,
    Op(char),
    Equals,
    Clear,
    Back,
    Negate,
}

impl CalcKey {
    fn label(self) -> String {
        match self {
            CalcKey::Digit(c) => c.to_string(),
            CalcKey::Dot => ",".to_string(),
            CalcKey::Op(c) => op_symbol(c).to_string(),
            CalcKey::Equals => "=".to_string(),
            CalcKey::Clear => "C".to_string(),
            CalcKey::Back => "Entf".to_string(),
            CalcKey::Negate => "±".to_string(),
        }
    }
}

/// Obere vier Reihen des Tastenfelds (die letzte Reihe "0 , =" wird gesondert gebaut).
const CALC_ROWS: [[CalcKey; 4]; 4] = [
    [
        CalcKey::Clear,
        CalcKey::Back,
        CalcKey::Negate,
        CalcKey::Op('/'),
    ],
    [
        CalcKey::Digit('7'),
        CalcKey::Digit('8'),
        CalcKey::Digit('9'),
        CalcKey::Op('*'),
    ],
    [
        CalcKey::Digit('4'),
        CalcKey::Digit('5'),
        CalcKey::Digit('6'),
        CalcKey::Op('-'),
    ],
    [
        CalcKey::Digit('1'),
        CalcKey::Digit('2'),
        CalcKey::Digit('3'),
        CalcKey::Op('+'),
    ],
];

const MAX_DIGITS: usize = 15;

fn op_symbol(op: char) -> char {
    match op {
        '*' => '×',
        '/' => '÷',
        other => other,
    }
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

struct Calculator {
    /// Aktuelle Eingabe bzw. letztes Ergebnis (mit Komma als Dezimaltrenner).
    display: String,
    /// Linker Operand, solange eine Rechenart aussteht.
    acc: Option<f64>,
    op: Option<char>,
    /// `true`: Die nächste Ziffer beginnt eine neue Zahl.
    fresh: bool,
    error: bool,
}

impl Calculator {
    fn new() -> Self {
        Self {
            display: "0".to_string(),
            acc: None,
            op: None,
            fresh: false,
            error: false,
        }
    }

    fn value(&self) -> f64 {
        self.display.replace(',', ".").parse().unwrap_or(0.0)
    }

    fn clear(&mut self) {
        *self = Self::new();
    }

    fn fail(&mut self) {
        self.display = "Fehler".to_string();
        self.acc = None;
        self.op = None;
        self.fresh = true;
        self.error = true;
    }

    fn digit(&mut self, d: char) {
        if self.error {
            self.clear();
        }
        if self.fresh {
            self.display = "0".to_string();
            self.fresh = false;
        }
        let digits = self.display.chars().filter(|c| c.is_ascii_digit()).count();
        if digits >= MAX_DIGITS {
            return;
        }
        match self.display.as_str() {
            "0" => self.display = d.to_string(),
            "-0" => self.display = format!("-{d}"),
            _ => self.display.push(d),
        }
    }

    fn dot(&mut self) {
        if self.error {
            self.clear();
        }
        if self.fresh {
            self.display = "0".to_string();
            self.fresh = false;
        }
        if !self.display.contains(',') {
            self.display.push(',');
        }
    }

    fn backspace(&mut self) {
        if self.error {
            self.clear();
            return;
        }
        if self.fresh {
            return;
        }
        self.display.pop();
        if self.display.is_empty() || self.display == "-" {
            self.display = "0".to_string();
        }
    }

    fn negate(&mut self) {
        if self.error || self.display == "0" {
            return;
        }
        if self.display.starts_with('-') {
            self.display.remove(0);
        } else {
            self.display.insert(0, '-');
        }
    }

    /// Rechnet "acc op anzeige" aus (falls eine Rechenart aussteht).
    fn evaluate(&mut self) {
        if let (Some(a), Some(op)) = (self.acc, self.op) {
            let b = self.value();
            let result = match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => {
                    if b == 0.0 {
                        self.fail();
                        return;
                    }
                    a / b
                }
                _ => b,
            };
            if !result.is_finite() {
                self.fail();
                return;
            }
            self.display = fmt_number(result);
            self.acc = None;
            self.op = None;
        }
        self.fresh = true;
    }

    fn operator(&mut self, op: char) {
        if self.error {
            return;
        }
        // Bei Kettenrechnung (2 + 3 +) wird sofort zwischengerechnet.
        // Wird nur die Rechenart gewechselt (fresh), bleibt der Operand erhalten.
        if self.op.is_some() && !self.fresh {
            self.evaluate();
            if self.error {
                return;
            }
        }
        self.acc = Some(self.value());
        self.op = Some(op);
        self.fresh = true;
    }

    fn press(&mut self, key: CalcKey) {
        match key {
            CalcKey::Digit(d) => self.digit(d),
            CalcKey::Dot => self.dot(),
            CalcKey::Op(o) => self.operator(o),
            CalcKey::Equals => self.evaluate(),
            CalcKey::Clear => self.clear(),
            CalcKey::Back => self.backspace(),
            CalcKey::Negate => self.negate(),
        }
    }

    /// Eingabe über die Tastatur (Zeichen aus `Event::Text`).
    fn press_char(&mut self, c: char) {
        match c {
            '0'..='9' => self.digit(c),
            ',' | '.' => self.dot(),
            '+' | '-' | '*' | '/' => self.operator(c),
            '=' => self.evaluate(),
            _ => {}
        }
    }
}

/// Eine Taste des Rechners zeichnen. Gibt `true` zurück, wenn sie geklickt wurde.
fn calc_button(
    ui: &mut egui::Ui,
    key: CalcKey,
    size: egui::Vec2,
    op_fill: egui::Color32,
    accent: egui::Color32,
) -> bool {
    let text = egui::RichText::new(key.label()).size(16.0);
    let button = match key {
        CalcKey::Equals => {
            egui::Button::new(text.color(egui::Color32::WHITE).strong()).fill(accent)
        }
        CalcKey::Digit(_) | CalcKey::Dot => egui::Button::new(text),
        _ => egui::Button::new(text).fill(op_fill),
    };
    ui.add_sized(size, button).clicked()
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
    /// `true`, wenn Tray-Icon UND Fenster-Handle verfügbar sind. Nur dann darf
    /// das Fenster versteckt werden (sonst wäre die App nicht mehr erreichbar).
    has_tray: bool,
}

impl SnippetApp {
    fn new(cc: &eframe::CreationContext<'_>, cfg: Config, path: PathBuf) -> Self {
        apply_theme(&cc.egui_ctx, cfg.dark_mode);

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
            .inner_margin(egui::Margin::symmetric(6.0, 0.0));

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
        let expr = match (self.calc.acc, self.calc.op) {
            (Some(a), Some(op)) => format!("{} {}", fmt_number(a), op_symbol(op)),
            _ => String::new(),
        };
        let display = self.calc.display.clone();
        let accent = ui.visuals().selection.bg_fill;
        let op_fill = ui.visuals().widgets.active.bg_fill;
        let mut pressed: Option<CalcKey> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let avail = ui.available_width();
                let sp = 6.0;

                // --- Anzeige: Rechenzeile + aktuelle Eingabe/Ergebnis ---
                egui::Frame::none()
                    .fill(ui.visuals().extreme_bg_color)
                    .stroke(ui.visuals().widgets.inactive.bg_stroke)
                    .rounding(4.0)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                    .show(ui, |ui| {
                        ui.set_width((avail - 18.0).max(10.0));
                        ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                            let line = if expr.is_empty() { " " } else { expr.as_str() };
                            ui.label(egui::RichText::new(line).small().weak());
                            let len = display.chars().count().max(1) as f32;
                            let size = (avail / 9.0)
                                .clamp(18.0, 34.0)
                                .min((avail - 24.0) / (0.62 * len))
                                .max(11.0);
                            ui.label(
                                egui::RichText::new(&display)
                                    .size(size)
                                    .monospace()
                                    .strong(),
                            );
                        });
                    });
                ui.add_space(sp);

                // --- Tastenfeld ---
                let cw = ((avail - 3.0 * sp) / 4.0).clamp(28.0, 80.0);
                let ch = (cw * 0.7).clamp(30.0, 46.0);
                let size = egui::vec2(cw, ch);

                egui::Grid::new("calc_grid")
                    .spacing([sp, sp])
                    .show(ui, |ui| {
                        for row in CALC_ROWS {
                            for key in row {
                                if calc_button(ui, key, size, op_fill, accent) {
                                    pressed = Some(key);
                                }
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(sp);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = sp;
                    for key in [CalcKey::Digit('0'), CalcKey::Dot] {
                        if calc_button(ui, key, size, op_fill, accent) {
                            pressed = Some(key);
                        }
                    }
                    let wide = egui::vec2(2.0 * cw + sp, ch);
                    if calc_button(ui, CalcKey::Equals, wide, op_fill, accent) {
                        pressed = Some(CalcKey::Equals);
                    }
                });

                ui.add_space(8.0);
                ui.weak("Tastatur: Ziffern, + - * /, Enter = Ergebnis, Rücktaste, Esc = C");
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
                    apply_theme(&ctx, dark);
                    self.save();
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

        // Tastatureingabe für den Taschenrechner (nur wenn kein Textfeld/Dialog aktiv ist).
        if self.tab == Tab::Calculator
            && self.editing.is_none()
            && !self.show_close_dialog
            && !ctx.wants_keyboard_input()
        {
            self.handle_calc_keys(ctx);
        }

        self.ui_titlebar(ctx);

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Snippets, "Snippets");
                ui.selectable_value(&mut self.tab, Tab::Clock, "Uhr & Timer");
                ui.selectable_value(&mut self.tab, Tab::Calculator, "Taschenrechner");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Einstellungen");
            });
            ui.add_space(3.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
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
        .rect_stroke(ctx.screen_rect().shrink(0.5), 0.0, border);
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
