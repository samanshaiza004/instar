// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const repository = 'https://github.com/samanshaiza004/instar';

// The landing page is a hand-built portal (src/pages/index.astro). Starlight
// owns everything under /docs/, which is why the content collection nests a
// `docs/` directory inside `src/content/docs/`: the directory name is the URL
// prefix, and keeping it in the filesystem means no route rewriting anywhere.
export default defineConfig({
  site: 'https://instar.samanshaiza.com',
  trailingSlash: 'ignore',
  build: { format: 'directory' },
  // Honour PORT so `npm run dev`/`preview` can be driven by a supervisor that
  // assigns the port; falls back to Astro's default otherwise.
  server: { port: Number(process.env.PORT) || 4321 },
  integrations: [
    starlight({
      // Page titles render as "<page> | Instar"; the visible header wordmark is
      // the SiteTitle override, not this string.
      title: 'Instar',
      description:
        'Install, run, build, and understand Instar: a native host for applications compiled as WebAssembly components.',
      tagline: 'Run the guest. Own the machine.',
      favicon: '/favicon.svg',
      customCss: ['./src/styles/docs.css'],
      editLink: {
        baseUrl: `${repository}/edit/master/website/`,
      },
      lastUpdated: false,
      pagination: true,
      credits: false,
      disable404Route: true,
      social: [
        { icon: 'github', label: 'GitHub', href: repository },
      ],
      components: {
        SiteTitle: './src/components/docs/SiteTitle.astro',
        Search: './src/components/docs/Search.astro',
        Footer: './src/components/docs/Footer.astro',
      },
      head: [
        {
          tag: 'meta',
          attrs: { name: 'theme-color', content: '#ff5a00' },
        },
      ],
      sidebar: [
        {
          label: 'Start here',
          items: [
            { slug: 'docs' },
            { slug: 'docs/getting-started/requirements' },
            { slug: 'docs/getting-started/install' },
            { slug: 'docs/getting-started/first-run' },
            { slug: 'docs/getting-started/build-a-guest' },
            { slug: 'docs/getting-started/verify' },
          ],
        },
        {
          label: 'Understand',
          items: [{ autogenerate: { directory: 'docs/concepts' } }],
        },
        {
          label: 'Reference',
          items: [{ autogenerate: { directory: 'docs/reference' } }],
        },
        {
          label: 'Develop Instar',
          items: [{ autogenerate: { directory: 'docs/development' } }],
        },
        {
          label: 'Project',
          items: [{ autogenerate: { directory: 'docs/project' } }],
        },
      ],
    }),
  ],
});
