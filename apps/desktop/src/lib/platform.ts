export type PlatformName = "desktop" | "android" | "ios";

/** UI capabilities only; native commands must enforce their own target boundary. */
export function platformFor(name: PlatformName) {
  const desktop = name === "desktop";
  return Object.freeze({
    name,
    mobile: !desktop,
    features: Object.freeze({
      localEngine: desktop,
      localProjects: desktop,
      git: desktop,
      worktrees: desktop,
      editor: desktop,
      fileDrop: desktop,
      hostConfiguration: desktop,
      nativeNotifications: desktop,
    }),
  });
}

function isPlatformName(name: unknown): name is PlatformName {
  return name === "desktop" || name === "android" || name === "ios";
}

/**
 * The native shell's identity, or why it could not be read. A failure never
 * falls back to desktop, which would offer host features on a phone.
 */
export async function nativePlatform(
  native: boolean,
  ask: () => Promise<unknown>,
): Promise<{ name: PlatformName } | { error: string }> {
  if (!native) return { name: "desktop" };
  try {
    const name = await ask();
    if (isPlatformName(name)) return { name };
    return { error: `unknown client platform ${JSON.stringify(name)}` };
  } catch (error) {
    return { error: `client_platform failed: ${String(error)}` };
  }
}

export let platform = platformFor("desktop");

/** Called before React mounts, from native target identity or the dev fixture. */
export function configurePlatform(name: unknown) {
  if (!isPlatformName(name)) throw new Error("Unknown QQ client platform");
  platform = platformFor(name);
  document.documentElement.dataset.platform = name;
  document.documentElement.dataset.mobile = String(platform.mobile);
}

/**
 * Whether an on-screen keyboard is covering the layout viewport (iOS keeps the
 * layout height; Android with `resizes-content` shrinks both). A pinch zoom
 * also shrinks the visual viewport, so only an unzoomed one counts.
 */
export function keyboardCovers(layoutHeight: number, visualHeight: number, scale: number) {
  return scale < 1.01 && layoutHeight - visualHeight > 120;
}

/** Follow the visible viewport while a mobile keyboard covers the layout viewport. */
export function watchMobileViewport(): () => void {
  if (!platform.mobile) return () => undefined;
  const viewport = window.visualViewport;
  const update = () => {
    const root = document.documentElement.style;
    root.setProperty("--app-height", `${viewport?.height ?? window.innerHeight}px`);
    root.setProperty("--app-top", `${viewport?.offsetTop ?? 0}px`);
    // The keyboard replaces the home indicator's bottom inset.
    document.documentElement.dataset.keyboard = String(
      !!viewport && keyboardCovers(window.innerHeight, viewport.height, viewport.scale),
    );
  };
  update();
  viewport?.addEventListener("resize", update);
  viewport?.addEventListener("scroll", update);
  window.addEventListener("resize", update);
  return () => {
    viewport?.removeEventListener("resize", update);
    viewport?.removeEventListener("scroll", update);
    window.removeEventListener("resize", update);
    document.documentElement.style.removeProperty("--app-height");
    document.documentElement.style.removeProperty("--app-top");
    delete document.documentElement.dataset.keyboard;
  };
}
