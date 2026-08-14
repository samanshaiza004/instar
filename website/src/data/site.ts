/**
 * Facts and links shared by the portal pages and the documentation chrome, so
 * the masthead, the nav strip, and the docs header cannot drift apart.
 */

export const repository = 'https://github.com/samanshaiza004/instar';

export const site = {
  name: 'Instar',
  protocolVersion: 9,
  tagline: 'run the guest. own the machine.',
  description:
    'Instar runs WebAssembly Component Model applications in a native window. The guest describes meaning; the host owns geometry, input, accessibility, and pixels.',
  repository,
  issues: `${repository}/issues`,
  releases: `${repository}/releases`,
  installCommand: 'curl -fsSL https://instar.samanshaiza.com/install | sh',
  installCommandWindows:
    'irm https://instar.samanshaiza.com/install.ps1 | iex',
} as const;

/** The orange-arrow channel strip under the masthead. */
export const channels = [
  { label: 'GET STARTED', href: '/docs/getting-started/install' },
  { label: 'FIRST RUN', href: '/docs/getting-started/first-run' },
  { label: 'BUILD A GUEST', href: '/docs/getting-started/build-a-guest' },
  { label: 'RUNTIME', href: '/docs/concepts/runtime-model' },
  { label: 'DOWNLOADS', href: '/downloads' },
  { label: 'BLOG', href: '/blog' },
];

/** The specification ribbon. Every value here is checked against the repo. */
export const facts = [
  { label: 'HOST', value: 'macOS / Linux / Windows' },
  { label: 'GUEST', value: 'wasm32-wasip2' },
  { label: 'RUNTIME', value: 'Wasmtime 47.0.3' },
  { label: 'RUST', value: '1.97.1' },
  { label: 'WIRE', value: 'protocol 9' },
  { label: 'LICENSE', value: 'MIT OR Apache-2.0' },
];

/** Footer links, in portal order. */
export const footerLinks = [
  { label: 'Documentation', href: '/docs' },
  { label: 'Downloads', href: '/downloads' },
  { label: 'Project status', href: '/docs/project/status' },
  { label: 'Source', href: repository },
  { label: 'License', href: `${repository}/blob/master/LICENSE-MIT` },
];
