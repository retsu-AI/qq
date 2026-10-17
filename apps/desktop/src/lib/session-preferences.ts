export interface SessionPreferences {
  aliases: Record<string, string>;
  archived: string[];
}

export function parseSessionPreferences(
  json: string | null,
): SessionPreferences {
  if (!json) return { aliases: {}, archived: [] };
  try {
    const value = JSON.parse(json);
    const aliases: Record<string, string> = Object.create(null);
    for (const [id, title] of Object.entries(value.aliases ?? {}).slice(
      0,
      2048,
    )) {
      if (typeof title === "string" && title.length <= 160) aliases[id] = title;
    }
    const archived = Array.isArray(value.archived)
      ? value.archived
          .filter((id: unknown): id is string => typeof id === "string")
          .slice(0, 2048)
      : [];
    return { aliases, archived };
  } catch {
    return { aliases: {}, archived: [] };
  }
}

export function matchesSession(query: string, title: string, tail: string) {
  const terms = query.toLocaleLowerCase().trim().split(/\s+/);
  const text = `${title}\n${tail}`.toLocaleLowerCase();
  return terms.every((term) => text.includes(term));
}

export function updateSessionPreferences(
  current: SessionPreferences,
  id: string,
  change: { title?: string; archived?: boolean },
): SessionPreferences {
  const aliases = { ...current.aliases };
  if (change.title !== undefined) {
    const title = change.title.trim();
    if (title.length > 160)
      throw new Error("Session names must be 160 characters or fewer");
    if (title) aliases[id] = title;
    else delete aliases[id];
  }
  const archived = new Set(current.archived);
  if (change.archived === true) archived.add(id);
  if (change.archived === false) archived.delete(id);
  if (Object.keys(aliases).length > 2048 || archived.size > 2048)
    throw new Error("Session organization limit reached (2048)");
  return { aliases, archived: [...archived] };
}
