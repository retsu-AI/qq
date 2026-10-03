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
