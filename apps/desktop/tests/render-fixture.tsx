// Dev-server-only fixture: no engine, credentials or model calls.
import { createRoot } from "react-dom/client";
import { lazy, Suspense, useEffect, useState, type ComponentType } from "react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { Sidebar } from "../src/components/Sidebar";
import { SessionPane } from "../src/components/SessionPane";
import { Settings } from "../src/components/Settings";
import { IconDialog } from "../src/components/ProjectIcon";
import { Search } from "../src/components/Search";
import type { Item, ProjectState } from "../src/lib/types";
import { getPreferences, setPreferences } from "../src/lib/preferences";
import { showSearch, showSettings, useApp, type SettingsPage } from "../src/lib/store";
import type { SearchTab } from "../src/lib/search";
import { setProjectIcon } from "../src/lib/project-icons";
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
// `?changes` / `?details`: open that side panel (saved like any preference).
if (new URLSearchParams(location.search).has("changes"))
  setPreferences({ ...getPreferences(), sidePanel: "changes" });
if (new URLSearchParams(location.search).has("details")) {
  setPreferences({ ...getPreferences(), sidePanel: "details" });
  const transcript = project.transcript!;
  transcript.context_tokens = 48_600;
  transcript.cost_usd_nanos = 184_000_000;
  transcript.reasoning_effort = "high";
  for (const item of transcript.items)
    if (item.kind === "run_end" && item.id === "e1")
      item.usage = {
        input_tokens: 41_200,
        cache_read_input_tokens: 96_400,
        cache_write_input_tokens: 8_100,
        output_tokens: 6_900,
        reasoning_tokens: 2_300,
      };
  project.tool_hosts = [
    { name: "github", generation: 1, tool_count: 26, ready: true },
    { name: "linear", generation: 1, tool_count: 31, ready: true },
    {
      name: "figma",
      generation: 1,
      tool_count: 0,
      ready: false,
      message: "Server exited: missing FIGMA_TOKEN",
    },
  ];
}
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
  const { settings, search } = useApp();
  useEffect(() => {
    const page = new URLSearchParams(location.search).get("settings");
    if (page) showSettings(page as SettingsPage);
    const tab = new URLSearchParams(location.search).get("search");
    if (tab) showSearch(tab as SearchTab);
  }, []);
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
    if (command === "auth_providers")
      return ["anthropic", "openai-codex", "openai"].map((id, index) => ({
        id,
        stored: index === 1 ? "keyring" : null,
        environment: null,
        accepts_key: id !== "openai-codex",
        browser_sign_in: id === "openai-codex",
        recommended_model: "fixture/model",
      }));
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
          app={{
            open: [
              {
                id: project.project_id,
                path: project.path,
                name: project.name,
                worktree: null,
              },
              ...extra.map((other) => ({
                id: other.project_id,
                path: other.path,
                name: other.name,
                worktree: null,
              })),
            ],
            projects: {
              [project.project_id]: project,
              ...Object.fromEntries(extra.map((other) => [other.project_id, other])),
            },
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
          onClick={() => showSettings()}
        >
          Fixture settings
        </button>
      )}
      {settings && <Settings page={settings} project={project} />}
      {search && (
        <Search
          tab={search}
          project={project}
          app={{
            open: [
              { id: project.project_id, path: project.path, name: project.name, worktree: null },
              ...extra.map((other) => ({ id: other.project_id, path: other.path, name: other.name, worktree: null })),
            ],
            recents: [{ id: "recent", path: "C:/temporary/old-prototype", name: "old-prototype", worktree: null }],
            projects: {
              [project.project_id]: project,
              ...Object.fromEntries(extra.map((other) => [other.project_id, other])),
            },
          }}
        />
      )}
      {extra.length > 0 && new URLSearchParams(location.search).has("icon") && (
        <IconDialog
          project={{ id: extra[0].project_id, path: extra[0].path, name: extra[0].name, worktree: null }}
          icon={
            new URLSearchParams(location.search).get("icon") === "emoji"
              ? { kind: "emoji", color: "orange", emoji: "🚀" }
              : { kind: "symbol", color: "teal", symbol: "server" }
          }
          close={() => undefined}
        />
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
// `?projects` opens more projects, with custom icons, beside the fixture.
const extra: ProjectState[] = new URLSearchParams(location.search).has("projects")
  ? (["api-gateway", "design-system", "notes-app"] as const).map((name, index) => ({
      ...project,
      project_id: `fixture-${name}`,
      name,
      path: `C:/temporary/${name}`,
      focused: null,
      transcript: null,
      sessions: project.sessions.slice(0, 3 + index).map((row, offset) => ({
        ...row,
        id: `${name}-${offset}`,
        title: ["Fix flaky retry test", "Review the auth flow", "Tidy the README", "Add dark mode", "Profile startup"][offset],
        group: index === 0 && offset === 0 ? "needs_you" : row.group,
      })),
    }))
  : [];
if (extra.length) {
  setProjectIcon(extra[0].path, { kind: "symbol", color: "teal", symbol: "server" });
  setProjectIcon(extra[1].path, { kind: "initials", color: "violet" });
  setProjectIcon(extra[2].path, { kind: "emoji", color: "orange", emoji: "📝" });
}
createRoot(document.getElementById("root")!).render(<Fixture />);
