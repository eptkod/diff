//! Tauri wiring: tray, global hotkey, windows and the commands the UI calls.

use crate::capture::{self, Source, SourceInfo};
use crate::clip::ClipError;
use crate::engine::{has_capture_permission, Engine, EngineHooks, PendingClip, Status};
use crate::library::{self, ClipInfo};
use crate::settings::Settings;
use crate::trim::trim_mp4;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(target_os = "macos")]
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, RunEvent, WebviewUrl,
    WebviewWindowBuilder,
};
use tauri_plugin_autostart::ManagerExt as AutostartExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

const SETTINGS_CHANGED: &str = "settings-changed";
const CLIPS_CHANGED: &str = "clips-changed";
const MIN_REGION_POINTS: f64 = 32.0;
const MAX_TRAY_NAME_CHARS: usize = 36;

#[cfg(target_os = "macos")]
const TRAY_RECORDING: &[u8] = include_bytes!("../icons/tray/tray-recording.png");
#[cfg(target_os = "macos")]
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray/tray-idle.png");

struct TrayMenu {
    status: MenuItem<tauri::Wry>,
    pause: MenuItem<tauri::Wry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppState {
    settings: Settings,
    status: Status,
    has_permission: bool,
    voice_issue: Option<String>,
    following: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryClip {
    #[serde(flatten)]
    clip: ClipInfo,
    thumbnail: Option<PathBuf>,
}

fn thumbnails_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_cache_dir().map_err(text)?.join("thumbnails"))
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn engine(app: &AppHandle) -> Arc<Engine> {
    app.state::<Arc<Engine>>().inner().clone()
}

fn status_label(status: &Status, settings: &Settings, following: Option<&str>) -> String {
    let length = format_length(settings.clip_seconds);
    match status {
        Status::Recording if settings.source == Some(Source::ActiveWindow) => match following {
            Some(name) => format!("Recording {}: last {length}", shorten(name)),
            None => "Waiting for a window to record".into(),
        },
        Status::Recording => format!("Recording: last {length}"),
        Status::Paused => "Paused".into(),
        Status::Stopped => "Not recording".into(),
        Status::NeedsPermission => "Needs screen recording permission".into(),
        Status::Error { .. } => "Recording failed".into(),
    }
}

fn shorten(name: &str) -> String {
    if name.chars().count() <= MAX_TRAY_NAME_CHARS {
        return name.to_string();
    }
    let kept: String = name.chars().take(MAX_TRAY_NAME_CHARS - 1).collect();
    format!("{}…", kept.trim_end())
}

fn format_length(seconds: u32) -> String {
    if seconds < 60 { format!("{seconds}s") } else { format!("{}m", seconds / 60) }
}

fn refresh_tray(app: &AppHandle) {
    let engine = engine(app);
    let status = engine.status();
    let following = engine.following_name();
    let label = status_label(&status, &engine.settings(), following.as_deref());
    if let Some(tray) = app.try_state::<TrayMenu>() {
        let _ = tray.status.set_text(&label);
        let _ = tray.pause.set_text(if status == Status::Paused { "Resume" } else { "Pause" });
    }
    if let Some(icon) = app.tray_by_id("main") {
        let _ = icon.set_tooltip(Some(format!("diff: {label}")));
        #[cfg(target_os = "macos")]
        {
            let bytes = if status == Status::Recording { TRAY_RECORDING } else { TRAY_IDLE };
            if let Ok(image) = Image::from_bytes(bytes) {
                let _ = icon.set_icon_with_as_template(Some(image), true);
            }
        }
    }
}

/// Starting capture waits on system services, so it stays off the main thread.
fn start_engine(app: &AppHandle) {
    let engine = engine(app);
    let app = app.clone();
    std::thread::spawn(move || {
        engine.start();
        refresh_tray(&app);
        let _ = app.emit(SETTINGS_CHANGED, ());
    });
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Entry point for every clip trigger: hotkey, tray menu, and voice.
pub fn trigger_clip(app: &AppHandle) {
    match engine(app).prepare_clip() {
        Ok(Some(pending)) => {
            let app = app.clone();
            std::thread::spawn(move || finish_clip(&app, &pending));
        }
        Ok(None) => {}
        Err(ClipError::NothingRecorded) => {
            notify(app, "Nothing to clip yet", &engine(app).nothing_recorded_reason());
        }
        Err(error) => notify(app, "Couldn't save clip", &error.to_string()),
    }
}

fn finish_clip(app: &AppHandle, pending: &PendingClip) {
    match pending.save() {
        Ok(path) => {
            // Heard even when notifications are silenced.
            crate::chime::play();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let body = match &pending.window_name {
                Some(window) => format!("From {window}\n{name}"),
                None => name,
            };
            notify(app, "Clip saved", &body);
            let _ = app.emit(CLIPS_CHANGED, ());
        }
        Err(error) => notify(app, "Couldn't save clip", &error.to_string()),
    }
}

fn register_hotkey(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let shortcut: Shortcut = hotkey.parse().map_err(|_| format!("\"{hotkey}\" isn't a valid shortcut"))?;
    app.global_shortcut()
        .on_shortcut(shortcut, |app, _, event| {
            if event.state == ShortcutState::Pressed {
                trigger_clip(app);
            }
        })
        .map_err(|_| format!("\"{hotkey}\" is already used by another app"))
}

fn allow_media_folder(app: &AppHandle, folder: &Path) {
    let _ = std::fs::create_dir_all(folder);
    let _ = app.asset_protocol_scope().allow_directory(folder, false);
}

fn open_window(app: &AppHandle, label: &str, route: &str, title: &str, size: (f64, f64)) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(label) {
        window.show().map_err(text)?;
        return window.set_focus().map_err(text);
    }
    let window = WebviewWindowBuilder::new(app, label, WebviewUrl::App(route.into()))
        .title(title)
        .inner_size(size.0, size.1)
        .min_inner_size(size.0.min(420.0), 420.0)
        .center()
        .build()
        .map_err(text)?;
    window.set_focus().map_err(text)
}

fn open_library(app: &AppHandle) -> Result<(), String> {
    open_window(app, "library", "library", "diff", (920.0, 620.0))
}

fn open_settings(app: &AppHandle) -> Result<(), String> {
    open_window(app, "settings", "settings", "diff settings", (460.0, 680.0))
}

fn open_setup(app: &AppHandle) -> Result<(), String> {
    open_window(app, "setup", "setup", "Welcome to diff", (460.0, 560.0))
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let clip = MenuItem::with_id(app, "clip", "Clip now", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause", true, None::<&str>)?;
    let library = MenuItem::with_id(app, "library", "Clips…", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit diff", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &clip,
            &pause,
            &PredefinedMenuItem::separator(app)?,
            &library,
            &settings,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("main")
        .icon_as_template(cfg!(target_os = "macos"))
        .menu(&menu)
        .tooltip("diff")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "clip" => trigger_clip(app),
            "pause" => {
                let engine = engine(app);
                if engine.status() == Status::Paused {
                    start_engine(app);
                } else {
                    let app = app.clone();
                    std::thread::spawn(move || {
                        engine.pause();
                        refresh_tray(&app);
                        let _ = app.emit(SETTINGS_CHANGED, ());
                    });
                }
            }
            "library" => {
                let _ = open_library(app);
            }
            "settings" => {
                let _ = open_settings(app);
            }
            "quit" => app.exit(0),
            _ => {}
        });
    #[cfg(target_os = "macos")]
    {
        builder = builder.icon(Image::from_bytes(TRAY_IDLE)?);
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    app.manage(TrayMenu { status, pause });
    Ok(())
}

fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory)?;

    let default_folder = app.path().video_dir()?.join("diff");
    let settings_path = app.path().app_config_dir()?.join("settings.json");
    let voice_model_dir = app.path().resolve("resources/voice", BaseDirectory::Resource)?;
    let hooks = EngineHooks {
        on_voice_trigger: {
            let app = app.clone();
            Arc::new(move || trigger_clip(&app))
        },
        on_notice: {
            let app = app.clone();
            Arc::new(move |title: &str, body: &str| notify(&app, title, body))
        },
        on_status_changed: {
            let app = app.clone();
            Arc::new(move || {
                refresh_tray(&app);
                let _ = app.emit(SETTINGS_CHANGED, ());
            })
        },
    };
    let engine = Arc::new(Engine::new(settings_path, &default_folder, voice_model_dir, hooks));
    app.manage(engine.clone());
    engine.spawn_window_follower();

    let settings = engine.settings();
    allow_media_folder(app, &settings.save_folder);
    let thumbnails = thumbnails_dir(app)?;
    allow_media_folder(app, &thumbnails);

    build_tray(app)?;
    if let Err(message) = register_hotkey(app, &settings.hotkey) {
        notify(app, "Clip shortcut unavailable", &message);
    }

    if settings.setup_complete {
        start_engine(app);
    } else {
        open_setup(app)?;
    }
    refresh_tray(app);
    Ok(())
}

#[tauri::command]
fn get_state(app: AppHandle) -> AppState {
    let engine = engine(&app);
    AppState {
        settings: engine.settings(),
        status: engine.status(),
        has_permission: has_capture_permission(),
        voice_issue: engine.voice_issue(),
        following: engine.following_name(),
    }
}

/// Applies new settings. Anything that can be rejected is checked first, so a
/// failure leaves the previous settings fully in place.
#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> Result<AppState, String> {
    let engine = engine(&app);
    let previous = engine.settings();

    if settings.hotkey != previous.hotkey {
        let shortcuts = app.global_shortcut();
        shortcuts.unregister_all().map_err(text)?;
        if let Err(message) = register_hotkey(&app, &settings.hotkey) {
            let _ = register_hotkey(&app, &previous.hotkey);
            return Err(message);
        }
    }
    if settings.start_at_login != previous.start_at_login {
        let autostart = app.autolaunch();
        if settings.start_at_login { autostart.enable() } else { autostart.disable() }.map_err(text)?;
    }
    if settings.save_folder != previous.save_folder {
        allow_media_folder(&app, &settings.save_folder);
    }

    engine.update_settings(settings).map_err(text)?;
    refresh_tray(&app);
    let _ = app.emit(SETTINGS_CHANGED, ());
    let _ = app.emit(CLIPS_CHANGED, ());
    Ok(get_state(app))
}

#[tauri::command]
async fn finish_setup(app: AppHandle) -> Result<(), String> {
    let engine = engine(&app);
    let settings = Settings { setup_complete: true, ..engine.settings() };
    engine.update_settings(settings).map_err(text)?;
    engine.start();
    refresh_tray(&app);
    if let Some(window) = app.get_webview_window("setup") {
        let _ = window.close();
    }
    let _ = app.emit(SETTINGS_CHANGED, ());
    Ok(())
}

#[tauri::command]
async fn retry_capture(app: AppHandle) -> AppState {
    engine(&app).start();
    refresh_tray(&app);
    get_state(app)
}

#[tauri::command]
fn request_screen_permission() -> bool {
    capture::request_permission()
}

#[tauri::command]
async fn open_permission_settings(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    app.opener()
        .open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture", None::<&str>)
        .map_err(text)?;
    #[cfg(not(target_os = "macos"))]
    let _ = app;
    Ok(())
}

#[tauri::command]
async fn open_microphone_settings(app: AppHandle) -> Result<(), String> {
    let url = if cfg!(target_os = "macos") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
    } else {
        "ms-settings:privacy-microphone"
    };
    app.opener().open_url(url, None::<&str>).map_err(text)
}

#[tauri::command]
async fn list_sources() -> Result<Vec<SourceInfo>, String> {
    capture::list_sources().map_err(text)
}

#[tauri::command]
async fn choose_folder(app: AppHandle) -> Option<PathBuf> {
    app.dialog().file().blocking_pick_folder().and_then(|path| path.into_path().ok())
}

#[tauri::command]
async fn open_region_picker(app: AppHandle, display_id: u32) -> Result<(), String> {
    let sources = capture::list_sources().map_err(text)?;
    let display = sources
        .iter()
        .find(|s| s.source == Source::Display { id: display_id })
        .ok_or("That display is no longer connected.")?;

    if let Some(existing) = app.get_webview_window("region") {
        let _ = existing.close();
    }
    let route = format!("region?display={display_id}");
    let window = WebviewWindowBuilder::new(&app, "region", WebviewUrl::App(route.into()))
        .title("Select a region")
        .transparent(true)
        .decorations(false)
        .always_on_top(true)
        .resizable(false)
        .shadow(false)
        .skip_taskbar(true)
        .build()
        .map_err(text)?;
    window.set_position(LogicalPosition::new(display.x, display.y)).map_err(text)?;
    window.set_size(LogicalSize::new(display.width, display.height)).map_err(text)?;
    window.set_focus().map_err(text)
}

#[tauri::command]
async fn set_region(
    app: AppHandle,
    display_id: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<AppState, String> {
    if width < MIN_REGION_POINTS || height < MIN_REGION_POINTS {
        return Err("That region is too small.".into());
    }
    let engine = engine(&app);
    let source = Source::Region { display_id, x, y, width, height };
    engine
        .update_settings(Settings { source: Some(source), ..engine.settings() })
        .map_err(text)?;
    refresh_tray(&app);
    let _ = app.emit(SETTINGS_CHANGED, ());
    if let Some(window) = app.get_webview_window("region") {
        let _ = window.close();
    }
    Ok(get_state(app))
}

#[tauri::command]
async fn open_library_window(app: AppHandle) -> Result<(), String> {
    open_library(&app)
}

#[tauri::command]
async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    open_settings(&app)
}

#[tauri::command]
async fn clip_now(app: AppHandle) {
    trigger_clip(&app);
}

#[tauri::command]
async fn list_clips(app: AppHandle) -> Result<Vec<LibraryClip>, String> {
    let folder = engine(&app).settings().save_folder;
    let thumbnails = thumbnails_dir(&app)?;
    let clips = library::list_clips(&folder).map_err(text)?;
    Ok(clips
        .into_iter()
        .map(|clip| {
            let path = thumbnails.join(library::thumbnail_file_name(&clip));
            LibraryClip { thumbnail: path.exists().then_some(path), clip }
        })
        .collect())
}

fn clip_path(app: &AppHandle, path: &Path) -> Result<PathBuf, String> {
    library::ensure_in_folder(path, &engine(app).settings().save_folder).map_err(text)
}

#[tauri::command]
async fn delete_clip(app: AppHandle, path: PathBuf) -> Result<(), String> {
    let resolved = clip_path(&app, &path)?;
    trash::delete(&resolved).map_err(text)?;
    // So a later clip with the same name doesn't arrive starred.
    let _ = library::set_starred(&resolved, false);
    let _ = app.emit(CLIPS_CHANGED, ());
    Ok(())
}

#[tauri::command]
async fn set_clip_starred(app: AppHandle, path: PathBuf, starred: bool) -> Result<(), String> {
    library::set_starred(&clip_path(&app, &path)?, starred).map_err(text)?;
    let _ = app.emit(CLIPS_CHANGED, ());
    Ok(())
}

#[tauri::command]
async fn set_clips_starred(app: AppHandle, paths: Vec<PathBuf>, starred: bool) -> Result<(), String> {
    let resolved = library::existing_in_folder(&paths, &engine(&app).settings().save_folder).map_err(text)?;
    let result = resolved.iter().try_for_each(|path| library::set_starred(path, starred)).map_err(text);
    let _ = app.emit(CLIPS_CHANGED, ());
    result
}

#[tauri::command]
async fn delete_clips(app: AppHandle, paths: Vec<PathBuf>) -> Result<(), String> {
    let resolved = library::existing_in_folder(&paths, &engine(&app).settings().save_folder).map_err(text)?;
    let result = if resolved.is_empty() { Ok(()) } else { trash::delete_all(&resolved).map_err(text) };
    for gone in resolved.iter().filter(|path| !path.exists()) {
        let _ = library::set_starred(gone, false);
    }
    // Some clips may have gone even if the batch failed partway.
    let _ = app.emit(CLIPS_CHANGED, ());
    result
}

#[tauri::command]
async fn rename_clip(app: AppHandle, path: PathBuf, name: String) -> Result<PathBuf, String> {
    let renamed = library::rename_clip(&clip_path(&app, &path)?, &name).map_err(text)?;
    let _ = app.emit(CLIPS_CHANGED, ());
    Ok(renamed)
}

#[tauri::command]
async fn trim_clip(app: AppHandle, path: PathBuf, start: f64, end: f64) -> Result<PathBuf, String> {
    let source = clip_path(&app, &path)?;
    let destination = library::trimmed_path(&source).ok_or("Couldn't name the trimmed clip.")?;
    let (start_us, end_us) = ((start * 1_000_000.0) as i64, (end * 1_000_000.0) as i64);
    let output = destination.clone();
    tauri::async_runtime::spawn_blocking(move || trim_mp4(&source, &output, start_us, end_us))
        .await
        .map_err(text)?
        .map_err(text)?;
    let _ = app.emit(CLIPS_CHANGED, ());
    Ok(destination)
}

#[tauri::command]
async fn reveal_clip(app: AppHandle, path: PathBuf) -> Result<(), String> {
    let resolved = clip_path(&app, &path)?;
    app.opener().reveal_item_in_dir(resolved).map_err(text)
}

const MAX_THUMBNAIL_BYTES: usize = 512 * 1024;

#[tauri::command]
async fn save_thumbnail(app: AppHandle, path: PathBuf, modified_ms: u64, jpeg: Vec<u8>) -> Result<PathBuf, String> {
    if jpeg.len() > MAX_THUMBNAIL_BYTES || !jpeg.starts_with(&[0xFF, 0xD8]) {
        return Err("Invalid thumbnail.".into());
    }
    // Hash the path as the library listed it, so the lookup in `list_clips` matches.
    clip_path(&app, &path)?;
    let clip = ClipInfo { path, name: String::new(), size_bytes: 0, duration_seconds: 0.0, modified_ms, starred: false };
    let folder = thumbnails_dir(&app)?;
    std::fs::create_dir_all(&folder).map_err(text)?;
    let destination = folder.join(library::thumbnail_file_name(&clip));
    std::fs::write(&destination, jpeg).map_err(text)?;
    Ok(destination)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be first, so a second launch exits before doing any work.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            let _ = open_library(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| setup(app.handle()))
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_settings,
            finish_setup,
            retry_capture,
            request_screen_permission,
            open_permission_settings,
            open_microphone_settings,
            list_sources,
            choose_folder,
            open_region_picker,
            set_region,
            open_library_window,
            open_settings_window,
            clip_now,
            list_clips,
            delete_clip,
            delete_clips,
            set_clip_starred,
            set_clips_starred,
            rename_clip,
            trim_clip,
            reveal_clip,
            save_thumbnail,
        ])
        .build(tauri::generate_context!())
        .expect("error while starting diff")
        .run(|_, event| {
            // Closing every window must leave the app running in the tray.
            if let RunEvent::ExitRequested { api, code: None, .. } = event {
                api.prevent_exit();
            }
        });
}
