// Dev-server-only fixture: no engine, credentials or model calls.
import { createRoot } from "react-dom/client";
import { lazy, Suspense, useEffect, useState, type ComponentType } from "react";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import { Sidebar } from "../src/components/Sidebar";
import { SessionPane } from "../src/components/SessionPane";
import { Settings } from "../src/components/Settings";
import type { Item, ProjectState } from "../src/lib/types";
import { getPreferences, setPreferences } from "../src/lib/preferences";
import { openGoalDialog } from "../src/lib/goal-ui";
import { showWorktreeDialog } from "../src/lib/worktree-dialog";
import { showProviders, start } from "../src/lib/store";
import "../src/styles.css";

const count = Math.min(
  8192,
  Math.max(1, Number(new URLSearchParams(location.search).get("rows") ?? 2048)),
);
const running = new URLSearchParams(location.search).has("running");
const baseline = new URLSearchParams(location.search).has("baseline");
const baselinePath = "/src-tauri/target/qq-perf/baseline.tsx";
const Baseline = lazy<
  ComponentType<{
    project: ProjectState;
    transcript: NonNullable<ProjectState["transcript"]>;
  }>
>(() =>
  import(/* @vite-ignore */ baselinePath).then((module) => ({
    default: module.Transcript,
  })),
);
const started = performance.now();
const project: ProjectState = {
  project_id: "render-fixture",
  name: "Throwaway rendering fixture",
  path: "C:/temporary/qq-render-fixture",
  connection: "live",
  error: null,
  owns_server: true,
  models: [
    {
      id: "fixture/model",
      provider: "fixture",
      model: "model",
      name: "Fixture model",
      context_window: 128000,
      reasoning_efforts: ["low", "high"],
    },
  ],
  selected_model: "fixture/model",
  notice: null,
  can_steer: true,
  can_interrupt: true,
  profiles: ["default", "reviewer"],
  tool_hosts: [],
  skills: [
    { name: "ship", kind: "command", description: "Cut a release branch" },
    { name: "review", kind: "skill", description: "Repository review checklist" },
  ],
  can_side_question: true,
  can_goal: true,
  check_interruption: null,
  focused: "s0",
  sessions: Array.from({ length: 512 }, (_, index) => ({
    id: `s${index}`,
    parent_id: null,
    depth: 0,
    title: `Task ${index}`,
    group: "idle",
    status: "idle",
    activity: null,
    active_tool: null,
    tail: "Rendering fixture",
    unread: 0,
    awaiting_approval: 0,
    queued_prompts: 0,
    model: "fixture/model",
    updated_at_ms: index,
    failed: false,
  })),
  transcript: {
    session_id: "s0",
    title: "Fixture conversation",
    loading: false,
    status: running ? "running" : "idle",
    activity: running ? "generating_response" : null,
    active_run_id: running ? "run1" : null,
    model: "fixture/model",
    approval_mode: "ask",
    profile: "default",
    reasoning_effort: null,
    approval_delegate: null,
    context_tokens: 2048,
    context_window: 128000,
    cost_usd_nanos: 0,
    queued_prompts: 0,
    side_questions: [],
    goal: null,
    items: Array.from({ length: count }, (_, index) => ({
      kind: "assistant",
      id: `m${index}`,
      text: `Message ${index}\n\nA bounded transcript should keep **earlier history** reachable.\n\n\`const result = ${index};\``,
      refusal: "",
      state: "complete",
      truncated: false,
    })),
  },
};
// `?demo`: one realistic conversation (tools, an edit, code and a failed run)
// in place of the generated rows.
if (new URLSearchParams(location.search).has("demo")) {
  const tool = (
    id: string,
    name: string,
    args: object,
    extra: Partial<Extract<Item, { kind: "tool" }>> = {},
  ): Item => ({
    kind: "tool",
    id,
    run_id: "run1",
    name,
    arguments: JSON.stringify(args),
    state: "completed",
    result: "ok",
    is_error: false,
    diff: null,
    live_output: null,
    duration_ms: 420,
    settled: null,
    child_session: null,
    approval: null,
    ...extra,
  });
  const edit = [
    "--- a/src/lib/retry.rs",
    "+++ b/src/lib/retry.rs",
    "@@ -10,7 +10,9 @@ use std::time::Duration;",
    " pub fn backoff(attempt: u32) -> Duration {",
    "-    Duration::from_millis(100 * attempt as u64)",
    "+    // Exponential, capped at five seconds.",
    "+    let millis = 100u64.saturating_mul(1 << attempt.min(6));",
    "+    Duration::from_millis(millis.min(5_000))",
    " }",
    " ",
    " #[cfg(test)]",
  ].join("\n");
  project.transcript!.goal = {
    session_id: "s0",
    goal_id: "0123456789abcdef0123456789abcdef",
    revision: 2,
    objective: "Make retry backoff exponential, capped at 5 s, with tests.",
    check: { command: "cargo test -p retry", timeout_ms: 600000 },
    check_authorization: "pending",
    budget: { max_goal_runs: 20, deadline_ms: Date.now() + 50 * 60_000 },
    checklist: [
      { id: 1, text: "Find the backoff helper", state: "done" },
      { id: 2, text: "Cap the delay at five seconds", state: "done" },
      { id: 3, text: "Add tests for the cap", state: "in_progress" },
      { id: 4, text: "Tune jitter", state: "dropped" },
    ],
    notes: "Callers rely on the first retry staying at 100 ms.",
    status: {
      type: "paused",
      reason: { type: "needs_user", cause: { type: "check_approval" } },
    },
    counters: {
      runs: 3,
      usage: {
        input_tokens: 41000,
        cache_read_input_tokens: 9000,
        cache_write_input_tokens: 0,
        output_tokens: 3200,
      },
      estimated_cost_usd_nanos: 210_000_000,
      audits: 0,
      check_failures: 0,
      no_progress_streak: 0,
      failure_streak: 0,
    },
    next_run_at_ms: 0,
    wait: null,
    audit_pending: false,
    audit_revision: null,
    pending_run_id: null,
    check_output: null,
  };
  project.transcript!.side_questions = [
    {
      id: "sq1",
      thread_id: "sq1",
      question: "Which file defines the retry limit?",
      answer: "`src/lib/retry.rs`, in `backoff`. The cap is `5_000` ms.",
      state: "completed",
    },
  ];
  project.transcript!.items = [
    {
      kind: "user",
      id: "u1",
      text: "Make the retry backoff exponential and cap it.",
      state: "complete",
      steering: false,
    },
    {
      kind: "reasoning",
      id: "r1",
      text: "Find the backoff helper, then check the tests.",
      streaming: false,
    },
    tool("t1", "search", { pattern: "fn backoff" }),
    tool("t2", "read_file", { path: "src/lib/retry.rs" }),
    tool("t3", "shell", { command: "cargo test -p retry" }),
    tool("t4", "edit_file", { path: "src/lib/retry.rs" }, {
      diff: { path: "src/lib/retry.rs", diff: edit },
    }),
    tool("t5", "shell", { command: "cargo test -p retry" }),
    tool("t6", "shell", { command: "cargo clippy -p retry" }, {
      is_error: true,
      result: "warning: unused import",
    }),
    {
      kind: "assistant",
      id: "a1",
      text: [
        "Backoff is now exponential and capped at **5 s**:",
        "",
        "```rust",
        "pub fn backoff(attempt: u32) -> Duration {",
        "    // Exponential, capped at five seconds.",
        "    let millis = 100u64.saturating_mul(1 << attempt.min(6));",
        '    Duration::from_millis(millis.min(5_000)) // "ms"',
        "}",
        "```",
        "",
        "Call it from TypeScript with `await retry(3)`:",
        "",
        "```ts",
        "export async function retry(times: number): Promise<void> {",
        "  for (let i = 0; i < times; i++) await sleep(2 ** i * 100);",
        "}",
        "```",
      ].join("\n"),
      refusal: "",
      state: "complete",
      truncated: false,
    },
    {
      kind: "run_end",
      id: "e1",
      outcome: { type: "completed" },
      duration_ms: 18_400,
      usage: null,
      cost_usd_nanos: null,
      tool_calls: 6,
    },
    {
      kind: "user",
      id: "u2",
      text: "Now update the docs.",
      state: "complete",
      steering: false,
    },
    {
      kind: "run_end",
      id: "e2",
      outcome: {
        type: "failed",
        failure: { kind: "provider", message: "The provider closed the stream." },
      },
      duration_ms: 2_100,
      usage: null,
      cost_usd_nanos: null,
      tool_calls: 0,
    },
  ];
}
// `?changes`: open the Changes panel (saved like any preference).
if (new URLSearchParams(location.search).has("changes"))
  setPreferences({ ...getPreferences(), changesPanel: true });
if (new URLSearchParams(location.search).has("approval"))
  project.transcript!.items.push({
    kind: "tool",
    id: "held-fixture",
    run_id: "run1",
    name: "shell",
    arguments: '{"command":"echo fixture"}',
    state: "awaiting_approval",
    result: null,
    is_error: false,
    diff: null,
    live_output: null,
    duration_ms: null,
    settled: null,
    child_session: null,
    approval: {
      shell: { command: "echo fixture", verdict: "prompt" },
      edit: null,
      question: null,
      fetch: null,
      escalation: null,
      grantable: true,
      grant_label: "echo fixture",
    },
  });
function Fixture() {
  const [commands, setCommands] = useState<string[]>([]);
  const [measurement, setMeasurement] = useState("");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [focusResult, setFocusResult] = useState("");
  useEffect(() => {
    let recorded = false;
    const measure = () => {
      const mounted = document.querySelectorAll(
        baseline
          ? "[data-baseline] .space-y-5 > div"
          : '[aria-label="Transcript"] > [role="listitem"]',
      ).length;
      if (recorded || mounted === 0) return;
      recorded = true;
      requestAnimationFrame(() =>
        requestAnimationFrame(() =>
          setMeasurement(
            JSON.stringify({
              baseline,
              rows: count,
              mounted,
              firstRenderMs:
                Math.round((performance.now() - started) * 100) / 100,
            }),
          ),
        ),
      );
    };
    const observer = new MutationObserver(measure);
    observer.observe(document.getElementById("root")!, {
      childList: true,
      subtree: true,
    });
    measure();
    return () => observer.disconnect();
  }, []);
  mockIPC(async (command, args) => {
    setCommands((values) => [
      ...values.slice(-9),
      JSON.stringify({ command, args }),
    ]);
    if (
      new URLSearchParams(location.search).has("reject") &&
      command === "submit_prompt"
    )
      throw new Error("Fixture submission rejected");
    if (
      command === "submit_prompt" &&
      new URLSearchParams(location.search).has("focus-check")
    ) {
      let retained = true;
      await new Promise<void>((resolve) => {
        let frames = 0;
        const check = () => {
          retained &&=
            document.activeElement?.getAttribute("aria-label") === "Message";
          if (++frames < 8) requestAnimationFrame(check);
          else resolve();
        };
        requestAnimationFrame(check);
      });
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          const focused =
            document.activeElement?.getAttribute("aria-label") === "Message";
          setFocusResult(
            retained && focused
              ? "PASS: composer retained focus during and after acknowledgement"
              : "FAIL: composer lost keyboard focus",
          );
        }),
      );
    }
    return undefined;
  });
  return (
    <>
      <div className="flex h-full">
        <Sidebar
          openSettings={() => undefined}
          app={{
            open: [
              {
                id: project.project_id,
                path: project.path,
                name: project.name,
                worktree: null,
              },
            ],
            projects: { [project.project_id]: project },
            activeId: project.project_id,
          }}
          project={project}
        />
        <main className="flex min-w-0 flex-1 flex-col">
          {baseline ? (
            <div data-baseline className="overflow-y-auto p-6">
              <Suspense fallback="Loading baseline">
                <Baseline project={project} transcript={project.transcript!} />
              </Suspense>
            </div>
          ) : (
            <SessionPane project={project} pendingTrust={[]} />
          )}
        </main>
      </div>
      {new URLSearchParams(location.search).has("approval") && (
        <button
          className="fixed left-2 top-2 bg-raised p-2"
          onClick={() => setSettingsOpen(true)}
        >
          Fixture settings
        </button>
      )}
      {settingsOpen && (
        <Settings project={project} close={() => setSettingsOpen(false)} />
      )}
      <output
        aria-label="Fixture commands"
        className="fixed bottom-0 right-0 max-w-[600px] truncate bg-bg text-[10px]"
      >
        {commands.join("\n")}
      </output>
      <output
        aria-label="Rendering measurement"
        className="fixed right-0 top-0 bg-bg text-[10px]"
      >
        {measurement}
      </output>
      <output
        aria-label="Composer focus regression"
        className="fixed left-0 top-0 bg-bg text-[12px]"
      >
        {focusResult}
      </output>
    </>
  );
}
// `?app`: the real App (title bar, store, dialogs) over mocked IPC and events.
// `welcome` opens no project, `empty` shows a new session's starters,
// `signed-out` has no models, `trust` holds the project's configuration,
// `deny-links` refuses external links as a missing opener scope does, and
// `open=palette|providers|worktree|goal|menu-file` opens that overlay.
async function renderApp() {
  const query = new URLSearchParams(location.search);
  if (query.has("empty")) {
    project.focused = null;
    project.transcript = null;
  }
  if (query.has("signed-out")) project.models = [];
  const info = {
    id: project.project_id,
    path: project.path,
    name: project.name,
    worktree: null,
  };
  mockWindows("main");
  mockIPC(
    async (command, args) => {
      switch (command) {
        case "plugin:opener|open_url":
          if (query.has("deny-links"))
            throw `Not allowed to open url ${(args as { url: string }).url}`;
          return undefined;
        case "plugin:window|is_maximized":
          return false;
        case "plugin:app|version":
          return "0.0.0-fixture";
        case "recent_projects":
          return [
            info,
            { ...info, id: "other", path: "C:/temporary/api", name: "api" },
            {
              id: "wt",
              path: "C:/temporary/qq-render-fixture-retry",
              name: "qq-render-fixture-retry",
              worktree: {
                branch: "retry-backoff",
                main_path: project.path,
                main_name: project.name,
              },
            },
          ];
        case "launch_paths":
          return [];
        case "open_project":
          return info;
        case "refresh":
          await emit("project-state", project);
          return undefined;
        case "project_trust":
          return query.has("trust")
            ? [
                {
                  path: ".qq/config.ron",
                  sections: ["mcp_servers", "policy"],
                  declarations: ["mcp_servers.github", "policy.shell.allow"],
                },
              ]
            : [];
        case "git_status":
          return {
            repo: true,
            branch: "retry-backoff",
            upstream: "origin/retry-backoff",
            ahead: 2,
            behind: 0,
            changed: 3,
            conflicted: 0,
            pr: {
              number: 42,
              state: "OPEN",
              draft: true,
              title: "Exponential retry backoff",
              url: "https://example.invalid/pull/42",
            },
            gh: true,
          };
        case "goal_defaults":
          return {
            duration_ms: 3_600_000,
            max_duration_ms: 86_400_000,
            default_runs: 20,
            max_runs: 200,
            check_timeout_ms: 600_000,
          };
        case "auth_providers":
          return [
            {
              id: "anthropic",
              stored: "keyring",
              environment: null,
              accepts_key: true,
              browser_sign_in: true,
              recommended_model: "anthropic/fixture",
            },
            {
              id: "openai-codex",
              stored: null,
              environment: null,
              accepts_key: false,
              browser_sign_in: true,
              recommended_model: "openai-codex/fixture",
            },
            {
              id: "openai",
              stored: null,
              environment: "OPENAI_API_KEY",
              accepts_key: true,
              browser_sign_in: false,
              recommended_model: "openai/fixture",
            },
            {
              id: "google",
              stored: null,
              environment: null,
              accepts_key: true,
              browser_sign_in: false,
              recommended_model: "google/fixture",
            },
          ];
        case "complete_mention":
          return ["src/lib/retry.rs", "src/lib/mod.rs", "README.md"];
        default:
          return undefined;
      }
    },
    { shouldMockEvents: true },
  );
  try {
    localStorage.setItem(
      "qq.openProjects",
      JSON.stringify(query.has("welcome") ? [] : [project.path]),
    );
  } catch {
    // Without storage the fixture opens the welcome screen.
  }
  // Imported after the mocks: the title bar resolves its window on import.
  const { App } = await import("../src/App");
  createRoot(document.getElementById("root")!).render(<App />);
  await start();
  // After the first commit, so the title bar's key handler is listening.
  await new Promise((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(resolve)),
  );
  const opened = query.get("open");
  if (opened === "palette" || opened?.startsWith("menu-"))
    window.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: opened === "palette" ? "k" : opened.slice(5, 6),
        ctrlKey: opened === "palette",
        altKey: opened !== "palette",
      }),
    );
  else if (opened === "providers") showProviders(true);
  else if (opened === "worktree") showWorktreeDialog(project.project_id);
  else if (opened === "goal")
    openGoalDialog({
      projectId: project.project_id,
      sessionId: project.focused ?? "s0",
      objective: "Make retry backoff exponential, capped at 5 s, with tests.",
      edit: false,
    });
}

if (new URLSearchParams(location.search).has("app")) void renderApp();
else createRoot(document.getElementById("root")!).render(<Fixture />);
