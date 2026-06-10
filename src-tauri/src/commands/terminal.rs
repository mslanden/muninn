//! PTY-backed terminal processes for the UI terminal.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;


use crate::AppState;
use crate::models::*;
use crate::tools::*;

#[tauri::command]
pub fn start_terminal_process(
    app: AppHandle,
    state: State<AppState>,
    req: TerminalStartRequest,
) -> Result<ProcessStartResponse, String> {
    let safety = is_dangerous_command(&req.command);
    if safety.dangerous && !req.approved_dangerous {
        return Err(format!(
            "command requires approval: {}",
            safety.reasons.join("; ")
        ));
    }

    let cwd = command_cwd(&state, req.cwd.as_deref())?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let mut command = CommandBuilder::new("/bin/zsh");
    command.arg("-lc");
    command.arg(req.command.clone());
    command.cwd(cwd);

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| e.to_string())?;
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    let process_id = Uuid::new_v4().to_string();
    let writer_arc: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
    let child_arc: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));

    {
        let mut processes = state
            .processes
            .lock()
            .map_err(|_| "process lock poisoned".to_string())?;
        processes.insert(
            process_id.clone(),
            RunningProcess {
                writer: writer_arc,
                child: child_arc.clone(),
            },
        );
    }

    log_tool(
        &state,
        &req.session_id,
        "terminal_start",
        &req.command,
        "started",
        "ok",
    );

    let app_clone = app.clone();
    let process_id_clone = process_id.clone();

    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = app_clone.emit(
                        "terminal-output",
                        TerminalOutputEvent {
                            process_id: process_id_clone.clone(),
                            chunk,
                        },
                    );
                }
                Err(_) => break,
            }
        }

        let exit_code = match child_arc.lock() {
            Ok(mut child) => match child.wait() {
                Ok(status) => Some(status.exit_code()),
                Err(_) => None,
            },
            Err(_) => None,
        };

        let _ = app_clone.emit(
            "terminal-exit",
            TerminalExitEvent {
                process_id: process_id_clone,
                exit_code,
            },
        );
    });

    Ok(ProcessStartResponse { process_id })
}

#[tauri::command]
pub fn send_terminal_input(state: State<AppState>, req: TerminalInputRequest) -> Result<(), String> {
    let processes = state
        .processes
        .lock()
        .map_err(|_| "process lock poisoned".to_string())?;
    let process = processes
        .get(&req.process_id)
        .ok_or_else(|| "process not found".to_string())?;
    let mut writer = process
        .writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;

    writer
        .write_all(req.input.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn stop_terminal_process(state: State<AppState>, req: TerminalStopRequest) -> Result<(), String> {
    let mut processes = state
        .processes
        .lock()
        .map_err(|_| "process lock poisoned".to_string())?;
    let process = processes
        .remove(&req.process_id)
        .ok_or_else(|| "process not found".to_string())?;

    let mut child = process
        .child
        .lock()
        .map_err(|_| "child lock poisoned".to_string())?;
    child.kill().map_err(|e| e.to_string())?;
    Ok(())
}
