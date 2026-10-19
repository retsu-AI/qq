// Naming for worktree sessions.

const STOP_WORDS = new Set(["a", "an", "the", "to", "of", "and", "for", "in", "on", "with", "please"]);

/**
 * A branch name for a task: `qq/` plus its first few meaningful words, or a
 * timestamp when the task is empty.
 */
export function branchFromTask(task: string, now = new Date()): string {
  const words = task
    .toLowerCase()
    .replace(/@\S+/g, " ")
    .split(/[^a-z0-9]+/)
    .filter((word) => word && !STOP_WORDS.has(word))
    .slice(0, 5);
  if (words.length > 0) return `qq/${words.join("-").slice(0, 48).replace(/-+$/, "")}`;
  const pad = (value: number) => String(value).padStart(2, "0");
  return `qq/task-${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}`;
}
