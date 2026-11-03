import type { ApprovalChoice } from "./types";

export function approvalShortcut(
  key: string,
  context: { blocked: boolean },
): ApprovalChoice | null {
  if (context.blocked) return null;
  switch (key.toLowerCase()) {
    case "y":
      return { type: "once" };
    case "n":
      return { type: "deny" };
    default:
      return null;
  }
}
