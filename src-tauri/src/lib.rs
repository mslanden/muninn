mod audio;
mod commands;
mod db;
mod feeds;
mod llm;
mod models;
mod scheduler;
mod tools;

use models::RunningProcess;
use reqwest::Client;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::Emitter;

pub struct AppState {
    pub workspace_root: Mutex<PathBuf>,
    pub workspace_root_canon: Mutex<PathBuf>,
    pub db: Mutex<Connection>,
    pub processes: Mutex<HashMap<String, RunningProcess>>,
    pub http: Client,
    pub audio: audio::AudioRecorder,
    /// Session IDs that have been requested to stop generation
    pub stop_requests: Mutex<HashSet<String>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use tauri::{Manager, WindowEvent};

    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
            fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
            let db_path = data_dir.join("spinosaur.db");
            let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
            conn.execute("PRAGMA foreign_keys = ON;", []).map_err(|e| e.to_string())?;
            conn.execute("PRAGMA journal_mode = WAL;", []).map_err(|e| e.to_string())?;
            db::init_db(&conn)?;

            let (workspace_root, workspace_root_canon) = tools::default_workspace_root()?;
            conn.execute(
                r#"
                INSERT INTO saved_workspaces (path, last_used_at)
                VALUES (?1, CURRENT_TIMESTAMP)
                ON CONFLICT(path) DO UPDATE SET last_used_at = CURRENT_TIMESTAMP
                "#,
                params![workspace_root.display().to_string()],
            )
            .map_err(|e| e.to_string())?;
            let http = Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .map_err(|e| e.to_string())?;

            app.manage(AppState {
                workspace_root: Mutex::new(workspace_root),
                workspace_root_canon: Mutex::new(workspace_root_canon),
                db: Mutex::new(conn),
                processes: Mutex::new(HashMap::new()),
                http,
                audio: audio::AudioRecorder::new(),
                stop_requests: Mutex::new(HashSet::new()),
            });

            // Plugins
            app.handle().plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                None,
            ))?;
            app.handle().plugin(tauri_plugin_dialog::init())?;

            // System tray
            let show_item = MenuItem::with_id(app, "show", "Show Spino", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().expect("app icon missing — check bundle.icon in tauri.conf.json").clone())
                .tooltip("Spino")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // Start the background task scheduler
            let scheduler_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                scheduler::run_scheduler(scheduler_handle).await;
            });

            // Backfill embeddings for entities created before embeddings were added
            let embed_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Small delay to let the app finish starting
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let state: tauri::State<'_, AppState> = embed_handle.state();
                llm::backfill_embeddings(state.inner()).await;
            });

            // Watch the content directory for changes
            let watcher_handle = app.handle().clone();
            let content_dir = data_dir.join("content");
            fs::create_dir_all(&content_dir).map_err(|e| e.to_string())?;
            std::thread::spawn(move || {
                use notify::{Event, RecursiveMode, Watcher};
                let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
                let mut watcher = match notify::recommended_watcher(tx) {
                    Ok(w) => w,
                    Err(_) => return,
                };
                let _ = watcher.watch(&content_dir, RecursiveMode::NonRecursive);
                // Debounce: only emit once per second
                let mut last_emit = std::time::Instant::now();
                for res in rx {
                    if res.is_ok() && last_emit.elapsed() > std::time::Duration::from_secs(1) {
                        let _ = watcher_handle.emit("content-changed", ());
                        last_emit = std::time::Instant::now();
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_session,
            commands::get_sessions,
            commands::delete_session,
            commands::get_messages,
            commands::get_tool_logs,
            commands::get_tools,
            commands::set_tool_enabled,
            commands::get_feed_items,
            commands::mark_feed_item_read,
            commands::mark_all_feed_read,
            commands::remove_feed_item,
            commands::clear_feed_items,
            commands::get_tasks,
            commands::add_task,
            commands::update_task,
            commands::set_task_status,
            commands::remove_task,
            commands::get_agents,
            commands::add_agent,
            commands::update_agent,
            commands::set_agent_enabled,
            commands::remove_agent,
            commands::get_app_settings,
            commands::set_app_settings,
            commands::get_memory_profile,
            commands::set_memory_profile,
            commands::get_entities,
            commands::upsert_entity,
            commands::delete_entity,
            commands::get_episodes,
            commands::add_episode,
            commands::delete_episode,
            commands::get_relationships,
            commands::upsert_relationship,
            commands::delete_relationship,
            commands::search_messages,
            commands::get_gallery_paths,
            commands::get_gallery_images,
            commands::get_gallery_audio,
            commands::delete_gallery_media,
            commands::open_content_dir,
            commands::get_content_path,
            commands::get_content_files,
            commands::delete_content_file,
            commands::check_command_safety,
            commands::get_workspace_root,
            commands::set_workspace_root,
            commands::list_saved_workspaces,
            commands::save_workspace,
            commands::remove_saved_workspace,
            commands::get_projects,
            commands::add_project,
            commands::remove_project,
            commands::open_project,
            commands::init_project,
            commands::read_file,
            commands::write_file,
            commands::list_files,
            commands::search_files,
            commands::start_terminal_process,
            commands::send_terminal_input,
            commands::stop_terminal_process,
            commands::chat,
            commands::stop_chat,
            commands::transcribe_whisper,
            commands::tts_speak,
            commands::start_voice_recording,
            commands::stop_voice_recording,
            commands::transcribe_wav,
            commands::import_content_files,
            commands::read_content_text,
            commands::open_content_file,
            commands::read_image_base64,
            commands::get_ai_feeds,
            commands::create_ai_feed,
            commands::delete_ai_feed,
            commands::get_ai_feed_items,
            commands::mark_ai_feed_item_read,
            commands::refresh_ai_feed
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                api.prevent_exit();
            }
        });
}
