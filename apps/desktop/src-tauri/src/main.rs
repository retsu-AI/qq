// No console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod engine;
mod project;
mod settings;
mod view;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use qq_protocol::{ApprovalMode, SessionId, ToolCallId};
use tauri::{Emitter, Manager, State, WindowEvent};

use crate::{
    auth::{AUTH_EVENT, AuthChanged, BrowserSignIn, PendingSignIns, ProviderDto, SignInError},
    engine::Engine,
    project::{ApprovalChoice, ProjectHandle, UiCommand},
    settings::Settings,
};

struct AppState {
    engine: Arc<Engine>,
    attentive: Arc<AtomicBool>,
    projects: Mutex<HashMap<String, ProjectHandle>>,
    settings: Mutex<Settings>,
    sign_ins: Arc<PendingSignIns>,
}

impl AppState {
    /// Every open project re-reads its catalog so new or removed
    /// credentials show up as models without restarting anything.
    fn refresh_models(&self) {
        if let Ok(projects) = self.projects.lock() {
            for project in projects.values() {
                let _ = project.send(UiCommand::RefreshModels);
            }
        }
    }

    fn project(&self, id: &str) -> Result<ProjectHandle, String> {
        self.projects
            .lock()
            .map_err(|_| "app state poisoned".to_owned())?
            .get(id)
            .cloned()
            .ok_or_else(|| format!("project {id} is not open"))
    }

    fn send(&self, project_id: &str, command: UiCommand) -> Result<(), String> {
        self.project(project_id)?.send(command)
    }
}

#[derive(serde::Serialize)]
struct ProjectInfo {
    id: String,
    path: String,
    name: String,
}

fn project_info(path: &std::path::Path) -> ProjectInfo {
    ProjectInfo {
        id: project_id(path),
        path: view::display_path(path),
        name: view::display_name(path),
    }
}

/// Projects are keyed by their canonical path, case-folded on Windows.
fn project_id(path: &std::path::Path) -> String {
    let id = path.to_string_lossy().into_owned();
    if cfg!(windows) { id.to_lowercase() } else { id }
}

fn parse_session(id: &str) -> Result<SessionId, String> {
    id.parse().map_err(|_| format!("invalid session id {id}"))
}

/// Folders named on the command line (`qq-desktop <folder>…`), opened at
/// launch the way `code .` opens one.
#[tauri::command]
fn launch_paths() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .filter_map(|arg| std::fs::canonicalize(&arg).ok())
        .filter(|path| path.is_dir())
        .map(|path| view::display_path(&path))
        .collect()
}

#[tauri::command]
fn recent_projects(state: State<'_, AppState>) -> Result<Vec<ProjectInfo>, String> {
    let settings = state.settings.lock().map_err(|_| "settings poisoned")?;
    Ok(settings
        .recent_projects
        .iter()
        .map(|path| project_info(path))
        .collect())
}

#[tauri::command]
fn open_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<ProjectInfo, String> {
    // The server keys workspaces by canonical path, which on Windows keeps
    // the verbatim `\\?\` prefix. The CLI sends that same form, so sessions
    // match across surfaces and the server's catalog requests validate.
    let path = std::fs::canonicalize(&path).map_err(|error| format!("{path}: {error}"))?;
    if !path.is_dir() {
        return Err(format!("{} is not a folder", path.display()));
    }
    let info = project_info(&path);
    let preferred_model = {
        let mut settings = state.settings.lock().map_err(|_| "settings poisoned")?;
        settings.remember_project(&path);
        settings.save(&app);
        settings.preferred_model.clone()
    };
    let mut projects = state.projects.lock().map_err(|_| "app state poisoned")?;
    match projects.get(&info.id) {
        Some(existing) => {
            let _ = existing.send(UiCommand::Refresh);
        }
        None => {
            let handle = project::open(
                app.clone(),
                Arc::clone(&state.engine),
                Arc::clone(&state.attentive),
                info.id.clone(),
                path,
                preferred_model,
            );
            projects.insert(info.id.clone(), handle);
        }
    }
    Ok(info)
}

#[tauri::command]
fn close_project(state: State<'_, AppState>, project_id: String) -> Result<(), String> {
    state
        .projects
        .lock()
        .map_err(|_| "app state poisoned")?
        .remove(&project_id);
    Ok(())
}

#[tauri::command]
fn forget_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_id: String,
) -> Result<(), String> {
    state
        .projects
        .lock()
        .map_err(|_| "app state poisoned")?
        .remove(&project_id);
    let mut settings = state.settings.lock().map_err(|_| "settings poisoned")?;
    settings
        .recent_projects
        .retain(|path| self::project_id(path) != project_id);
    settings.save(&app);
    Ok(())
}

#[tauri::command]
fn refresh(state: State<'_, AppState>, project_id: String) -> Result<(), String> {
    state.send(&project_id, UiCommand::Refresh)
}

#[tauri::command]
fn focus_session(
    state: State<'_, AppState>,
    project_id: String,
    session_id: Option<String>,
) -> Result<(), String> {
    let session_id = session_id.as_deref().map(parse_session).transpose()?;
    state.send(&project_id, UiCommand::Focus(session_id))
}

#[tauri::command]
fn new_session(state: State<'_, AppState>, project_id: String) -> Result<(), String> {
    state.send(&project_id, UiCommand::NewSession)
}

#[tauri::command]
fn submit_prompt(
    state: State<'_, AppState>,
    project_id: String,
    session_id: Option<String>,
    text: String,
) -> Result<(), String> {
    let session_id = session_id.as_deref().map(parse_session).transpose()?;
    state.send(&project_id, UiCommand::Submit { session_id, text })
}

#[tauri::command]
fn cancel_run(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
) -> Result<(), String> {
    state.send(&project_id, UiCommand::Cancel(parse_session(&session_id)?))
}

#[tauri::command]
fn respond_approval(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    tool_call_id: String,
    decision: ApprovalChoice,
) -> Result<(), String> {
    let tool_call_id: ToolCallId = tool_call_id
        .parse()
        .map_err(|_| format!("invalid tool call id {tool_call_id}"))?;
    state.send(
        &project_id,
        UiCommand::Approve {
            session_id: parse_session(&session_id)?,
            tool_call_id,
            decision,
        },
    )
}

#[tauri::command]
fn set_approval_mode(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    mode: ApprovalMode,
) -> Result<(), String> {
    state.send(
        &project_id,
        UiCommand::SetApprovalMode(parse_session(&session_id)?, mode),
    )
}

#[tauri::command]
fn select_model(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_id: String,
    model: String,
) -> Result<(), String> {
    {
        let mut settings = state.settings.lock().map_err(|_| "settings poisoned")?;
        settings.preferred_model = Some(model.clone());
        settings.save(&app);
    }
    state.send(&project_id, UiCommand::SelectModel(model))
}

#[tauri::command]
fn delete_session(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
) -> Result<(), String> {
    state.send(&project_id, UiCommand::Delete(parse_session(&session_id)?))
}

#[tauri::command]
async fn auth_providers() -> Result<Vec<ProviderDto>, SignInError> {
    tauri::async_runtime::spawn_blocking(auth::providers)
        .await
        .map_err(|_| SignInError::TaskFailed)?
}

#[tauri::command]
async fn auth_save_key(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    provider: String,
    key: String,
) -> Result<(), SignInError> {
    let saved = provider.clone();
    tauri::async_runtime::spawn_blocking(move || auth::save_key(&saved, &key))
        .await
        .map_err(|_| SignInError::TaskFailed)??;
    state.refresh_models();
    let _ = app.emit(
        AUTH_EVENT,
        AuthChanged {
            provider,
            error: None,
        },
    );
    Ok(())
}

#[tauri::command]
async fn auth_sign_out(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    provider: String,
) -> Result<(), SignInError> {
    let removed = provider.clone();
    tauri::async_runtime::spawn_blocking(move || auth::sign_out(&removed))
        .await
        .map_err(|_| SignInError::TaskFailed)??;
    state.refresh_models();
    let _ = app.emit(
        AUTH_EVENT,
        AuthChanged {
            provider,
            error: None,
        },
    );
    Ok(())
}

/// Starts a browser sign-in and returns at once with the URL (and device
/// code) to show; completion arrives later as an `auth-changed` event.
#[tauri::command]
async fn auth_browser_sign_in(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    provider: String,
) -> Result<BrowserSignIn, SignInError> {
    let pending = Arc::clone(&state.sign_ins);
    let id = provider.clone();
    let (started, shown) =
        tauri::async_runtime::spawn_blocking(move || auth::start_browser(&pending, &id))
            .await
            .map_err(|_| SignInError::TaskFailed)??;
    let pending = Arc::clone(&state.sign_ins);
    tauri::async_runtime::spawn(async move {
        let result = tauri::async_runtime::spawn_blocking(move || auth::finish_browser(started))
            .await
            .map_err(|_| SignInError::TaskFailed)
            .and_then(|result| result);
        pending.release(&provider);
        if result.is_ok() {
            app.state::<AppState>().refresh_models();
        }
        let _ = app.emit(
            AUTH_EVENT,
            AuthChanged {
                provider,
                error: result.err().map(|error| error.to_string()),
            },
        );
    });
    Ok(shown)
}

fn main() {
    let engine = Arc::new(Engine::default());
    let shutdown_engine = Arc::clone(&engine);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let settings = Settings::load(app.handle());
            app.manage(AppState {
                engine,
                attentive: Arc::new(AtomicBool::new(true)),
                projects: Mutex::new(HashMap::new()),
                settings: Mutex::new(settings),
                sign_ins: Arc::new(PendingSignIns::default()),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(focused) = event {
                window
                    .state::<AppState>()
                    .attentive
                    .store(*focused, Ordering::Relaxed);
            }
        })
        .invoke_handler(tauri::generate_handler![
            launch_paths,
            recent_projects,
            open_project,
            close_project,
            forget_project,
            refresh,
            focus_session,
            new_session,
            submit_prompt,
            cancel_run,
            respond_approval,
            set_approval_mode,
            select_model,
            delete_session,
            auth_providers,
            auth_save_key,
            auth_sign_out,
            auth_browser_sign_in,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the QQ desktop app")
        .run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                shutdown_engine.shutdown();
            }
        });
}
