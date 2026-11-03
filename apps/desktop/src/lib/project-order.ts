import type { ProjectInfo } from "./types";

const KEY = "qq.sidebar.order";
const MAX_ENTRIES = 256;

export function groupKey(project: ProjectInfo): string {
  return project.worktree?.main_path ?? project.path;
}

export function parseProjectOrder(json: string | null): string[] {
  let value: unknown;
  try {
    value = JSON.parse(json ?? "[]");
  } catch {
    return [];
  }
  if (!Array.isArray(value)) return [];
  const keys = value.filter((key): key is string => typeof key === "string");
  return [...new Set(keys)].slice(0, MAX_ENTRIES);
}

export function sortProjects(open: ProjectInfo[], order: string[]): ProjectInfo[] {
  const rank = new Map(order.map((key, index) => [key, index]));
  const repoName = (p: ProjectInfo) => (p.worktree?.main_name ?? p.name).toLowerCase();
  const place = (p: ProjectInfo) => rank.get(groupKey(p)) ?? Infinity;
  return [...open].sort(
    (a, b) =>
      place(a) - place(b) ||
      repoName(a).localeCompare(repoName(b)) ||
      groupKey(a).localeCompare(groupKey(b)) ||
      Number(Boolean(a.worktree)) - Number(Boolean(b.worktree)) ||
      a.name.localeCompare(b.name),
  );
}

export function moveGroup(
  order: string[],
  shown: string[],
  key: string,
  to: number,
): string[] {
  const rest = shown.filter((k) => k !== key);
  const index = Math.max(0, Math.min(to, rest.length));
  const placed = [...rest.slice(0, index), key, ...rest.slice(index)];
  const closed = order.filter((k) => !shown.includes(k));
  return [...placed, ...closed].slice(0, MAX_ENTRIES);
}

export interface GroupBox {
  key: string;
  top: number;
  bottom: number;
}

export function dragLayout(
  boxes: GroupBox[],
  key: string,
  dy: number,
  gap: number,
): { to: number; slot: number; offsets: Record<string, number> } {
  const from = boxes.findIndex((b) => b.key === key);
  const offsets: Record<string, number> = {};
  if (from < 0) return { to: 0, slot: 0, offsets };
  const self = boxes[from];
  const minDy = boxes[0].top - self.top;
  const maxDy = boxes[boxes.length - 1].bottom - self.bottom;
  const shift = Math.max(minDy, Math.min(dy, maxDy));
  const middle = (b: GroupBox) => (b.top + b.bottom) / 2;
  const passedAbove = boxes.filter((b, i) => i < from && self.top + shift < middle(b));
  const passedBelow = boxes.filter((b, i) => i > from && self.bottom + shift > middle(b));
  const to = from - passedAbove.length + passedBelow.length;
  const step = self.bottom - self.top + gap;
  for (const b of boxes) offsets[b.key] = 0;
  for (const b of passedAbove) offsets[b.key] = step;
  for (const b of passedBelow) offsets[b.key] = -step;
  const height = (list: GroupBox[]) =>
    list.reduce((sum, b) => sum + b.bottom - b.top + gap, 0);
  const slot = height(passedBelow) - height(passedAbove);
  offsets[key] = shift;
  return { to, slot, offsets };
}

export function readProjectOrder(): string[] {
  try {
    return parseProjectOrder(localStorage.getItem(KEY));
  } catch {
    return [];
  }
}

export function saveProjectOrder(order: string[]) {
  localStorage.setItem(KEY, JSON.stringify(order));
}
