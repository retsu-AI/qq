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
import type { GitStatusDto, GoalDefaultsDto } from "./generated";
import type { BudgetPreset, GoalAction, SeenGoal } from "./goals";

export const api = {
  sessionAction: (
    projectId: string,
    sessionId: string | null,
    action: "compact" | "rollback" | "prune" | "child",
  ) => invoke<void>("session_action", { projectId, sessionId, action }),
  completeMention: (projectId: string, query: string) =>
    invoke<string[]>("complete_mention", { projectId, query }),
  submitSideQuestion: (
    projectId: string,
    sessionId: string,
    question: string,
    newThread: boolean,
  ) =>
    invoke<void>("submit_side_question", {
      projectId,
      sessionId,
      question,
      newThread,
    }),
  cancelSideQuestion: (projectId: string, sessionId: string) =>
    invoke<void>("cancel_side_question", { projectId, sessionId }),
  gitStatus: (projectId: string, withPr: boolean) =>
    invoke<GitStatusDto>("git_status", { projectId, withPr }),
  /** Stages everything and commits; resolves to the short commit id. */
  gitCommit: (projectId: string, message: string, push: boolean) =>
    invoke<string>("git_commit", { projectId, message, push }),
  gitPush: (projectId: string) => invoke<void>("git_push", { projectId }),
  /** Resolves to the new worktree's folder. */
  createWorktree: (projectId: string, branch: string, base: string | null) =>
    invoke<string>("create_worktree", { projectId, branch, base }),
  removeWorktree: (projectId: string, force: boolean) =>
    invoke<void>("remove_worktree", { projectId, force }),
  /** Pushes and opens a pull request; resolves to its URL. */
  gitCreatePr: (projectId: string, title: string, body: string, draft: boolean) =>
    invoke<string>("git_create_pr", { projectId, title, body, draft }),
  goalDefaults: (projectId: string) =>
    invoke<GoalDefaultsDto>("goal_defaults", { projectId }),
  /** Creates, edits or replaces a goal; resolves once the server accepts it. */
  setGoal: (
    projectId: string,
    sessionId: string,
    goal: {
      objective: string;
      check: string | null;
      preset: BudgetPreset;
      expected: SeenGoal | null;
      replace: boolean;
    },
  ) => invoke<void>("set_goal", { projectId, sessionId, ...goal }),
  goalControl: (
    projectId: string,
    sessionId: string,
    expected: SeenGoal,
    action: GoalAction,
  ) => invoke<void>("goal_control", { projectId, sessionId, expected, action }),
  acknowledgeCheckInterruption: (projectId: string, runId: string) =>
    invoke<void>("acknowledge_check_interruption", { projectId, runId }),
  /** Resolves to the editor's name. `path` is project-relative. */
  openInEditor: (projectId: string, path?: string) =>
    invoke<string>("open_in_editor", { projectId, path: path ?? null }),
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
