import type { RunActivity, RunOutcome, ToolCallState } from "./types";

export function duration(ms: number | null | undefined): string {
  if (ms == null) return "";
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s % 60)}s`;
}

export function tokens(n: number | null | undefined): string {
  if (n == null) return "";
  if (n < 1000) return `${n}`;
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

export function cost(nanos: number | null | undefined): string {
  if (nanos == null) return "";
  const usd = nanos / 1e9;
  if (usd === 0) return "$0";
  if (usd < 0.01) return "<$0.01";
  return `$${usd.toFixed(2)}`;
}

export function relativeTime(ms: number): string {
  const diff = Date.now() - ms;
  const minutes = Math.round(diff / 60_000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.round(hours / 24);
  if (days < 7) return `${days}d`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function activityLabel(activity: RunActivity | null, tool?: string | null): string {
  if (tool) return `Running ${tool}`;
  switch (activity) {
    case "waiting_for_provider":
      return "Waiting for model";
    case "reasoning":
      return "Thinking";
    case "generating_response":
      return "Writing";
    case "preparing_tool_call":
      return "Preparing tool call";
    default:
      return "Working";
  }
}

export function outcomeLabel(outcome: RunOutcome | null): string {
  switch (outcome?.type) {
    case "completed":
      return "Done";
    case "cancelled":
      return "Cancelled";
    case "interrupted":
      return "Interrupted";
    case "failed":
      return "Failed";
    case "budget_exhausted":
      return "Budget exhausted";
    case "paused":
      return "Paused";
    default:
      return "";
  }
}

export function outcomeDetail(outcome: RunOutcome | null): string | null {
  if (!outcome) return null;
  if (outcome.type === "failed") return outcome.failure?.message ?? null;
  if (outcome.type === "paused") return outcome.pause?.message ?? null;
  return null;
}

export function isTerminal(state: ToolCallState): boolean {
  return state === "completed" || state === "failed" || state === "denied" || state === "interrupted";
}

export function modelShort(model: string | null | undefined): string {
  if (!model) return "No model";
  const slash = model.indexOf("/");
  return slash >= 0 ? model.slice(slash + 1) : model;
}

const PROVIDER_NAMES: Record<string, string> = {
  "openai-codex": "ChatGPT",
  openai: "OpenAI",
  anthropic: "Anthropic",
  google: "Google",
  xai: "xAI",
};

/** A provider ID as people know it ("openai-codex" is ChatGPT sign-in). */
export function providerName(id: string): string {
  return PROVIDER_NAMES[id] ?? id;
}

/** The catalog's display name for a model route, else its short route. */
export function modelLabel(
  route: string | null | undefined,
  models: ReadonlyArray<{ id: string; name: string | null; model: string }>,
): string {
  const model = models.find((candidate) => candidate.id === route);
  return model ? (model.name ?? model.model) : modelShort(route);
}
