import type { ApprovalChoice } from "./types";

export function approvalShortcut(
  key: string,
  context: { blocked: boolean; grantable: boolean },
): ApprovalChoice | null {
  if (context.blocked) return null;
  switch (key.toLowerCase()) {
    case "y":
      return { type: "once" };
    case "n":
      return { type: "deny" };
    case "s":
      return context.grantable ? { type: "session" } : null;
    case "w":
      return context.grantable ? { type: "workspace" } : null;
    default:
      return null;
  }
}
