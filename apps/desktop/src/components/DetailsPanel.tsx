import { useMemo } from "react";
import clsx from "clsx";
import {
  Bot,
  ChevronRight,
  FileDiff,
  Folder,
  GitBranch,
  Plug,
  Settings as SettingsIcon,
  ShieldCheck,
  X,
} from "lucide-react";
import { showSettings } from "../lib/store";
import { useProjectIcons } from "../lib/project-icons";
import { cost, modelLabel, tokens } from "../lib/format";
import { contextPercent, sessionUsage } from "../lib/usage";
import type { FileChange } from "../lib/changes";
import type { ProjectState, Transcript } from "../lib/types";
import { CopyButton } from "./CodeBlock";
import { MODES } from "./Composer";
import { useGitStatus } from "./GitStrip";
import { ProjectBadge } from "./ProjectIcon";

/**
 * The session at a glance: where it runs, how full its context is, what it
 * has used, its MCP servers, and a way into its file changes.
 */
export function DetailsPanel({
  project,
  transcript,
  changes,
  showChanges,
  close,
}: {
  project: ProjectState;
  transcript: Transcript;
  changes: FileChange[];
  showChanges: () => void;
  close: () => void;
}) {
  return (
    <aside
      aria-label="Session details"
      // Beside the conversation on wide windows, over it on narrow ones.
      className="animate-slide-in flex w-[min(340px,36vw)] min-w-[280px] shrink-0 flex-col border-l border-line bg-sidebar max-[1100px]:absolute max-[1100px]:inset-y-0 max-[1100px]:right-0 max-[1100px]:z-20 max-[1100px]:w-[min(340px,85%)] max-[1100px]:shadow-2xl"
    >
      <header className="flex h-12 shrink-0 items-center border-b border-line pl-4 pr-2">
        <h2 className="flex-1 text-body font-medium">Details</h2>
        <button
          onClick={close}
          aria-label="Close details"
          title="Close (Ctrl Shift D)"
          className="rounded-md p-1.5 text-muted hover:bg-hover hover:text-fg"
        >
          <X className="h-4 w-4" />
        </button>
      </header>
      <div className="min-h-0 flex-1 divide-y divide-line overflow-y-auto px-4">
        <Environment project={project} transcript={transcript} />
        <Context transcript={transcript} />
        <Connectors project={project} />
      </div>
      <Changes changes={changes} showChanges={showChanges} />
    </aside>
  );
}

function Section({
  title,
  aside,
  children,
}: {
  title: string;
  aside?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section className="py-4">
      <div className="mb-2.5 flex items-baseline justify-between gap-2">
        <h3 className="text-ui font-medium">{title}</h3>
        {aside && <span className="text-small text-faint">{aside}</span>}
      </div>
      {children}
    </section>
  );
}

function Row({
  icon,
  title,
  children,
  action,
}: {
  icon: React.ReactNode;
  title?: string;
  children: React.ReactNode;
  action?: React.ReactNode;
}) {
  return (
    <div className="group/row flex h-8 items-center gap-2.5 text-ui">
      <span className="flex w-5 shrink-0 justify-center text-muted">{icon}</span>
      <span className="min-w-0 flex-1 truncate" title={title}>
        {children}
      </span>
      {action}
    </div>
  );
}

function Environment({
  project,
  transcript,
}: {
  project: ProjectState;
  transcript: Transcript;
}) {
  const icons = useProjectIcons();
  const { status } = useGitStatus(
    project.project_id,
    `${transcript.session_id}:${transcript.status}`,
    false,
  );
  const mode = MODES.find((m) => m.value === transcript.approval_mode);
  const effort = transcript.reasoning_effort;
  return (
    <Section title="Environment">
      <Row
        icon={
          <ProjectBadge
            project={{ name: project.name, worktree: null }}
            icon={icons[project.path]}
            size="sm"
          />
        }
        action={
          <button
            onClick={() => showSettings()}
            aria-label="Settings"
            title="Settings (Ctrl ,)"
            className="rounded-md p-1 text-muted hover:bg-hover hover:text-fg"
          >
            <SettingsIcon className="h-3.5 w-3.5" />
          </button>
        }
      >
        <span className="font-medium">{project.name}</span>
      </Row>
      <Row
        icon={<Folder className="h-3.5 w-3.5" />}
        title={project.path}
        action={
          <CopyButton
            text={project.path}
            label="Copy path"
            className="opacity-0 focus-visible:opacity-100 group-hover/row:opacity-100"
          />
        }
      >
        {/* Long paths keep their end, which names the folder. */}
        <span className="block truncate text-muted [direction:rtl] [text-align:left]">
          <bdi>{project.path}</bdi>
        </span>
      </Row>
      {status?.repo && (
        <Row icon={<GitBranch className="h-3.5 w-3.5" />}>
          {status.branch ?? "Detached HEAD"}
          {(status.ahead > 0 || status.behind > 0) && (
            <span className="ml-2 text-small text-faint">
              {status.ahead > 0 && `↑${status.ahead}`}
              {status.ahead > 0 && status.behind > 0 && " "}
              {status.behind > 0 && `↓${status.behind}`}
            </span>
          )}
        </Row>
      )}
      <Row icon={<Bot className="h-3.5 w-3.5" />} title={transcript.model ?? undefined}>
        {modelLabel(transcript.model, project.models)}
        {effort && (
          <span className="ml-2 text-small text-faint">
            {effort[0].toUpperCase() + effort.slice(1)} effort
          </span>
        )}
      </Row>
      <Row icon={<ShieldCheck className="h-3.5 w-3.5" />} title={mode?.hint}>
        {mode?.label ?? transcript.approval_mode}
        <span className="ml-2 text-small text-faint">approvals</span>
      </Row>
    </Section>
  );
}

function Context({ transcript }: { transcript: Transcript }) {
  const usage = useMemo(() => sessionUsage(transcript.items), [transcript.items]);
  const used = transcript.context_tokens;
  const limit = transcript.context_window;
  const percent = used != null && limit ? contextPercent(used, limit) : null;
  const tone =
    percent == null
      ? ""
      : percent > 85
        ? "bg-danger"
        : percent > 65
          ? "bg-warn"
          : "bg-accent";
  const rows: Array<[string, string]> = usage
    ? [
        ["Input", tokens(usage.input_tokens)],
        ["Cache read", tokens(usage.cache_read_input_tokens)],
        ["Cache write", tokens(usage.cache_write_input_tokens)],
        ["Output", tokens(usage.output_tokens)],
        ...(usage.reasoning_tokens
          ? ([["Reasoning (in output)", tokens(usage.reasoning_tokens)]] as Array<
              [string, string]
            >)
          : []),
      ]
    : [];
  return (
    <Section title="Context">
      {percent == null ? (
        <p className="text-ui text-muted">
          Context size appears after the first reply.
        </p>
      ) : (
        <>
          <div className="mb-1.5 flex items-baseline justify-between text-ui">
            <span>{percent}% used</span>
            <span className="text-small text-faint">
              {tokens(used)} / {tokens(limit)}
            </span>
          </div>
          <div
            role="meter"
            aria-label="Context used"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent}
            className="h-1.5 overflow-hidden rounded-full bg-line-strong"
          >
            <div
              className={clsx("h-full rounded-full", tone)}
              style={{ width: `${Math.max(percent, 1)}%` }}
            />
          </div>
          {percent > 65 && (
            <p className="mt-2 text-small text-faint">
              Compact the session from Ctrl K to free space.
            </p>
          )}
        </>
      )}
      {rows.length > 0 && (
        <dl className="mt-3 space-y-1 text-ui">
          <dt className="pb-0.5 text-small text-faint">Used this session</dt>
          {rows.map(([label, value]) => (
            <div key={label} className="flex justify-between gap-2">
              <dt className="text-muted">{label}</dt>
              <dd className="tabular-nums">{value}</dd>
            </div>
          ))}
          {transcript.cost_usd_nanos != null && (
            <div className="flex justify-between gap-2">
              <dt className="text-muted">Estimated cost</dt>
              <dd className="tabular-nums">{cost(transcript.cost_usd_nanos)}</dd>
            </div>
          )}
        </dl>
      )}
    </Section>
  );
}

function Connectors({ project }: { project: ProjectState }) {
  const hosts = project.tool_hosts ?? [];
  const total = hosts.reduce(
    (sum, host) => sum + (host.ready ? host.tool_count : 0),
    0,
  );
  return (
    <Section
      title="Connectors"
      aside={hosts.length > 0 ? `${total} MCP tool${total === 1 ? "" : "s"}` : undefined}
    >
      {hosts.length === 0 ? (
        <p className="text-ui text-muted">
          No MCP servers. Add them to your QQ config.ron.
        </p>
      ) : (
        <ul>
          {hosts.map((host) => (
            <li key={host.name} className="py-1 text-ui">
              <div className="flex items-center gap-2.5">
                <span className="flex w-5 shrink-0 justify-center">
                  <Plug className="h-3.5 w-3.5 text-muted" />
                </span>
                <span className="min-w-0 flex-1 truncate">{host.name}</span>
                <span
                  className={clsx(
                    "h-1.5 w-1.5 shrink-0 rounded-full",
                    host.ready ? "bg-ok" : "bg-danger",
                  )}
                />
                <span className="shrink-0 text-small text-faint">
                  {host.ready
                    ? `${host.tool_count} tool${host.tool_count === 1 ? "" : "s"}`
                    : "Unavailable"}
                </span>
              </div>
              {host.message && (
                <p className="ml-[30px] mt-0.5 text-small text-muted">
                  {host.message}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

function Changes({
  changes,
  showChanges,
}: {
  changes: FileChange[];
  showChanges: () => void;
}) {
  const added = changes.reduce((total, file) => total + file.added, 0);
  const removed = changes.reduce((total, file) => total + file.removed, 0);
  return (
    <section className="shrink-0 border-t border-line px-4 py-3">
      <h3 className="mb-2 text-ui font-medium">Changes</h3>
      <button
        onClick={showChanges}
        className="flex h-9 w-full items-center gap-2.5 rounded-lg border border-line-strong px-3 text-ui hover:bg-hover"
      >
        <FileDiff className="h-3.5 w-3.5 text-muted" />
        {changes.length === 0 ? (
          <span className="text-muted">No file edits yet</span>
        ) : (
          <span>
            {changes.length} file{changes.length === 1 ? "" : "s"}{" "}
            <span className="font-mono text-small text-ok">+{added}</span>{" "}
            <span className="font-mono text-small text-danger">−{removed}</span>
          </span>
        )}
        <span className="ml-auto flex items-center gap-0.5 text-small text-muted">
          View
          <ChevronRight className="h-3.5 w-3.5" />
        </span>
      </button>
    </section>
  );
}
