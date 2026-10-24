#[path = "auth.rs"]
mod auth;
#[path = "editor.rs"]
mod editor;
#[path = "engine.rs"]
mod engine;
#[path = "git.rs"]
mod git;
#[path = "goals.rs"]
mod goals;
#[path = "input.rs"]
mod input;
#[path = "project.rs"]
mod project;
#[path = "settings.rs"]
mod settings;
#[path = "trust.rs"]
mod trust;
#[path = "view.rs"]
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
use tauri_plugin_window_state::StateFlags;

use self::{
    auth::{AUTH_EVENT, AuthChanged, BrowserSignIn, PendingSignIns, ProviderDto, SignInError},
    engine::Engine,
    project::{ApprovalChoice, ProjectHandle, SessionAction, SessionOption, SubmitMode, UiCommand},
    settings::Settings,
    trust::{PendingFileDto, TrustError},
};

struct AppState {
    engine: Arc<Engine>,
    attentive: Arc<AtomicBool>,
    projects: Mutex<HashMap<String, ProjectHandle>>,
    settings: Mutex<Settings>,
    sign_ins: Arc<PendingSignIns>,
    input_slots: Arc<tokio::sync::Semaphore>,
    notifications_enabled: AtomicBool,
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
    /// Set when the folder is a linked Git worktree.
    worktree: Option<git::WorktreeDto>,
}

fn project_info(path: &std::path::Path) -> ProjectInfo {
    ProjectInfo {
        id: project_id(path),
        path: view::display_path(path),
        name: view::display_name(path),
        worktree: git::worktree_of(path),
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
    if !projects.contains_key(&info.id) && projects.len() >= 16 {
        return Err("at most 16 projects may be open; close a project first".to_owned());
    }
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

/// Creates a worktree for `branch` beside the project's repository and
/// returns its folder, which the UI then opens as a project.
#[tauri::command]
async fn create_worktree(
    state: State<'_, AppState>,
    project_id: String,
    branch: String,
    base: Option<String>,
) -> Result<String, String> {
    let root = state.project(&project_id)?.path;
    let _guard = GIT_ACTIONS.lock().await;
    git::add_worktree(&root, &branch, base.as_deref())
        .await
        .map(|folder| view::display_path(&folder))
        .map_err(|error| error.to_string())
}

/// Closes a worktree project and deletes its folder; the branch is kept.
/// Without `force`, uncommitted changes make git refuse.
#[tauri::command]
async fn remove_worktree(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_id: String,
    force: bool,
) -> Result<(), String> {
    // Only the path: a live handle would keep the project's actor running.
    let path = state.project(&project_id)?.path;
    if git::worktree_of(&path).is_none() {
        return Err("this project is not a linked worktree".to_owned());
    }
    let _guard = GIT_ACTIONS.lock().await;
    // Stop the project's actor first so nothing holds the folder open.
    state
        .projects
        .lock()
        .map_err(|_| "app state poisoned")?
        .remove(&project_id);
    git::remove_worktree(&path, force)
        .await
        .map_err(|error| error.to_string())?;
    let mut settings = state.settings.lock().map_err(|_| "settings poisoned")?;
    settings
        .recent_projects
        .retain(|path| self::project_id(path) != project_id);
    settings.save(&app);
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
async fn submit_prompt(
    state: State<'_, AppState>,
    project_id: String,
    session_id: Option<String>,
    text: String,
    mode: Option<SubmitMode>,
    files: Option<Vec<String>>,
) -> Result<(), String> {
    let session_id = session_id.as_deref().map(parse_session).transpose()?;
    if text.len() > qq_protocol::MAX_INPUT_TEXT_BYTES {
        return Err("message exceeds the 128 KiB text limit".to_owned());
    }
    let files = files.unwrap_or_default();
    if files.len() > qq_protocol::MAX_INPUT_FILE_PARTS {
        return Err("attach at most eight workspace files".to_owned());
    }
    let path = state.project(&project_id)?.path;
    let permit = Arc::clone(&state.input_slots)
        .try_acquire_owned()
        .map_err(|_| "too many file resolutions; try again".to_owned())?;
    let input = tauri::async_runtime::spawn_blocking(move || {
        let _permit = permit;
        input::resolve(&path, &text, files)
    })
    .await
    .map_err(|_| "file mention resolution failed".to_owned())??;
    let (reply, receipt) = tokio::sync::oneshot::channel();
    state.send(
        &project_id,
        UiCommand::Submit {
            session_id,
            input,
            reply,
            mode: mode.unwrap_or_default(),
        },
    )?;
    match tokio::time::timeout(std::time::Duration::from_secs(65), receipt).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            Err("project closed before acknowledgement; check history before retrying".to_owned())
        }
        Err(_) => Err("acknowledgement timed out; check history before retrying".to_owned()),
    }
}

#[tauri::command]
fn cancel_run(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
) -> Result<(), String> {
    state.send(&project_id, UiCommand::Cancel(parse_session(&session_id)?))
}

/// Side questions are short by nature; the server applies its own limits.
const MAX_SIDE_QUESTION_BYTES: usize = 16 * 1024;

#[tauri::command]
fn submit_side_question(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    question: String,
    new_thread: bool,
) -> Result<(), String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("write a question first".to_owned());
    }
    if question.len() > MAX_SIDE_QUESTION_BYTES {
        return Err("side questions are limited to 16 KiB".to_owned());
    }
    state.send(
        &project_id,
        UiCommand::SideQuestion {
            session_id: parse_session(&session_id)?,
            question: question.to_owned(),
            new_thread,
        },
    )
}

#[tauri::command]
fn cancel_side_question(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
) -> Result<(), String> {
    state.send(
        &project_id,
        UiCommand::CancelSideQuestion(parse_session(&session_id)?),
    )
}

/// One commit, push or pull request at a time across the app; a second click
/// waits rather than racing the first.
static GIT_ACTIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tauri::command]
async fn git_status(
    state: State<'_, AppState>,
    project_id: String,
    with_pr: bool,
) -> Result<git::GitStatusDto, String> {
    let root = state.project(&project_id)?.path;
    git::status(&root, with_pr)
        .await
        .map_err(|error| error.to_string())
}

/// Stages everything and commits; pushes too when `push` is set. Returns the
/// short commit id.
#[tauri::command]
async fn git_commit(
    state: State<'_, AppState>,
    project_id: String,
    message: String,
    push: bool,
) -> Result<String, String> {
    let root = state.project(&project_id)?.path;
    let _guard = GIT_ACTIONS.lock().await;
    let id = git::commit(&root, &message)
        .await
        .map_err(|error| error.to_string())?;
    if push {
        git::push(&root)
            .await
            .map_err(|error| format!("committed {id}, but the push failed: {error}"))?;
    }
    Ok(id)
}

#[tauri::command]
async fn git_push(state: State<'_, AppState>, project_id: String) -> Result<(), String> {
    let root = state.project(&project_id)?.path;
    let _guard = GIT_ACTIONS.lock().await;
    git::push(&root).await.map_err(|error| error.to_string())
}

/// Pushes the branch and opens a pull request with `gh`. Returns its URL.
#[tauri::command]
async fn git_create_pr(
    state: State<'_, AppState>,
    project_id: String,
    title: String,
    body: String,
    draft: bool,
) -> Result<String, String> {
    let root = state.project(&project_id)?.path;
    let _guard = GIT_ACTIONS.lock().await;
    git::create_pr(&root, &title, &body, draft)
        .await
        .map_err(|error| error.to_string())
}

/// Waits for the server to accept or reject a command the actor sent.
async fn await_receipt(
    receipt: tokio::sync::oneshot::Receiver<Result<(), String>>,
) -> Result<(), String> {
    match tokio::time::timeout(std::time::Duration::from_secs(65), receipt).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the project closed before the server answered".to_owned()),
        Err(_) => {
            Err("the server did not answer in time; check the goal before retrying".to_owned())
        }
    }
}

/// The workspace's goal defaults and ceilings, read from its configuration.
#[tauri::command]
async fn goal_defaults(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<goals::GoalDefaultsDto, String> {
    let path = state.project(&project_id)?.path;
    tauri::async_runtime::spawn_blocking(move || goals::defaults(&path))
        .await
        .map_err(|_| "reading the goal configuration failed".to_owned())?
}

/// The goal identity a command acts on, as the UI saw it.
#[derive(serde::Deserialize)]
struct SeenGoal {
    goal_id: String,
    revision: u32,
}

impl SeenGoal {
    fn identity(&self) -> Result<qq_protocol::GoalIdentity, String> {
        Ok(qq_protocol::GoalIdentity {
            goal_id: self
                .goal_id
                .parse()
                .map_err(|_| format!("invalid goal id {}", self.goal_id))?,
            revision: self.revision,
        })
    }
}

/// Creates, edits or replaces a session's goal. Setting a goal does not
/// grant consent for its check command.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn set_goal(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    objective: String,
    check: Option<String>,
    preset: goals::BudgetPreset,
    expected: Option<SeenGoal>,
    replace: bool,
) -> Result<(), String> {
    let session_id = parse_session(&session_id)?;
    let objective = goals::objective(&objective)?;
    let expected = expected.as_ref().map(SeenGoal::identity).transpose()?;
    let path = state.project(&project_id)?.path;
    let defaults = tauri::async_runtime::spawn_blocking(move || goals::defaults(&path))
        .await
        .map_err(|_| "reading the goal configuration failed".to_owned())??;
    let check = goals::check(check.as_deref(), defaults.check_timeout_ms)?;
    let (reply, receipt) = tokio::sync::oneshot::channel();
    state.send(
        &project_id,
        UiCommand::SetGoal {
            session_id,
            objective,
            check,
            preset,
            defaults,
            expected,
            replace,
            reply,
        },
    )?;
    await_receipt(receipt).await
}

/// Pause, resume, clear, or answer check consent for the goal revision the
/// user saw; a newer revision is refused rather than acted on.
#[tauri::command]
async fn goal_control(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    expected: SeenGoal,
    action: goals::GoalAction,
) -> Result<(), String> {
    let (reply, receipt) = tokio::sync::oneshot::channel();
    state.send(
        &project_id,
        UiCommand::GoalControl {
            session_id: parse_session(&session_id)?,
            expected: expected.identity()?,
            action: action.into(),
            reply,
        },
    )?;
    await_receipt(receipt).await
}

/// Releases the workspace hold of an interrupted check whose uncertain
/// effects the user has resolved. Does not resume any goal.
#[tauri::command]
async fn acknowledge_check_interruption(
    state: State<'_, AppState>,
    project_id: String,
    run_id: String,
) -> Result<(), String> {
    let run_id = run_id
        .parse()
        .map_err(|_| format!("invalid run id {run_id}"))?;
    let (reply, receipt) = tokio::sync::oneshot::channel();
    state.send(
        &project_id,
        UiCommand::AcknowledgeCheckInterruption { run_id, reply },
    )?;
    await_receipt(receipt).await
}

/// Opens the project, or `path` inside it, in the first installed editor.
/// Returns the editor's name for the confirmation.
#[tauri::command]
fn open_in_editor(
    state: State<'_, AppState>,
    project_id: String,
    path: Option<String>,
) -> Result<&'static str, String> {
    let root = state.project(&project_id)?.path;
    editor::open(&root, path.as_deref()).map_err(|error| error.to_string())
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
fn set_session_option(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    option: SessionOption,
) -> Result<(), String> {
    state.send(
        &project_id,
        UiCommand::SetOption(parse_session(&session_id)?, option),
    )
}

#[tauri::command]
fn delete_session(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
) -> Result<(), String> {
    state.send(&project_id, UiCommand::Delete(parse_session(&session_id)?))
}

/// `@` completion for the composer: the TUI's ignore-aware, ranked and
/// deadline-bounded `complete_paths`, off the executor. A busy pool or a
/// failed walk yields no suggestions rather than an error, since completion
/// is advisory and the next keystroke asks again.
#[tauri::command]
async fn complete_mention(
    state: State<'_, AppState>,
    project_id: String,
    query: String,
) -> Result<Vec<String>, String> {
    if query.len() > MAX_COMPLETION_QUERY_BYTES {
        return Ok(Vec::new());
    }
    let path = state.project(&project_id)?.path;
    let Ok(permit) = Arc::clone(&state.input_slots).try_acquire_owned() else {
        return Ok(Vec::new());
    };
    Ok(tauri::async_runtime::spawn_blocking(move || {
        let _permit = permit;
        qq_core::mentions::complete_paths(&path, &query, &[])
    })
    .await
    .unwrap_or_default())
}

/// Longer queries cannot match a useful path and only cost a walk.
const MAX_COMPLETION_QUERY_BYTES: usize = 512;

#[tauri::command]
fn set_notifications_enabled(state: State<'_, AppState>, enabled: bool) {
    state
        .notifications_enabled
        .store(enabled, Ordering::Relaxed);
}

#[tauri::command]
fn session_action(
    state: State<'_, AppState>,
    project_id: String,
    session_id: Option<String>,
    action: SessionAction,
) -> Result<(), String> {
    state.send(
        &project_id,
        UiCommand::Action(
            session_id.as_deref().map(parse_session).transpose()?,
            action,
        ),
    )
}

/// Project files whose QQ configuration waits for the user's trust; empty
/// when the project has none or all of it is trusted.
#[tauri::command]
async fn project_trust(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<Vec<PendingFileDto>, TrustError> {
    let Ok(project) = state.project(&project_id) else {
        return Ok(Vec::new());
    };
    tauri::async_runtime::spawn_blocking(move || trust::pending(&project.path))
        .await
        .map_err(|_| TrustError::TaskFailed)?
}

/// Trust the project's pending files as `qq trust` does, then have the
/// project reload what the configuration now provides.
#[tauri::command]
async fn trust_project(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<Vec<PendingFileDto>, TrustError> {
    let Ok(project) = state.project(&project_id) else {
        return Ok(Vec::new());
    };
    let path = project.path.clone();
    let trusted = tauri::async_runtime::spawn_blocking(move || trust::grant(&path))
        .await
        .map_err(|_| TrustError::TaskFailed)??;
    let _ = project.send(UiCommand::RefreshModels);
    Ok(trusted)
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

#[tauri::command]
fn client_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else {
        "desktop"
    }
}

pub fn run() {
    let engine = Arc::new(Engine::default());
    let shutdown_engine = Arc::clone(&engine);
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            let paths: Vec<String> = args
                .into_iter()
                .skip(1)
                .filter(|arg| !arg.starts_with('-'))
                .filter_map(|arg| {
                    let path = std::path::Path::new(&cwd).join(arg);
                    std::fs::canonicalize(path)
                        .ok()
                        .filter(|path| path.is_dir())
                        .map(|path| view::display_path(&path))
                })
                .collect();
            if !paths.is_empty() {
                let _ = app.emit("open-paths", paths);
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // Size, position and maximized state persist across launches.
        // Visibility is left out so a first run, with nothing saved, shows the
        // window; decorations stay as configured (the app draws its own).
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(StateFlags::all() - StateFlags::VISIBLE - StateFlags::DECORATIONS)
                .build(),
        )
        .setup(move |app| {
            let settings = Settings::load(app.handle());
            app.manage(AppState {
                engine,
                attentive: Arc::new(AtomicBool::new(true)),
                projects: Mutex::new(HashMap::new()),
                settings: Mutex::new(settings),
                sign_ins: Arc::new(PendingSignIns::default()),
                input_slots: Arc::new(tokio::sync::Semaphore::new(8)),
                notifications_enabled: AtomicBool::new(false),
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
            client_platform,
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
            set_session_option,
            session_action,
            set_notifications_enabled,
            complete_mention,
            submit_side_question,
            cancel_side_question,
            open_in_editor,
            git_status,
            git_commit,
            git_push,
            git_create_pr,
            create_worktree,
            goal_defaults,
            set_goal,
            goal_control,
            acknowledge_check_interruption,
            remove_worktree,
            select_model,
            delete_session,
            auth_providers,
            auth_save_key,
            auth_sign_out,
            auth_browser_sign_in,
            project_trust,
            trust_project,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the QQ desktop app")
        .run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                shutdown_engine.shutdown();
            }
        });
}
