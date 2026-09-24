import { readFileSync } from 'node:fs';
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { site, href } from './site.config.mjs';

// The sidebar and the generated pages share one manifest; scripts/sync-docs.mjs
// fails the build if the manifest and docs/guide disagree.
const sidebar = JSON.parse(readFileSync(new URL('./sidebar.json', import.meta.url), 'utf8')).map(
  (group) => ({
    label: group.label,
    items: group.items.map((item) => ({
      label: item.label,
      slug: item.page === 'index' ? 'docs' : `docs/${item.page}`,
    })),
  }),
);

export default defineConfig({
  site: site.url,
  base: site.base,
  output: 'static',
  trailingSlash: 'always',
  devToolbar: { enabled: false },
  integrations: [
    starlight({
      title: 'QQ',
      description: site.description,
      // Starlight prefixes the base itself.
      favicon: '/favicon.svg',
      customCss: ['./src/styles/custom.css'],
      // Every generated page carries an editUrl into docs/guide; the two
      // authored pages (landing, 404) opt out, so no default base is needed.
      // Pages are generated at build time, so Git dates would describe the
      // generator's output, not the guide; the edit link is the provenance.
      lastUpdated: false,
      tableOfContents: { minHeadingLevel: 2, maxHeadingLevel: 3 },
      expressiveCode: {
        themes: ['nord'],
        useStarlightDarkModeSwitch: false,
        useStarlightUiThemeColors: false,
        styleOverrides: {
          codeBackground: '#20242c',
          codeFontFamily: "'JetBrains Mono', monospace",
          codeFontSize: '0.875rem',
          borderColor: '#7b84974d',
          borderRadius: '0.5rem',
          frames: { editorBackground: '#20242c', terminalBackground: '#20242c' },
        },
      },
      head: [
        { tag: 'meta', attrs: { name: 'theme-color', content: '#141720' } },
        { tag: 'meta', attrs: { property: 'og:image', content: new URL(href('og-image.png'), site.url).href } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
      ],
      components: {
        Header: './src/components/Header.astro',
        SiteTitle: './src/components/SiteTitle.astro',
        Hero: './src/components/Hero.astro',
        PageTitle: './src/components/PageTitle.astro',
        Footer: './src/components/Footer.astro',
        ThemeProvider: './src/components/ThemeProvider.astro',
      },
      sidebar,
    }),
  ],
});
