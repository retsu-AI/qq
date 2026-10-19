import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import clsx from "clsx";
import {
  ArrowDown,
  ArrowUp,
  GitBranch,
  GitCommitHorizontal,
  GitPullRequest,
  GitPullRequestDraft,
  RefreshCw,
  Upload,
} from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api } from "../lib/api";
import type { GitStatusDto } from "../lib/generated";
import { toast } from "../lib/toast";
import { ConfirmDialog } from "./ConfirmDialog";
import { Dropdown, type DropdownAction } from "./Dropdown";

/** Local status is cheap; the pull request needs `gh` and the network. */
const STATUS_EVERY_MS = 15_000;
const PR_EVERY_MS = 120_000;

function useGitStatus(projectId: string, refreshKey: unknown) {
  const [status, setStatus] = useState<GitStatusDto | null>(null);
  const lastPr = useRef(0);
  const sequence = useRef(0);
  const refresh = useCallback(
    async (withPr = false) => {
      const request = ++sequence.current;
      const pr = withPr || Date.now() - lastPr.current > PR_EVERY_MS;
      try {
        const next = await api.gitStatus(projectId, pr);
        if (request !== sequence.current) return;
        if (pr) lastPr.current = Date.now();
        // Keep the known pull request between network checks.
        setStatus((previous) =>
          pr || !previous ? next : { ...next, pr: previous.pr },
        );
      } catch {
        if (request === sequence.current) setStatus(null);
      }
    },
    [projectId],
  );

  useEffect(() => {
    lastPr.current = 0;
    setStatus(null);
    void refresh(true);
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void refresh();
    }, STATUS_EVERY_MS);
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("focus", onFocus);
    };
  }, [refresh]);

  // A run settling usually means files changed.
  useEffect(() => {
    void refresh();
  }, [refreshKey]);

  return { status, refresh };
}

type Dialog = "commit" | "commit-push" | "push" | "pr" | null;

/**
 * Branch, change count, ahead/behind and pull request for the project, with
 * commit, push and pull-request actions. Each outward action is confirmed in
 * a dialog first.
 */
export function GitStrip({
  projectId,
  sessionTitle,
  refreshKey,
}: {
  projectId: string;
  /** Prefills the pull request title. */
  sessionTitle: string;
  /** Changes when the session's run state changes, to refresh promptly. */
  refreshKey: unknown;
}) {
  const { status, refresh } = useGitStatus(projectId, refreshKey);
  const [dialog, setDialog] = useState<Dialog>(null);
  if (!status?.repo) return null;

  const pr = status.pr;
  const openPr = pr && pr.state === "OPEN";
  const canPush = Boolean(status.branch) && (status.ahead > 0 || !status.upstream);
  const actions: DropdownAction[] = [
    ...(status.changed > 0
      ? [
          {
            label: "Commit…",
            icon: <GitCommitHorizontal className="h-3.5 w-3.5" />,
            onSelect: () => setDialog("commit"),
          },
          ...(status.branch
            ? [
                {
                  label: "Commit and push…",
                  icon: <Upload className="h-3.5 w-3.5" />,
                  onSelect: () => setDialog("commit-push"),
                },
              ]
            : []),
        ]
      : []),
    ...(canPush
      ? [
          {
            label: status.upstream ? "Push" : "Publish branch",
            icon: <ArrowUp className="h-3.5 w-3.5" />,
            onSelect: () => setDialog("push"),
          },
        ]
      : []),
    ...(pr
      ? [
          {
            label: `Open pull request #${pr.number}`,
            icon: <GitPullRequest className="h-3.5 w-3.5" />,
            onSelect: () => void openUrl(pr.url),
          },
        ]
      : status.gh && status.branch
        ? [
            {
              label: "Create pull request…",
              icon: <GitPullRequest className="h-3.5 w-3.5" />,
              onSelect: () => setDialog("pr"),
            },
          ]
        : []),
    {
      label: "Refresh",
      icon: <RefreshCw className="h-3.5 w-3.5" />,
      onSelect: () => void refresh(true),
    },
  ];
  const summary = [
    status.branch ?? "Detached HEAD",
    status.changed ? `${status.changed} changed` : "clean",
    status.ahead ? `${status.ahead} to push` : "",
    status.behind ? `${status.behind} behind` : "",
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <>
      <Dropdown
        label={`Git: ${summary}`}
        title={summary}
        value={null}
        options={[]}
        onChange={() => undefined}
        actions={actions}
        side="bottom"
        align="end"
        bare
        className="flex h-7 max-w-[260px] items-center gap-1.5 rounded-md px-2 text-[12px] text-muted hover:bg-hover hover:text-fg aria-expanded:bg-hover aria-expanded:text-fg"
      >
        <GitBranch className="h-3.5 w-3.5 shrink-0" />
        <span className="min-w-0 truncate font-mono text-[11.5px]">
          {status.branch ?? "detached"}
        </span>
        {status.changed > 0 && (
          <span
            className={clsx(
              "shrink-0 rounded-full px-1.5 text-[10.5px] font-medium",
              status.conflicted
                ? "bg-danger-soft text-danger"
                : "bg-warn-soft text-warn",
            )}
          >
            {status.changed}
          </span>
        )}
        {status.ahead > 0 && (
          <span className="flex shrink-0 items-center text-[11px]">
            <ArrowUp className="h-3 w-3" />
            {status.ahead}
          </span>
        )}
        {status.behind > 0 && (
          <span className="flex shrink-0 items-center text-[11px]">
            <ArrowDown className="h-3 w-3" />
            {status.behind}
          </span>
        )}
        {pr && (
          <span
            className={clsx(
              "flex shrink-0 items-center gap-0.5 rounded-full px-1.5 text-[10.5px] font-medium",
              pr.state === "MERGED"
                ? "bg-accent-soft text-accent"
                : openPr
                  ? "bg-ok/15 text-ok"
                  : "bg-hover text-muted",
            )}
          >
            {pr.draft ? (
              <GitPullRequestDraft className="h-3 w-3" />
            ) : (
              <GitPullRequest className="h-3 w-3" />
            )}
            #{pr.number}
          </span>
        )}
      </Dropdown>
      {(dialog === "commit" || dialog === "commit-push") && (
        <CommitDialog
          projectId={projectId}
          changed={status.changed}
          branch={status.branch}
          push={dialog === "commit-push"}
          close={() => setDialog(null)}
          done={() => void refresh(dialog === "commit-push")}
        />
      )}
      {dialog === "push" && (
        <ConfirmDialog
          title={status.upstream ? "Push to the remote?" : "Publish this branch?"}
          confirmLabel={status.upstream ? "Push" : "Publish"}
          tone="primary"
          onConfirm={async () => {
            await api.gitPush(projectId);
            toast(`Pushed ${status.branch}`);
            void refresh(true);
          }}
          close={() => setDialog(null)}
        >
          {status.upstream ? (
            <>
              Sends {status.ahead} commit{status.ahead === 1 ? "" : "s"} on{" "}
              <code className="font-mono text-fg">{status.branch}</code> to{" "}
              <code className="font-mono text-fg">{status.upstream}</code>.
            </>
          ) : (
            <>
              Creates <code className="font-mono text-fg">{status.branch}</code>{" "}
              on <code className="font-mono text-fg">origin</code> and pushes it.
            </>
          )}
        </ConfirmDialog>
      )}
      {dialog === "pr" && status.branch && (
        <PullRequestDialog
          projectId={projectId}
          branch={status.branch}
          defaultTitle={sessionTitle === "New session" ? "" : sessionTitle}
          close={() => setDialog(null)}
          done={() => void refresh(true)}
        />
      )}
    </>
  );
}

function FormDialog({
  title,
  children,
  submitLabel,
  submit,
  close,
}: {
  title: string;
  children: ReactNode;
  submitLabel: string;
  /** Rejections show in the dialog, which stays open. */
  submit: () => Promise<void>;
  close: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog
      ref={dialog}
      onCancel={(event) => {
        if (busy) event.preventDefault();
        else close();
      }}
      aria-label={title}
      className="m-auto w-[520px] max-w-[92vw] rounded-2xl border border-line-strong bg-bg p-0 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <form
        className="px-6 pb-5 pt-5"
        onSubmit={async (event) => {
          event.preventDefault();
          setBusy(true);
          setError(null);
          try {
            await submit();
            close();
          } catch (error) {
            setError(String(error));
            setBusy(false);
          }
        }}
      >
        <h2 className="text-[15px] font-semibold">{title}</h2>
        <div className="mt-3 space-y-3 text-[13px]">{children}</div>
        {error && (
          <p
            role="alert"
            className="mt-3 max-h-40 overflow-y-auto whitespace-pre-wrap rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable"
          >
            {error}
          </p>
        )}
        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={close}
            disabled={busy}
            className="h-8 rounded-lg border border-line-strong px-3 text-[13px] hover:bg-hover disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy}
            className="flex h-8 items-center gap-2 rounded-lg bg-solid px-3 text-[13px] font-medium text-white hover:bg-solid-hover disabled:opacity-60"
          >
            {busy && <RefreshCw className="h-3.5 w-3.5 animate-spin" />}
            {submitLabel}
          </button>
        </div>
      </form>
    </dialog>
  );
}

const fieldClass =
  "w-full rounded-lg border border-line-strong bg-raised px-3 py-2 text-[13px] outline-none focus:border-accent/60 focus-visible:outline-none";

function CommitDialog({
  projectId,
  changed,
  branch,
  push,
  close,
  done,
}: {
  projectId: string;
  changed: number;
  branch: string | null;
  push: boolean;
  close: () => void;
  done: () => void;
}) {
  const [message, setMessage] = useState("");
  return (
    <FormDialog
      title={push ? "Commit and push" : "Commit changes"}
      submitLabel={push ? "Commit and push" : "Commit"}
      close={close}
      submit={async () => {
        const id = await api.gitCommit(projectId, message, push);
        toast(push ? `Committed ${id} and pushed ${branch}` : `Committed ${id}`);
        done();
      }}
    >
      <p className="text-muted">
        Stages and commits all {changed} changed file{changed === 1 ? "" : "s"}
        {branch ? (
          <>
            {" "}on <code className="font-mono text-fg">{branch}</code>
          </>
        ) : null}
        {push ? ", then pushes the branch." : "."} Git hooks run as usual.
      </p>
      <textarea
        autoFocus
        required
        aria-label="Commit message"
        placeholder="feat: describe the change"
        rows={4}
        value={message}
        onChange={(event) => setMessage(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && event.ctrlKey) {
            event.preventDefault();
            event.currentTarget.form?.requestSubmit();
          }
        }}
        className={clsx(fieldClass, "resize-y font-mono")}
      />
      <p className="text-[12px] text-faint">Ctrl Enter to commit.</p>
    </FormDialog>
  );
}

function PullRequestDialog({
  projectId,
  branch,
  defaultTitle,
  close,
  done,
}: {
  projectId: string;
  branch: string;
  defaultTitle: string;
  close: () => void;
  done: () => void;
}) {
  const [title, setTitle] = useState(defaultTitle);
  const [body, setBody] = useState("");
  const [draft, setDraft] = useState(true);
  return (
    <FormDialog
      title="Create pull request"
      submitLabel={draft ? "Create draft pull request" : "Create pull request"}
      close={close}
      submit={async () => {
        const url = await api.gitCreatePr(projectId, title, body, draft);
        toast(url ? `Opened ${url}` : "Pull request created");
        done();
      }}
    >
      <p className="text-muted">
        Pushes <code className="font-mono text-fg">{branch}</code> and opens a
        pull request against the repository's default branch with the GitHub
        CLI (<code className="font-mono">gh</code>).
      </p>
      <input
        autoFocus
        required
        aria-label="Title"
        placeholder="Title"
        value={title}
        onChange={(event) => setTitle(event.target.value)}
        className={fieldClass}
      />
      <textarea
        aria-label="Description"
        placeholder="What changed and why (Markdown)"
        rows={6}
        value={body}
        onChange={(event) => setBody(event.target.value)}
        className={clsx(fieldClass, "resize-y")}
      />
      <label className="flex items-center gap-2 text-muted">
        <input
          type="checkbox"
          checked={draft}
          onChange={(event) => setDraft(event.target.checked)}
          className="accent-[var(--accent)]"
        />
        Open as a draft
      </label>
    </FormDialog>
  );
}
