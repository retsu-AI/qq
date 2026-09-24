// The one place the site's deployment facts live. Everything else imports
// from here so a domain or base-path change is a single edit.
// Written by scripts/sync-docs.mjs from the workspace Cargo.toml, so the
// landing page shows the released version without a second edit here.
import generated from './src/generated/site.json' with { type: 'json' };

export const site = {
  // GitHub Pages for the retsu-AI/qq repository. Moving to a custom domain
  // means changing these two lines and adding public/CNAME.
  url: 'https://retsu-ai.github.io',
  base: '/qq/',
  github: 'https://github.com/retsu-AI/qq',
  version: `v${generated.version}`,
  description:
    'AI coding agents in one binary. An interactive terminal, a headless runner, and a local server — with readable approvals and a durable record.',
};

// Mirrors docs/guide/install.md; keep the two in step.
export const installMethods = [
  { label: 'macOS/Linux', command: `curl -fsSL ${site.url}${site.base}install.sh | sh` },
  { label: 'Homebrew', command: 'brew install retsu-ai/qq/qq' },
  { label: 'Nix', command: 'nix run github:retsu-AI/qq' },
  { label: 'Cargo', command: `cargo binstall --git ${site.github} qq` },
  { label: 'Windows', command: '# Download qq-vX.Y.Z-x86_64-pc-windows-msvc.zip from Releases' },
];

/** Site-absolute href that honors the deployment base. */
export function href(path = '') {
  return `${site.base.replace(/\/$/, '')}/${path.replace(/^\//, '')}`;
}
