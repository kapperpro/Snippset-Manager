// Im Release-Build kein Konsolenfenster unter Windows anzeigen.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use arboard::Clipboard;
use chrono::{Datelike, Local, Weekday};
use eframe::egui::{self, Color32, Stroke};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW,
};

const FEEDBACK_DURATION: Duration = Duration::from_secs(1);
const MAX_LABEL_CHARS: usize = 80;
const TITLEBAR_HEIGHT: f32 = 24.0;
/// Wird dem Autostart-Eintrag als Argument mitgegeben, damit die App weiß,
/// dass sie von Windows (und nicht manuell vom Nutzer) gestartet wurde.
const AUTOSTART_ARG: &str = "--autostart";

// ---------------------------------------------------------------------------
// Datenmodell
// ---------------------------------------------------------------------------

/// Ein Snippet: sichtbare Beschreibung + versteckter Inhalt + optionale Gruppe.
///
/// Die Reihenfolge in `Config::snippets` ist die manuelle Reihenfolge.
/// Das Einlesen akzeptiert auch ältere Formate (reiner String bzw. ohne Gruppe),
/// damit eine vorhandene `config.json` weiter funktioniert.
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
    Legacy(String),
    Current {
        description: String,
        content: String,
        #[serde(default)]
        group: String,
    },
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
                group: group.trim().to_string(),
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
// Autostart (Windows-Registry: HKCU\...\Run)
//
// Der Registry-Zugriff ist ein Cargo-Feature (`autostart`, standardmäßig an).
// Ohne das Feature enthält die .exe keinerlei Registry-Code – Autostart-
// Einträge sind eines der Verhaltensmuster, auf die Virenscanner besonders
// empfindlich reagieren.
// ---------------------------------------------------------------------------

#[cfg(feature = "autostart")]
mod autostart {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const NAME: &str = "SnippetManager";
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn is_enabled() -> bool {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .and_then(|key| key.get_value::<String, _>(NAME))
            .is_ok()
    }

    /// Legt den Autostart-Eintrag an bzw. entfernt ihn. Der Pfad wird in
    /// Anführungszeichen geschrieben, damit auch Pfade mit Leerzeichen
    /// (z. B. `C:\Program Files\...`) zuverlässig funktionieren.
    pub fn set(on: bool) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(|e| e.to_string())?;

        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let command = format!("\"{}\" {}", exe.display(), super::AUTOSTART_ARG);
            key.set_value(NAME, &command).map_err(|e| e.to_string())
        } else {
            match key.delete_value(NAME) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

#[cfg(not(feature = "autostart"))]
mod autostart {
    pub fn is_enabled() -> bool {
        false
    }
    pub fn set(_on: bool) -> Result<(), String> {
        Ok(())
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
// Optik: kontrastreiche Visuals für Dark- und Light-Mode
// ---------------------------------------------------------------------------

struct Palette {
    text: Color32,
    text_strong: Color32,
    text_weak: Color32,
    panel: Color32,
    window: Color32,
    field: Color32,
    idle: Color32,
    hover: Color32,
    active: Color32,
    border: Color32,
    border_hover: Color32,
    border_active: Color32,
    separator: Color32,
    selection: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color32::from_rgb(232, 234, 240),
            text_strong: Color32::from_rgb(255, 255, 255),
            text_weak: Color32::from_rgb(165, 172, 186),
            panel: Color32::from_rgb(26, 28, 34),
            window: Color32::from_rgb(34, 37, 44),
            field: Color32::from_rgb(16, 17, 21),
            idle: Color32::from_rgb(50, 54, 64),
            hover: Color32::from_rgb(66, 72, 86),
            active: Color32::from_rgb(46, 98, 176),
            border: Color32::from_rgb(122, 130, 148),
            border_hover: Color32::from_rgb(176, 186, 206),
            border_active: Color32::from_rgb(130, 180, 245),
            separator: Color32::from_rgb(84, 90, 104),
            selection: Color32::from_rgb(40, 100, 190),
        }
    } else {
        Palette {
            text: Color32::from_rgb(24, 26, 32),
            text_strong: Color32::from_rgb(0, 0, 0),
            text_weak: Color32::from_rgb(88, 94, 108),
            panel: Color32::from_rgb(246, 247, 250),
            window: Color32::from_rgb(255, 255, 255),
            field: Color32::from_rgb(255, 255, 255),
            idle: Color32::from_rgb(230, 233, 240),
            hover: Color32::from_rgb(212, 221, 238),
            active: Color32::from_rgb(170, 196, 236),
            border: Color32::from_rgb(110, 116, 132),
            border_hover: Color32::from_rgb(50, 80, 140),
            border_active: Color32::from_rgb(25, 55, 130),
            separator: Color32::from_rgb(150, 155, 170),
            selection: Color32::from_rgb(150, 190, 245),
        }
    }
}

fn make_visuals(dark: bool) -> egui::Visuals {
    let p = palette(dark);
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.panel_fill = p.panel;
    v.window_fill = p.window;
    v.extreme_bg_color = p.field; // Hintergrund der Eingabefelder
    v.weak_text_color = Some(p.text_weak);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.text_strong);

    let w = &mut v.widgets;

    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);

    // Buttons im Ruhezustand: klarer Rand + gut lesbare Schrift.
    w.inactive.bg_fill = p.idle;
    w.inactive.weak_bg_fill = p.idle;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);

    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::new(1.5, p.border_hover);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text_strong);

    w.active.bg_fill = p.active;
    w.active.weak_bg_fill = p.active;
    w.active.bg_stroke = Stroke::new(1.5, p.border_active);
    w.active.fg_stroke = Stroke::new(1.5, p.text_strong);

    w.open.bg_fill = p.hover;
    w.open.weak_bg_fill = p.hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border_hover);
    w.open.fg_stroke = Stroke::new(1.0, p.text_strong);

    v
}

fn error_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(255, 120, 120)
    } else {
        Color32::from_rgb(170, 25, 25)
    }
}

fn ok_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(110, 220, 140)
    } else {
        Color32::from_rgb(15, 120, 50)
    }
}

// ---------------------------------------------------------------------------
// Fenstersteuerung direkt über die Windows-API
//
// Ein verstecktes eframe-Fenster verarbeitet keine Frames mehr. Befehle wie
// `ViewportCommand::Visible(true)` kämen dort deshalb erst an, wenn ein anderes
// Ereignis die Event-Loop weckt. Darum zeigen/verstecken wir das Fenster direkt
// per Win32 – das funktioniert sofort, unabhängig von egui.
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
// Eigene Titelleiste
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum TitleBtn {
    Minimize,
    Close,
}

/// Sehr kleiner Titelleisten-Button; das Symbol wird selbst gezeichnet
/// (unabhängig von Schriftarten).
fn title_button(ui: &mut egui::Ui, kind: TitleBtn) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(28.0, TITLEBAR_HEIGHT), egui::Sense::click());
    let dark = ui.visuals().dark_mode;

    if resp.hovered() {
        let fill = match kind {
            TitleBtn::Close => Color32::from_rgb(200, 50, 50),
            TitleBtn::Minimize => {
                if dark {
                    Color32::from_gray(64)
                } else {
                    Color32::from_gray(205)
                }
            }
        };
        ui.painter().rect_filled(rect, 0.0, fill);
    }

    let fg = if resp.hovered() && kind == TitleBtn::Close {
        Color32::WHITE
    } else {
        ui.visuals().text_color()
    };
    let stroke = Stroke::new(1.4, fg);
    let c = rect.center();
    let painter = ui.painter();
    match kind {
        TitleBtn::Minimize => {
            painter.line_segment(
                [c + egui::vec2(-5.0, 3.0), c + egui::vec2(5.0, 3.0)],
                stroke,
            );
        }
        TitleBtn::Close => {
            painter.line_segment(
                [c + egui::vec2(-4.5, -4.5), c + egui::vec2(4.5, 4.5)],
                stroke,
            );
            painter.line_segment(
                [c + egui::vec2(-4.5, 4.5), c + egui::vec2(4.5, -4.5)],
                stroke,
            );
        }
    }
    resp
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Snippets,
    Clock,
    Settings,
}

/// Zustand des Bearbeiten-Dialogs.
struct EditState {
    index: usize,
    description: String,
    group: String,
    content: String,
    just_opened: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum CdState {
    Idle,
    Running { end: Instant },
    Paused { remaining: Duration },
    Finished,
}

/// Aktionen, die beim Zeichnen der Liste angefordert werden und erst danach
/// (ohne Borrow-Konflikte) ausgeführt werden.
#[derive(Default)]
struct RowActions {
    copy: Option<usize>,
    edit: Option<usize>,
    delete: Option<usize>,
    /// (Index, true = nach oben / false = nach unten)
    mv: Option<(usize, bool)>,
}

struct SnippetApp {
    cfg: Config,
    path: PathBuf,
    tab: Tab,

    // Snippets
    show_new: bool,
    focus_new: bool,
    new_description: String,
    new_group: String,
    new_content: String,
    reorder_mode: bool,
    editing: Option<EditState>,
    clipboard: Option<Clipboard>,
    copied: Option<(usize, Instant)>,

    // Uhr & Timer
    cd_h: u32,
    cd_m: u32,
    cd_s: u32,
    cd_state: CdState,
    sw_elapsed: Duration,
    sw_started: Option<Instant>,

    // Sonstiges
    confirm_delete_all: bool,
    show_close_dialog: bool,
    remember_close_choice: bool,
    status: Option<String>,
    /// `true`, wenn Tray-Icon UND Fenster-Handle verfügbar sind. Nur dann darf
    /// das Fenster versteckt werden (sonst wäre die App nicht mehr erreichbar).
    has_tray: bool,
}

impl SnippetApp {
    fn new(cc: &eframe::CreationContext<'_>, cfg: Config, path: PathBuf) -> Self {
        cc.egui_ctx
            .set_visuals_of(egui::Theme::Dark, make_visuals(true));
        cc.egui_ctx
            .set_visuals_of(egui::Theme::Light, make_visuals(false));
        cc.egui_ctx.set_theme(if cfg.dark_mode {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });

        let mut status = None;

        // Autostart-Eintrag mit dem gespeicherten Zustand abgleichen
        // (aktualisiert auch den Pfad, falls die .exe verschoben wurde).
        if cfg.autostart {
            if let Err(e) = autostart::set(true) {
                status = Some(format!("Autostart konnte nicht gesetzt werden: {e}"));
            }
        } else if autostart::is_enabled() {
            let _ = autostart::set(false);
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
            show_new: false,
            focus_new: false,
            new_description: String::new(),
            new_group: String::new(),
            new_content: String::new(),
            reorder_mode: false,
            editing: None,
            clipboard: Clipboard::new().ok(),
            copied: None,
            cd_h: 0,
            cd_m: 5,
            cd_s: 0,
            cd_state: CdState::Idle,
            sw_elapsed: Duration::ZERO,
            sw_started: None,
            confirm_delete_all: false,
            show_close_dialog: false,
            remember_close_choice: true,
            status,
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

    /// Benannte Gruppen in der Reihenfolge ihres ersten Auftretens.
    fn group_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for s in &self.cfg.snippets {
            let g = s.group.trim();
            if !g.is_empty() && !names.iter().any(|n| n == g) {
                names.push(g.to_string());
            }
        }
        names
    }

    /// Gibt `true` zurück, wenn das Snippet hinzugefügt wurde.
    fn add_snippet(&mut self) -> bool {
        let description = self.new_description.trim().to_string();
        if description.is_empty() || self.new_content.trim().is_empty() {
            self.status = Some("Bitte Beschreibung und Inhalt ausfüllen.".to_string());
            return false;
        }

        self.cfg.snippets.push(Snippet {
            description,
            content: std::mem::take(&mut self.new_content),
            group: self.new_group.trim().to_string(),
        });
        self.new_description.clear();
        self.new_group.clear();
        self.status = None;
        self.save();
        true
    }

    fn delete_snippet(&mut self, index: usize) {
        if index < self.cfg.snippets.len() {
            self.cfg.snippets.remove(index);
            self.copied = None;
            self.editing = None; // Indizes haben sich verschoben
            self.save();
        }
    }

    /// Verschiebt ein Snippet innerhalb seiner Gruppe um eine Position.
    fn move_snippet(&mut self, index: usize, up: bool) {
        let len = self.cfg.snippets.len();
        if index >= len {
            return;
        }
        let group = self.cfg.snippets[index].group.clone();
        let same_group = |j: &usize| self.cfg.snippets[*j].group == group;
        let neighbor = if up {
            (0..index).rev().find(same_group)
        } else {
            (index + 1..len).find(same_group)
        };
        if let Some(j) = neighbor {
            self.cfg.snippets.swap(index, j);
            self.copied = None;
            self.editing = None;
            self.save();
        }
    }

    fn start_editing(&mut self, index: usize) {
        if let Some(s) = self.cfg.snippets.get(index) {
            self.editing = Some(EditState {
                index,
                description: s.description.clone(),
                group: s.group.clone(),
                content: s.content.clone(),
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
        match autostart::set(on) {
            Ok(()) => {
                self.cfg.autostart = on;
                self.status = None;
                self.save();
            }
            Err(e) => self.status = Some(format!("Autostart-Änderung fehlgeschlagen: {e}")),
        }
    }

    // ----- Titelleiste ------------------------------------------------------

    fn ui_titlebar(&mut self, ctx: &egui::Context) {
        let dark = ctx.style().visuals.dark_mode;
        let bar_fill = if dark {
            Color32::from_rgb(12, 13, 16)
        } else {
            Color32::from_rgb(222, 226, 234)
        };

        egui::TopBottomPanel::top("titlebar")
            .exact_height(TITLEBAR_HEIGHT)
            .frame(egui::Frame::none().fill(bar_fill))
            .show(ctx, |ui| {
                let bar = ui.max_rect();

                // Die ganze Leiste ist Ziehfläche. Die Buttons werden danach
                // hinzugefügt und liegen dadurch oben auf.
                let drag = ui.interact(
                    bar,
                    egui::Id::new("titlebar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.painter().text(
                    bar.left_center() + egui::vec2(8.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    "Snippet-Manager",
                    egui::FontId::proportional(12.0),
                    ui.visuals().text_color(),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    if title_button(ui, TitleBtn::Close)
                        .on_hover_text("Schließen")
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if title_button(ui, TitleBtn::Minimize)
                        .on_hover_text("Minimieren")
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                });
            });
    }

    // ----- Tab: Snippets ----------------------------------------------------

    fn ui_snippets(&mut self, ui: &mut egui::Ui) {
        let groups = self.group_names();
        let content_id = egui::Id::new("new_snippet_content");

        // --- Kopfzeile: nur "Neu" und "Ordnen" ---
        ui.horizontal(|ui| {
            if ui.button("Neu").clicked() {
                self.show_new = !self.show_new;
                if self.show_new {
                    self.focus_new = true;
                }
            }
            ui.toggle_value(&mut self.reorder_mode, "Ordnen")
                .on_hover_text("Reihenfolge innerhalb einer Gruppe mit ↑ / ↓ ändern");
        });
        ui.add_space(4.0);

        let mut add = false;
        let mut cancel_new = false;
        let mut actions = RowActions::default();
        let reorder = self.reorder_mode;
        let copied = self
            .copied
            .filter(|(_, t)| t.elapsed() < FEEDBACK_DURATION)
            .map(|(i, _)| i);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // --- Eingabeformular (nur nach Klick auf "Neu") ---
                if self.show_new {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label("Beschreibung");
                        let desc_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.new_description)
                                .hint_text("z. B. E-Mail-Signatur")
                                .desired_width(f32::INFINITY),
                        );
                        if self.focus_new {
                            desc_resp.request_focus();
                            self.focus_new = false;
                        }
                        // Enter im Beschreibungsfeld: hinzufügen, oder zum
                        // Inhaltsfeld springen, falls der Inhalt noch leer ist.
                        if desc_resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            if self.new_content.trim().is_empty() {
                                ui.memory_mut(|m| m.request_focus(content_id));
                            } else {
                                add = true;
                            }
                        }

                        ui.add_space(4.0);
                        ui.label("Gruppe (optional)");
                        group_picker(ui, "new_group_combo", &mut self.new_group, &groups);

                        ui.add_space(4.0);
                        ui.label("Inhalt (wird beim Klick kopiert)");
                        egui::ScrollArea::vertical()
                            .id_salt("new_content_scroll")
                            .max_height(110.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.new_content)
                                        .id(content_id)
                                        .hint_text("Der Text, der in die Zwischenablage kopiert wird …")
                                        .desired_rows(4)
                                        .desired_width(f32::INFINITY),
                                );
                            });

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Hinzufügen").clicked() {
                                add = true;
                            }
                            if ui.button("Abbrechen").clicked() {
                                cancel_new = true;
                            }
                        });
                    });
                    ui.add_space(6.0);
                }

                // --- Liste, nach Gruppen sortiert ---
                if self.cfg.snippets.is_empty() {
                    ui.weak("Noch keine Snippets gespeichert.");
                    return;
                }

                let snippets = &self.cfg.snippets;
                let mut sections: Vec<(String, Vec<usize>)> = Vec::new();
                for (i, s) in snippets.iter().enumerate() {
                    let g = s.group.trim();
                    match sections.iter_mut().find(|(name, _)| name == g) {
                        Some((_, idxs)) => idxs.push(i),
                        None => sections.push((g.to_string(), vec![i])),
                    }
                }
                // "Ohne Gruppe" (leerer Name) ans Ende; stabile Sortierung
                // behält die Reihenfolge der benannten Gruppen bei.
                sections.sort_by_key(|(name, _)| name.is_empty());
                let flat = sections.len() == 1 && sections[0].0.is_empty();

                for (name, idxs) in &sections {
                    if flat {
                        snippet_rows(ui, snippets, idxs, copied, reorder, &mut actions);
                    } else {
                        let title = if name.is_empty() {
                            "Ohne Gruppe"
                        } else {
                            name.as_str()
                        };
                        egui::CollapsingHeader::new(
                            egui::RichText::new(format!("{title} ({})", idxs.len())).strong(),
                        )
                        .id_salt(format!("group_{name}"))
                        .default_open(true)
                        .show(ui, |ui| {
                            snippet_rows(ui, snippets, idxs, copied, reorder, &mut actions);
                        });
                    }
                }
            });

        if cancel_new {
            self.show_new = false;
            self.new_description.clear();
            self.new_group.clear();
            self.new_content.clear();
        } else if add && self.add_snippet() {
            self.show_new = false;
        }

        if let Some(i) = actions.delete {
            self.delete_snippet(i);
        } else if let Some(i) = actions.edit {
            self.start_editing(i);
        } else if let Some((i, up)) = actions.mv {
            self.move_snippet(i, up);
        } else if let Some(i) = actions.copy {
            self.copy_snippet(i);
        }
    }

    // ----- Bearbeiten-Dialog (Pop-up) ---------------------------------------

    fn ui_edit_window(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.editing.take() else {
            return;
        };
        let groups = self.group_names();

        let mut keep_open = true;
        let mut save = false;

        egui::Window::new("Snippet bearbeiten")
            .collapsible(false)
            .resizable(false)
            .vscroll(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(dialog_width(ctx));

                ui.label("Beschreibung");
                let desc_resp = ui.add(
                    egui::TextEdit::singleline(&mut edit.description)
                        .desired_width(f32::INFINITY),
                );
                if edit.just_opened {
                    desc_resp.request_focus();
                    edit.just_opened = false;
                }

                ui.add_space(4.0);
                ui.label("Gruppe (optional)");
                group_picker(ui, "edit_group_combo", &mut edit.group, &groups);

                ui.add_space(4.0);
                ui.label("Inhalt");
                egui::ScrollArea::vertical()
                    .id_salt("edit_content_scroll")
                    .max_height(180.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut edit.content)
                                .desired_rows(6)
                                .desired_width(f32::INFINITY),
                        );
                    });

                ui.add_space(6.0);
                let valid =
                    !edit.description.trim().is_empty() && !edit.content.trim().is_empty();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(valid, egui::Button::new("Speichern"))
                        .clicked()
                    {
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
            if let Some(s) = self.cfg.snippets.get_mut(edit.index) {
                s.description = edit.description.trim().to_string();
                s.group = edit.group.trim().to_string();
                s.content = edit.content.clone();
                self.copied = None;
                self.save();
            }
            keep_open = false;
        }

        if keep_open {
            self.editing = Some(edit);
        }
    }

    // ----- Tab: Uhr & Timer -------------------------------------------------

    fn stopwatch_total(&self) -> Duration {
        self.sw_elapsed + self.sw_started.map(|t| t.elapsed()).unwrap_or_default()
    }

    fn ui_clock(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // --- Aktuelle Uhrzeit ---
                let now = Local::now();
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(now.format("%H:%M:%S").to_string())
                            .size(30.0)
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
                ui.heading("Countdown");
                let state = self.cd_state;
                match state {
                    CdState::Idle | CdState::Finished => {
                        ui.horizontal_wrapped(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.cd_h)
                                    .range(0..=99)
                                    .suffix(" h"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut self.cd_m)
                                    .range(0..=59)
                                    .suffix(" min"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut self.cd_s)
                                    .range(0..=59)
                                    .suffix(" s"),
                            );
                        });
                        if state == CdState::Finished {
                            ui.label(
                                egui::RichText::new("Abgelaufen!")
                                    .color(error_color(dark))
                                    .strong(),
                            );
                        }
                        let total = u64::from(self.cd_h) * 3600
                            + u64::from(self.cd_m) * 60
                            + u64::from(self.cd_s);
                        if ui
                            .add_enabled(total > 0, egui::Button::new("Start"))
                            .clicked()
                        {
                            self.cd_state = CdState::Running {
                                end: Instant::now() + Duration::from_secs(total),
                            };
                        }
                    }
                    CdState::Running { end } => {
                        let rem = end.saturating_duration_since(Instant::now());
                        let secs = (rem.as_millis() as u64 + 999) / 1000; // aufrunden
                        ui.label(
                            egui::RichText::new(format_hms(secs))
                                .size(26.0)
                                .monospace(),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Pause").clicked() {
                                self.cd_state = CdState::Paused { remaining: rem };
                            }
                            if ui.button("Zurücksetzen").clicked() {
                                self.cd_state = CdState::Idle;
                            }
                        });
                    }
                    CdState::Paused { remaining } => {
                        let secs = (remaining.as_millis() as u64 + 999) / 1000;
                        ui.label(
                            egui::RichText::new(format_hms(secs))
                                .size(26.0)
                                .monospace()
                                .weak(),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Fortsetzen").clicked() {
                                self.cd_state = CdState::Running {
                                    end: Instant::now() + remaining,
                                };
                            }
                            if ui.button("Zurücksetzen").clicked() {
                                self.cd_state = CdState::Idle;
                            }
                        });
                    }
                }

                ui.add_space(6.0);
                ui.separator();

                // --- Stoppuhr ---
                ui.heading("Stoppuhr");
                ui.label(
                    egui::RichText::new(format_stopwatch(self.stopwatch_total()))
                        .size(26.0)
                        .monospace(),
                );
                ui.horizontal(|ui| {
                    let running = self.sw_started.is_some();
                    if ui.button(if running { "Stopp" } else { "Start" }).clicked() {
                        if let Some(t) = self.sw_started.take() {
                            self.sw_elapsed += t.elapsed();
                        } else {
                            self.sw_started = Some(Instant::now());
                        }
                    }
                    let can_reset = running || !self.sw_elapsed.is_zero();
                    if ui
                        .add_enabled(can_reset, egui::Button::new("Zurücksetzen"))
                        .clicked()
                    {
                        self.sw_started = None;
                        self.sw_elapsed = Duration::ZERO;
                    }
                });
            });
    }

    /// Prüft den Countdown und plant die nötigen Neuzeichnungen.
    fn tick_timers(&mut self, ctx: &egui::Context) {
        if let CdState::Running { end } = self.cd_state {
            let now = Instant::now();
            if now >= end {
                self.cd_state = CdState::Finished;
                // Blinkt in der Taskleiste, bis das Fenster angeklickt wird.
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Critical,
                ));
                ctx.request_repaint();
            } else {
                ctx.request_repaint_after((end - now).min(Duration::from_millis(250)));
            }
        }

        if self.tab == Tab::Clock {
            let interval = if self.sw_started.is_some() { 50 } else { 200 };
            ctx.request_repaint_after(Duration::from_millis(interval));
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

        // Some(true) = in den Tray, Some(false) = beenden
        let mut choice: Option<bool> = None;
        let mut cancel = false;

        egui::Window::new("Programm schließen")
            .collapsible(false)
            .resizable(false)
            .vscroll(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(dialog_width(ctx));
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

    // ----- Tab: Einstellungen -----------------------------------------------

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let dark = ui.visuals().dark_mode;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("Darstellung");
                let mut dark_mode = self.cfg.dark_mode;
                if ui.checkbox(&mut dark_mode, "Dark Mode").changed() {
                    self.cfg.dark_mode = dark_mode;
                    ctx.set_theme(if dark_mode {
                        egui::Theme::Dark
                    } else {
                        egui::Theme::Light
                    });
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

                // Autostart-Optionen nur anzeigen, wenn die Funktion einkompiliert ist.
                if cfg!(feature = "autostart") {
                    ui.add_space(10.0);
                    ui.heading("System");
                    let mut autostart_on = self.cfg.autostart;
                    if ui
                        .checkbox(&mut autostart_on, "Mit Windows starten")
                        .changed()
                    {
                        self.set_autostart(autostart_on);
                    }

                    let mut background = self.cfg.start_in_background;
                    if ui
                        .checkbox(&mut background, "Im Hintergrund starten (nur Tray)")
                        .changed()
                    {
                        self.cfg.start_in_background = background;
                        self.save();
                    }
                    ui.weak("Gilt für den Start über den Windows-Autostart. Manuell gestartet\nöffnet sich das Fenster immer.");
                }

                ui.add_space(10.0);
                ui.heading("Daten");
                ui.label(format!("{} Snippet(s) gespeichert", self.cfg.snippets.len()));
                ui.add_space(4.0);

                if self.confirm_delete_all {
                    ui.colored_label(
                        error_color(dark),
                        "Wirklich ALLE Snippets unwiderruflich löschen?",
                    );
                    ui.horizontal_wrapped(|ui| {
                        let yes = egui::Button::new(
                            egui::RichText::new("Ja, alles löschen")
                                .color(Color32::WHITE)
                                .strong(),
                        )
                        .fill(Color32::from_rgb(190, 50, 50));
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

// ---------------------------------------------------------------------------
// Hilfsfunktionen
// ---------------------------------------------------------------------------

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

/// Dialogbreite, die auch in sehr kleinen Fenstern (ab 200 px) passt.
fn dialog_width(ctx: &egui::Context) -> f32 {
    (ctx.screen_rect().width() - 40.0).clamp(140.0, 300.0)
}

/// Textfeld für die Gruppe + Auswahlliste der vorhandenen Gruppen.
fn group_picker(ui: &mut egui::Ui, id: &str, value: &mut String, groups: &[String]) {
    ui.horizontal(|ui| {
        let reserve = if groups.is_empty() { 0.0 } else { 80.0 };
        ui.add(
            egui::TextEdit::singleline(value)
                .hint_text("z. B. Arbeit")
                .desired_width((ui.available_width() - reserve).max(60.0)),
        );
        if !groups.is_empty() {
            egui::ComboBox::from_id_salt(id)
                .selected_text("Wählen")
                .show_ui(ui, |ui| {
                    for g in groups {
                        if ui.selectable_label(value.as_str() == g, g.as_str()).clicked() {
                            *value = g.clone();
                        }
                    }
                });
        }
    });
}

/// Zeichnet die Zeilen einer Gruppe (bzw. der flachen Liste).
fn snippet_rows(
    ui: &mut egui::Ui,
    snippets: &[Snippet],
    idxs: &[usize],
    copied: Option<usize>,
    reorder: bool,
    actions: &mut RowActions,
) {
    let dark = ui.visuals().dark_mode;

    for (pos, &i) in idxs.iter().enumerate() {
        let snippet = &snippets[i];
        ui.push_id(i, |ui| {
            ui.horizontal(|ui| {
                if reorder {
                    let can_up = pos > 0;
                    let can_down = pos + 1 < idxs.len();
                    if ui
                        .add_enabled(
                            can_up,
                            egui::Button::new("↑").min_size(egui::vec2(22.0, 28.0)),
                        )
                        .clicked()
                    {
                        actions.mv = Some((i, true));
                    }
                    if ui
                        .add_enabled(
                            can_down,
                            egui::Button::new("↓").min_size(egui::vec2(22.0, 28.0)),
                        )
                        .clicked()
                    {
                        actions.mv = Some((i, false));
                    }
                }

                let label = if copied == Some(i) {
                    egui::RichText::new("Kopiert!")
                        .color(ok_color(dark))
                        .strong()
                } else {
                    egui::RichText::new(shorten(&snippet.description, MAX_LABEL_CHARS))
                };

                let width = ui.available_width();
                let response = ui
                    .add_sized([width, 28.0], egui::Button::new(label))
                    .on_hover_text("Linksklick: kopieren · Rechtsklick: Menü");

                if response.clicked() {
                    actions.copy = Some(i);
                }

                response.context_menu(|ui| {
                    if ui.button("Kopieren").clicked() {
                        actions.copy = Some(i);
                        ui.close_menu();
                    }
                    if ui.button("Bearbeiten").clicked() {
                        actions.edit = Some(i);
                        ui.close_menu();
                    }
                    ui.separator();
                    let delete_label =
                        egui::RichText::new("Löschen").color(error_color(dark));
                    if ui.button(delete_label).clicked() {
                        actions.delete = Some(i);
                        ui.close_menu();
                    }
                });
            });
        });
    }
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

fn format_hms(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

fn format_stopwatch(d: Duration) -> String {
    format!("{}.{}", format_hms(d.as_secs()), d.subsec_millis() / 100)
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------

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

        let dark = ctx.style().visuals.dark_mode;

        // Dünner Rahmen um das randlose Fenster.
        let border = if dark {
            Color32::from_rgb(90, 96, 110)
        } else {
            Color32::from_rgb(130, 135, 150)
        };
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("window_border"),
        ))
        .rect_stroke(ctx.screen_rect().shrink(0.5), 0.0, Stroke::new(1.0, border));

        self.ui_titlebar(ctx);

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Snippets, "Snippets");
                ui.selectable_value(&mut self.tab, Tab::Clock, "Uhr & Timer");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Einstellungen");
            });
            ui.add_space(4.0);
        });

        if let Some(msg) = self.status.clone() {
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                ui.colored_label(error_color(dark), msg);
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Snippets => self.ui_snippets(ui),
            Tab::Clock => self.ui_clock(ui),
            Tab::Settings => self.ui_settings(ui),
        });

        // Modaler Bearbeiten-Dialog (liegt über dem Hauptfenster).
        self.ui_edit_window(ctx);

        // Rückfrage beim Schließen (Beenden oder in den Tray).
        self.ui_close_dialog(ctx);
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
        .with_inner_size([380.0, 520.0])
        .with_min_inner_size([200.0, 200.0]) // frühere `min_window_size`
        .with_decorations(false) // frühere `decorated: false` – eigene Titelleiste
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
