export type MenuItem =
  | {
      label: string;
      shortcut?: string;
      onSelect: () => void;
      disabled?: boolean;
      checked?: boolean;
      desktopOnly?: boolean;
    }
  | "separator";

export interface Menu {
  label: string;
  /** Alt plus this key opens the menu. */
  mnemonic: string;
  items: MenuItem[];
}

/**
 * The menu bar folded into one phone menu: desktop-only items removed, each
 * former menu (or separated group) divided by one separator.
 */
export function phoneMenus(menus: Menu[]): Menu[] {
  const items: MenuItem[] = [];
  for (const menu of menus) {
    for (const item of [...menu.items, "separator" as const]) {
      if (item === "separator") {
        if (items.length > 0 && items.at(-1) !== "separator") items.push(item);
      } else if (!item.desktopOnly) {
        items.push(item);
      }
    }
  }
  if (items.at(-1) === "separator") items.pop();
  return items.length === 0 ? [] : [{ label: "Menu", mnemonic: "m", items }];
}
