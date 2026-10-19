// One-line summaries of tool calls from their JSON arguments.

export function parseArgs(raw: string): Record<string, unknown> {
  try {
    const value = JSON.parse(raw);
    return value && typeof value === "object" ? (value as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

const str = (v: unknown): string | null => (typeof v === "string" && v.trim() ? v : null);

const VERBS: Record<string, string> = {
  shell: "Run",
  read_file: "Read",
  write_file: "Write",
  edit_file: "Edit",
  search: "Search",
  tree: "List",
  fetch: "Fetch",
  spawn_agent: "Delegate",
  ask_user: "Ask",
  load_skill: "Load skill",
  update_plan: "Plan",
};

export function toolVerb(name: string): string {
  return VERBS[name] ?? name.replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
}

export function toolSubject(name: string, raw: string): string {
  const args = parseArgs(raw);
  switch (name) {
    case "shell":
      return str(args.command) ?? "";
    case "read_file":
    case "write_file":
    case "edit_file":
      return str(args.path) ?? str(args.file) ?? "";
    case "search":
      return str(args.pattern) ?? str(args.query) ?? "";
    case "tree":
      return str(args.path) ?? ".";
    case "fetch":
      return str(args.url) ?? "";
    case "spawn_agent":
      return str(args.title) ?? str(args.task) ?? str(args.brief) ?? "";
    default: {
      for (const value of Object.values(args)) {
        const s = str(value);
        if (s) return s;
      }
      return "";
    }
  }
}

export function isShell(name: string): boolean {
  return name === "shell";
}

const ACTIVITY: Record<string, [string, string, string]> = {
  shell: ["Ran", "command", "commands"],
  read_file: ["Read", "file", "files"],
  write_file: ["Wrote", "file", "files"],
  edit_file: ["Edited", "file", "files"],
  search: ["Searched", "pattern", "patterns"],
  tree: ["Listed", "folder", "folders"],
  fetch: ["Fetched", "page", "pages"],
  spawn_agent: ["Delegated", "task", "tasks"],
};

/** "Ran 3 commands, read 2 files" for a run of settled tool calls. */
export function summarizeTools(names: string[]): string {
  const counts = new Map<string, number>();
  for (const name of names) {
    const key = name in ACTIVITY ? name : "other";
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  const parts = [...counts].map(([key, count]) => {
    const [verb, one, many] = ACTIVITY[key] ?? ["Used", "tool", "tools"];
    return `${verb} ${count} ${count === 1 ? one : many}`;
  });
  return parts
    .map((part, index) => (index === 0 ? part : part[0].toLowerCase() + part.slice(1)))
    .join(", ");
}
