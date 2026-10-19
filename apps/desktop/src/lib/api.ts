// Typed wrappers over the Tauri commands in `src-tauri/src/main.rs`.
import { invoke } from "@tauri-apps/api/core";
import type {
  ApprovalChoice,
  ApprovalMode,
  BrowserSignIn,
  PendingFile,
  ProjectInfo,
  Provider,
} from "./types";

export const api = {
  sessionAction: (
    projectId: string,
    sessionId: string | null,
    action: "compact" | "rollback" | "prune" | "child",
  ) => invoke<void>("session_action", { projectId, sessionId, action }),
  completeMention: (projectId: string, query: string) =>
    invoke<string[]>("complete_mention", { projectId, query }),
  setNotificationsEnabled: (enabled: boolean) =>
    invoke<void>("set_notifications_enabled", { enabled }),
  launchPaths: () => invoke<string[]>("launch_paths"),
  recentProjects: () => invoke<ProjectInfo[]>("recent_projects"),
  openProject: (path: string) => invoke<ProjectInfo>("open_project", { path }),
  closeProject: (projectId: string) =>
    invoke<void>("close_project", { projectId }),
  forgetProject: (projectId: string) =>
    invoke<void>("forget_project", { projectId }),
  refresh: (projectId: string) => invoke<void>("refresh", { projectId }),
  focusSession: (projectId: string, sessionId: string | null) =>
    invoke<void>("focus_session", { projectId, sessionId }),
  newSession: (projectId: string) => invoke<void>("new_session", { projectId }),
  submitPrompt: (
    projectId: string,
    sessionId: string | null,
    text: string,
    mode = "queue",
    files: string[] = [],
  ) =>
    invoke<void>("submit_prompt", { projectId, sessionId, text, mode, files }),
  cancelRun: (projectId: string, sessionId: string) =>
    invoke<void>("cancel_run", { projectId, sessionId }),
  respondApproval: (
    projectId: string,
    sessionId: string,
    toolCallId: string,
    decision: ApprovalChoice,
  ) =>
    invoke<void>("respond_approval", {
      projectId,
      sessionId,
      toolCallId,
      decision,
    }),
  setApprovalMode: (projectId: string, sessionId: string, mode: ApprovalMode) =>
    invoke<void>("set_approval_mode", { projectId, sessionId, mode }),
  setSessionOption: (
    projectId: string,
    sessionId: string,
    type: "profile" | "effort" | "delegate",
    value: string | null,
  ) =>
    invoke<void>("set_session_option", {
      projectId,
      sessionId,
      option: { type, value },
    }),
  selectModel: (projectId: string, model: string) =>
    invoke<void>("select_model", { projectId, model }),
  deleteSession: (projectId: string, sessionId: string) =>
    invoke<void>("delete_session", { projectId, sessionId }),
  projectTrust: (projectId: string) =>
    invoke<PendingFile[]>("project_trust", { projectId }),
  trustProject: (projectId: string) =>
    invoke<PendingFile[]>("trust_project", { projectId }),
  authProviders: () => invoke<Provider[]>("auth_providers"),
  authSaveKey: (provider: string, key: string) =>
    invoke<void>("auth_save_key", { provider, key }),
  authSignOut: (provider: string) =>
    invoke<void>("auth_sign_out", { provider }),
  authBrowserSignIn: (provider: string) =>
    invoke<BrowserSignIn>("auth_browser_sign_in", { provider }),
};
