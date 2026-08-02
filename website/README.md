# youth.samanshaiza.com

This directory is the source for Youth's website, deployed on Netlify as a
subdomain of samanshaiza.com. It lives in the main repo (not a separate
one) so documentation changes ship in the same PR as the code change that
motivated them -- see the commit that added this file for the reasoning.

## Layout

- `public/` -- static files copied as-is into the deploy: the landing page
  (`index.html`) and the install script (`install.sh`, currently a
  placeholder -- see below).
- `build.sh` -- combines `public/` with a fresh `mdbook build` of
  `docs/book/` (the developer guide) into `website/dist/`, which is what
  actually gets published. `docs/book/` is the single source of truth for
  the documentation; nothing here duplicates its content.
- `../netlify.toml` (repo root, not in this directory) -- Netlify's build
  config: build command, publish directory, and an `ignore` hook so
  unrelated commits elsewhere in the repo don't trigger a redeploy.

## Building locally

```bash
bash website/build.sh
```

Output lands in `website/dist/` (gitignored). Open `website/dist/index.html`
directly, or serve the directory with anything static
(`python3 -m http.server --directory website/dist`).

With the [Netlify CLI](https://docs.netlify.com/cli/get-started/) installed
and the site linked (`netlify link`), `netlify dev` runs the same build
through `netlify.toml` and serves it with redirects/headers applied exactly
as production would.

## Connecting this to Netlify (one-time setup)

1. Netlify dashboard -> **Add new site -> Import an existing project** ->
   select this GitHub repo. Leave the base directory at its default (the
   repo root); `netlify.toml` there already points at
   `bash website/build.sh` / `website/dist`.
2. **Domain settings -> Add domain alias** -> `youth.samanshaiza.com`.
   Netlify shows the exact DNS record (a CNAME, or one click if
   samanshaiza.com's DNS is already delegated to Netlify) to add wherever
   samanshaiza.com's DNS is managed.
3. Push to `master` (or trigger a deploy) and confirm `/`, `/docs/`, and
   `/install.sh` all resolve once DNS propagates.

## The install script is a placeholder

`public/install.sh` intentionally does not install anything yet -- Youth
has no tagged release with prebuilt binaries (see
[docs/DISTRIBUTION.md](../docs/DISTRIBUTION.md)). It prints guidance and
exits non-zero. Once the first real `dist` release ships, replace it with
either a copy of `dist`'s generated `youth-cli-installer.sh`, or -- the
lower-maintenance option, since it can never drift from what `dist` actually
publishes -- a `netlify.toml` redirect straight to
`https://github.com/samanshaiza004/youth/releases/latest/download/youth-cli-installer.sh`.
