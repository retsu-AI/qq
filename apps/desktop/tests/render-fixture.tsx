// Dev-server-only fixture: no engine, credentials or model calls.
import { createRoot } from "react-dom/client";
import { lazy, Suspense, useEffect, useState, type ComponentType } from "react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { Sidebar } from "../src/components/Sidebar";
import { SessionPane } from "../src/components/SessionPane";
import { Settings } from "../src/components/Settings";
import type { ProjectState } from "../src/lib/types";
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
          app={{
            open: [
              {
                id: project.project_id,
                path: project.path,
                name: project.name,
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
createRoot(document.getElementById("root")!).render(<Fixture />);
