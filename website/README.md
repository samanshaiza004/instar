# instar.samanshaiza.com

Instar's website: a portal-style landing page plus the full user and developer
guide. It is an [Astro](https://astro.build) project; the guide is
[Starlight](https://starlight.astro.build).

## Build locally

```sh
cd website
npm install
npm run dev
```

Then `http://localhost:4321/` for the front page and `/docs/` for the guide.
`npm run build` writes the deployable site to `website/dist/`; `npm run preview`
serves that build.

Search is production-only: Pagefind indexes the built HTML, so the dev server
shows a notice where results would be.

## Layout

| Path | Contents |
|---|---|
| `src/pages/` | The hand-built portal pages: front page, downloads. |
| `src/layouts/PortalLayout.astro` | Masthead, channel strip, footer, and the copy-to-clipboard behaviour. |
| `src/components/portal/` | The front page's modules: hero, card deck, dispatches, wire, indexes. |
| `src/components/docs/` | Starlight component overrides: site title, search deep-linking, footer. |
| `src/styles/portal.css` | The 780px portal canvas and responsive rules. |
| `src/styles/docs.css` | Starlight, dressed in the same palette and rules. |
| `src/content/docs/docs/` | The guide. One directory per section; the `docs/` nesting *is* the URL prefix. |
| `src/data/site.ts` | Links and facts shared by the portal and the docs chrome. |
| `public/install.sh` | Stable macOS/Linux installer entry point. |
| `public/install.ps1` | Stable Windows installer entry point. |
| `public/assets/` | Photography, cropped for the composition. |
| `src/assets/shore-source.jpg` | The uncropped source frame the tiles are cut from. Not shipped. |
| `../netlify.toml` | Build, redirects, content types, and security headers. |

## Documentation URLs

Starlight's content collection lives at `src/content/docs/`, and everything in
it is nested one level deeper inside `docs/`. That directory name is the URL
prefix — `src/content/docs/docs/reference/cli.md` serves `/docs/reference/cli` —
which keeps the front page at `/` without any route rewriting.

The guide previously ran on mdBook and served `.html` addresses. Those are
redirected one-for-one in `netlify.toml`; add a redirect there if you rename a
page that has been published.

## Design

The front page is deliberately built like a 2002 developer portal: a centered
780px canvas, dense 7–11px Verdana, clipped module corners, 1px rules, a glossy
editorial hero, and a compact orange project-news rail. The same information
reflows into a single column below 520px without introducing a second content
hierarchy.

The homepage has one path through the product: understand the host/guest split,
choose Start, Build, Runtime, or Download, then verify the current technical
facts. Project status appears once in the “what's new” rail rather than being
repeated in another index below the fold.

## Deploy

Netlify builds from this directory: base `website`, command `npm run build`,
publish `dist`. After deploying, check `/`, `/downloads`, `/docs/`, `/install`,
and `/install.ps1`.

No user-facing page should refer to the predecessor brand or repository. Old
names may remain in engineering archaeology and measured baseline documents
where changing them would falsify history.
