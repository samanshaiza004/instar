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
| `src/styles/portal.css` | The 780px portal canvas. |
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

The front page is deliberately built like a 2002 portal front page: a fixed
780px canvas, dense 8–11px Verdana, hard-coded hierarchy, 1px rules, aggressive
photographic crops, and modules laid out like small magazine advertisements.

One structural rule keeps it maintainable: nothing carrying text is positioned
absolutely inside a fixed-height box. The hero is a real three-column grid, the
photograph and the halftone dots are the only absolute layers, and the module
deck's overlap into the hero is paid for by reserved padding
(`--deck-overlap`). Copy can grow without colliding.

## Deploy

Netlify builds from this directory: base `website`, command `npm run build`,
publish `dist`. After deploying, check `/`, `/downloads`, `/docs/`, `/install`,
and `/install.ps1`.

No user-facing page should refer to the predecessor brand or repository. Old
names may remain in engineering archaeology and measured baseline documents
where changing them would falsify history.
