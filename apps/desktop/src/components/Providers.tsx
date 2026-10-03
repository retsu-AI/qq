import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { CircleCheck, ExternalLink, Globe, KeyRound, Loader2, X } from "lucide-react";
import { api } from "../lib/api";
import { showProviders } from "../lib/store";
import type { AuthChanged, BrowserSignIn, Provider } from "../lib/types";

interface Copy {
  name: string;
  blurb: string;
  keyUrl?: string;
  keyHint?: string;
  browserLabel?: string;
}

const COPY: Record<string, Copy> = {
  anthropic: {
    name: "Anthropic",
    blurb: "Claude models",
    keyUrl: "https://console.anthropic.com/settings/keys",
    keyHint: "sk-ant-…",
  },
  openai: {
    name: "OpenAI",
    blurb: "GPT models with an API key",
    keyUrl: "https://platform.openai.com/api-keys",
    keyHint: "sk-…",
  },
  "openai-codex": {
    name: "ChatGPT",
    blurb: "Use your ChatGPT plan (Codex models)",
    browserLabel: "Sign in with ChatGPT",
  },
  google: {
    name: "Google",
    blurb: "Gemini models",
    keyUrl: "https://aistudio.google.com/apikey",
    keyHint: "AIza…",
  },
  xai: {
    name: "xAI",
    blurb: "Grok models",
    keyUrl: "https://console.x.ai",
    keyHint: "xai-…",
    browserLabel: "Sign in with xAI",
  },
};

/** Display order: the most common choices first. */
const ORDER = ["anthropic", "openai-codex", "openai", "google", "xai"];

export function Providers() {
  const [providers, setProviders] = useState<Provider[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [browser, setBrowser] = useState<Record<string, BrowserSignIn>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});

  const reload = useCallback(async () => {
    try {
      setProviders(await api.authProviders());
      setLoadError(null);
    } catch (error) {
      setLoadError(String(error));
    }
  }, []);

  useEffect(() => {
    void reload();
    const unlisten = listen<AuthChanged>("auth-changed", (event) => {
      const { provider, error } = event.payload;
      setBrowser((current) => {
        const next = { ...current };
        delete next[provider];
        return next;
      });
      setErrors((current) => ({ ...current, [provider]: error ?? "" }));
      void reload();
    });
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") showProviders(false);
    };
    window.addEventListener("keydown", onKey);
    return () => {
      void unlisten.then((stop) => stop());
      window.removeEventListener("keydown", onKey);
    };
  }, [reload]);

  const sorted = (providers ?? []).slice().sort((a, b) => ORDER.indexOf(a.id) - ORDER.indexOf(b.id));
  const connected = sorted.filter((p) => p.stored || p.environment).length;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-6"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) showProviders(false);
      }}
    >
      <div className="flex max-h-full w-full max-w-[620px] flex-col overflow-hidden rounded-2xl border border-line-strong bg-bg shadow-2xl">
        <header className="flex items-start gap-3 border-b border-line px-6 py-4">
          <div className="flex-1">
            <h2 className="text-[16px] font-semibold">Model providers</h2>
            <p className="mt-0.5 text-[12.5px] text-muted">
              Keys are stored in the Windows credential store and shared with the
              <code className="mx-1 font-mono">qq</code>command line.
              {connected > 0 && ` ${connected} connected.`}
            </p>
          </div>
          <button
            onClick={() => showProviders(false)}
            className="rounded-md p-1 text-muted hover:bg-hover hover:text-fg"
            title="Close (Esc)"
          >
            <X className="h-4 w-4" />
          </button>
        </header>

        <div className="overflow-y-auto px-6 py-4">
          {loadError && (
            <p className="mb-3 rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger selectable">
              {loadError}
            </p>
          )}
          {!providers && !loadError && (
            <div className="flex items-center gap-2 py-6 text-muted">
              <Loader2 className="h-4 w-4 animate-spin" /> Checking credentials…
            </div>
          )}
          <ul className="space-y-3">
            {sorted.map((provider) => (
              <ProviderRow
                key={provider.id}
                provider={provider}
                browser={browser[provider.id] ?? null}
                error={errors[provider.id] || null}
                onBrowser={(signIn) => setBrowser((c) => ({ ...c, [provider.id]: signIn }))}
                onError={(message) => setErrors((c) => ({ ...c, [provider.id]: message }))}
                onChanged={reload}
              />
            ))}
          </ul>
        </div>
      </div>
    </div>
  );
}

function ProviderRow({
  provider,
  browser,
  error,
  onBrowser,
  onError,
  onChanged,
}: {
  provider: Provider;
  browser: BrowserSignIn | null;
  error: string | null;
  onBrowser: (signIn: BrowserSignIn) => void;
  onError: (message: string) => void;
  onChanged: () => Promise<void>;
}) {
  const copy = COPY[provider.id] ?? { name: provider.id, blurb: "" };
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const connected = Boolean(provider.stored || provider.environment);

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    onError("");
    try {
      await action();
    } catch (failure) {
      onError(String(failure));
    } finally {
      setBusy(false);
    }
  };

  const saveKey = () =>
    run(async () => {
      await api.authSaveKey(provider.id, key);
      setKey("");
      setEditing(false);
      await onChanged();
    });

  const signInWithBrowser = () =>
    run(async () => {
      const signIn = await api.authBrowserSignIn(provider.id);
      onBrowser(signIn);
      await openUrl(signIn.url);
    });

  const signOut = () =>
    run(async () => {
      await api.authSignOut(provider.id);
      await onChanged();
    });

  const showKeyForm = provider.accepts_key && (!connected || editing);

  return (
    <li className="rounded-xl border border-line bg-raised/60 px-4 py-3">
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="font-medium">{copy.name}</span>
            {connected && (
              <span className="flex items-center gap-1 rounded-full bg-[color-mix(in_srgb,var(--ok)_14%,transparent)] px-2 py-px text-[11px] font-medium text-ok">
                <CircleCheck className="h-3 w-3" /> Connected
              </span>
            )}
          </div>
          <div className="text-[12.5px] text-muted">
            {copy.blurb}
            {provider.stored && <span className="text-faint"> · saved in {provider.stored}</span>}
            {!provider.stored && provider.environment && (
              <span className="text-faint"> · from {provider.environment}</span>
            )}
          </div>
        </div>
        {provider.stored && (
          <div className="flex items-center gap-1">
            {provider.accepts_key && !editing && (
              <SmallButton onClick={() => setEditing(true)} disabled={busy}>
                Replace key
              </SmallButton>
            )}
            <SmallButton onClick={() => void signOut()} disabled={busy} danger>
              Sign out
            </SmallButton>
          </div>
        )}
      </div>

      {!provider.stored && provider.browser_sign_in && !browser && (
        <button
          onClick={() => void signInWithBrowser()}
          disabled={busy}
          className={clsx(
            "mt-3 flex h-9 w-full items-center justify-center gap-2 rounded-lg text-[13px] font-medium disabled:opacity-60",
            provider.accepts_key
              ? "border border-line-strong bg-bg hover:bg-hover"
              : "bg-solid text-white hover:bg-solid-hover",
          )}
        >
          {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Globe className="h-4 w-4" />}
          {copy.browserLabel ?? "Sign in with browser"}
        </button>
      )}

      {browser && (
        <div className="mt-3 rounded-lg border border-accent/40 bg-accent-soft px-3 py-2.5 text-[12.5px]">
          <div className="flex items-center gap-2">
            <Loader2 className="h-3.5 w-3.5 animate-spin text-accent" />
            <span className="flex-1">Finish signing in in your browser…</span>
            <button
              onClick={() => void openUrl(browser.url)}
              className="flex items-center gap-1 text-accent hover:underline"
            >
              Open again <ExternalLink className="h-3 w-3" />
            </button>
          </div>
          {browser.user_code && (
            <p className="mt-1.5 text-muted">
              Confirm the code{" "}
              <code className="rounded bg-bg px-1.5 py-0.5 font-mono text-[13px] text-fg selectable">
                {browser.user_code}
              </code>
            </p>
          )}
        </div>
      )}

      {showKeyForm && !browser && (
        <form
          className="mt-3 flex gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (key.trim()) void saveKey();
          }}
        >
          <div className="relative flex-1">
            <KeyRound className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-faint" />
            <input
              ref={input}
              type="password"
              value={key}
              autoComplete="off"
              spellCheck={false}
              placeholder={`${provider.browser_sign_in ? "Or paste an" : "Paste your"} API key${copy.keyHint ? ` (${copy.keyHint})` : ""}`}
              onChange={(event) => setKey(event.target.value)}
              className="h-9 w-full rounded-lg border border-line-strong bg-bg pl-8 pr-3 font-mono text-[12.5px] outline-none placeholder:font-sans placeholder:text-faint focus:border-accent"
            />
          </div>
          <button
            type="submit"
            disabled={busy || !key.trim()}
            className="h-9 rounded-lg bg-solid px-4 text-[13px] font-medium text-white hover:bg-solid-hover disabled:bg-hover disabled:text-faint"
          >
            {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : "Save"}
          </button>
          {editing && (
            <SmallButton onClick={() => setEditing(false)} disabled={busy}>
              Cancel
            </SmallButton>
          )}
        </form>
      )}

      {showKeyForm && !browser && copy.keyUrl && (
        <button
          onClick={() => void openUrl(copy.keyUrl!)}
          className="mt-1.5 flex items-center gap-1 text-[12px] text-muted hover:text-accent"
        >
          Get a {copy.name} API key <ExternalLink className="h-3 w-3" />
        </button>
      )}

      {error && (
        <p className="mt-2 rounded-md bg-danger-soft px-3 py-1.5 text-[12px] text-danger selectable">{error}</p>
      )}
    </li>
  );
}

function SmallButton({
  children,
  onClick,
  disabled,
  danger,
}: {
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={clsx(
        "h-8 rounded-lg border border-line-strong px-2.5 text-[12px] disabled:opacity-50",
        danger ? "text-danger hover:bg-danger-soft" : "text-muted hover:bg-hover hover:text-fg",
      )}
    >
      {children}
    </button>
  );
}
