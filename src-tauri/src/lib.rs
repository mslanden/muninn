mod audio;
mod commands;
mod db;
mod feeds;
mod interactive;
mod llm;
mod mail;
mod mcp;
mod models;
mod notify_user;
mod scheduler;
mod search;
mod todos;
mod tools;

use models::RunningProcess;
use r2d2_sqlite::SqliteConnectionManager;
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::Emitter;

pub type DbPool = r2d2::Pool<SqliteConnectionManager>;

pub struct AppState {
    pub workspace_root: Mutex<PathBuf>,
    pub workspace_root_canon: Mutex<PathBuf>,
    pub db: DbPool,
    pub processes: Mutex<HashMap<String, RunningProcess>>,
    pub http: Client,
    pub audio: audio::AudioRecorder,
    /// Session IDs that have been requested to stop generation
    pub stop_requests: Mutex<HashSet<String>>,
    /// Agent-owned to-do lists, keyed by session_id. Ephemeral — not persisted.
    pub todos: Mutex<HashMap<String, Vec<models::TodoItem>>>,
    /// Outstanding `ask_user` questions, keyed by session_id. Sender fires when
    /// the user answers; drop the sender to cancel.
    pub pending_questions: Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
    /// Connections to configured MCP servers (lazy-spawned stdio children).
    pub mcp: mcp::McpManager,
}

/// Show/hide the frameless quick-ask overlay (created on first use). Lives at
/// module level because both the global shortcut handler and (potentially) the
/// tray need it.
fn toggle_quickask_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("quickask") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
            let _ = w.emit("quickask-shown", ());
        }
        return;
    }
    let result = tauri::WebviewWindowBuilder::new(
        app,
        "quickask",
        tauri::WebviewUrl::App("index.html?quickask=1".into()),
    )
    .title("Muninn Quick Ask")
    .inner_size(680.0, 460.0)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .visible_on_all_workspaces(true)
    .center()
    .focused(true)
    .build();
    if let Err(e) = result {
        eprintln!("[QuickAsk] failed to create window: {}", e);
    }
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
            let db_path = data_dir.join("muninn.db");
            // Pooled connections: WAL allows concurrent readers, so chat turns, the
            // scheduler, and UI queries no longer serialize on one connection. Pragmas
            // run on every new connection (execute_batch, not execute — see 09fc056).
            let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
                c.execute_batch(
                    "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;",
                )
            });
            let pool = r2d2::Pool::builder()
                .max_size(8)
                .build(manager)
                .map_err(|e| e.to_string())?;
            let conn = pool.get().map_err(|e| e.to_string())?;
            db::init_db(&conn)?;
            // A quit/crash mid-run leaves tasks stuck in 'running'; reschedule them.
            scheduler::recover_stuck_tasks(&conn);

            let (workspace_root, workspace_root_canon) = tools::default_workspace_root()?;
            drop(conn);
            let http = Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .map_err(|e| e.to_string())?;

            app.manage(AppState {
                workspace_root: Mutex::new(workspace_root),
                workspace_root_canon: Mutex::new(workspace_root_canon),
                db: pool,
                processes: Mutex::new(HashMap::new()),
                http,
                audio: audio::AudioRecorder::new(),
                stop_requests: Mutex::new(HashSet::new()),
                todos: Mutex::new(HashMap::new()),
                pending_questions: Mutex::new(HashMap::new()),
                mcp: mcp::McpManager::default(),
            });

            // Plugins
            app.handle().plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                None,
            ))?;
            app.handle().plugin(tauri_plugin_dialog::init())?;
            app.handle().plugin(tauri_plugin_notification::init())?;

            // Global quick-ask shortcut. The accelerator comes from settings
            // (quickask/shortcut, default Cmd+Shift+Space); changes take effect
            // on next launch.
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(|app, _shortcut, event| {
                        if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                            toggle_quickask_window(app);
                        }
                    })
                    .build(),
            )?;
            {
                use tauri_plugin_global_shortcut::GlobalShortcutExt;
                let state: tauri::State<'_, AppState> = app.state();
                let accel = db::get_app_setting_value(&state, "quickask/shortcut")
                    .ok()
                    .flatten()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "cmd+shift+space".to_string());
                if let Err(e) = app.global_shortcut().register(accel.as_str()) {
                    eprintln!("[QuickAsk] failed to register '{}': {} — falling back to cmd+shift+space", accel, e);
                    let _ = app.global_shortcut().register("cmd+shift+space");
                }
            }

            // System tray
            let show_item = MenuItem::with_id(app, "show", "Show Muninn", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().expect("app icon missing — check bundle.icon in tauri.conf.json").clone())
                .tooltip("Muninn")
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
            commands::update_ai_feed,
            commands::get_ai_feed_items,
            commands::mark_ai_feed_item_read,
            commands::generate_feed_item_image,
            commands::generate_feed_article,
            commands::refresh_ai_feed,
            commands::ai_web_search,
            commands::geocode_user_location,
            commands::get_session_todos,
            commands::answer_user_question,
            commands::cancel_user_question,
            commands::has_pending_question,
            commands::get_mcp_status,
            commands::reload_mcp_servers
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                api.prevent_exit();
            }
        });
}
