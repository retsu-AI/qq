// Token totals for a session, summed from its finished runs.
import type { TokenUsage } from "./types";

/** The sum of every run's usage, or null when no run reported any. */
export function sessionUsage(
  items: ReadonlyArray<{ kind: string; usage?: TokenUsage | null }>,
): TokenUsage | null {
  let total: TokenUsage | null = null;
  for (const item of items) {
    if (item.kind !== "run_end" || !item.usage) continue;
    const usage = item.usage;
    total ??= {
      input_tokens: 0,
      cache_read_input_tokens: 0,
      cache_write_input_tokens: 0,
      output_tokens: 0,
    };
    total.input_tokens += usage.input_tokens;
    total.cache_read_input_tokens += usage.cache_read_input_tokens;
    total.cache_write_input_tokens += usage.cache_write_input_tokens;
    total.output_tokens += usage.output_tokens;
    if (usage.reasoning_tokens != null)
      total.reasoning_tokens =
        (total.reasoning_tokens ?? 0) + usage.reasoning_tokens;
  }
  return total;
}

/** How full the context is, as a whole percent from 0 to 100. */
export function contextPercent(used: number, limit: number): number {
  return limit > 0 ? Math.min(100, Math.round((used / limit) * 100)) : 0;
}
