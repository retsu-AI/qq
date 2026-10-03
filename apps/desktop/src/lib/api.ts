// Typed wrappers over the Tauri commands in `src-tauri/src/main.rs`.
import { invoke } from "@tauri-apps/api/core";
import type { ApprovalChoice, ApprovalMode, BrowserSignIn, ProjectInfo, Provider } from "./types";

export const api = {
  launchPaths: () => invoke<string[]>("launch_paths"),
  recentProjects: () => invoke<ProjectInfo[]>("recent_projects"),
  openProject: (path: string) => invoke<ProjectInfo>("open_project", { path }),
  closeProject: (projectId: string) => invoke<void>("close_project", { projectId }),
  forgetProject: (projectId: string) => invoke<void>("forget_project", { projectId }),
  refresh: (projectId: string) => invoke<void>("refresh", { projectId }),
  focusSession: (projectId: string, sessionId: string | null) =>
    invoke<void>("focus_session", { projectId, sessionId }),
  newSession: (projectId: string) => invoke<void>("new_session", { projectId }),
  submitPrompt: (projectId: string, sessionId: string | null, text: string) =>
    invoke<void>("submit_prompt", { projectId, sessionId, text }),
  cancelRun: (projectId: string, sessionId: string) =>
    invoke<void>("cancel_run", { projectId, sessionId }),
  respondApproval: (
    projectId: string,
    sessionId: string,
    toolCallId: string,
    decision: ApprovalChoice,
  ) => invoke<void>("respond_approval", { projectId, sessionId, toolCallId, decision }),
  setApprovalMode: (projectId: string, sessionId: string, mode: ApprovalMode) =>
    invoke<void>("set_approval_mode", { projectId, sessionId, mode }),
  selectModel: (projectId: string, model: string) =>
    invoke<void>("select_model", { projectId, model }),
  deleteSession: (projectId: string, sessionId: string) =>
    invoke<void>("delete_session", { projectId, sessionId }),
  authProviders: () => invoke<Provider[]>("auth_providers"),
  authSaveKey: (provider: string, key: string) => invoke<void>("auth_save_key", { provider, key }),
  authSignOut: (provider: string) => invoke<void>("auth_sign_out", { provider }),
  authBrowserSignIn: (provider: string) =>
    invoke<BrowserSignIn>("auth_browser_sign_in", { provider }),
};
