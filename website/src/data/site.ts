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
  { label: 'START', href: '/docs/getting-started/install' },
  { label: 'FIRST RUN', href: '/docs/getting-started/first-run' },
  { label: 'BUILD A GUEST', href: '/docs/getting-started/build-a-guest' },
  { label: 'RUNTIME', href: '/docs/concepts/runtime-model' },
  { label: 'CLI', href: '/docs/reference/cli' },
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
  { label: 'Blog', href: '/blog' },
  { label: 'Project status', href: '/docs/project/status' },
  { label: 'Source', href: repository },
  { label: 'Issues', href: `${repository}/issues` },
  { label: 'License', href: `${repository}/blob/master/LICENSE-MIT` },
];

/** The footer "jump to" menu — a period-correct navigation dropdown. */
export const jumpTargets = [
  { label: 'Requirements', href: '/docs/getting-started/requirements' },
  { label: 'Install Instar', href: '/docs/getting-started/install' },
  { label: 'Your first run', href: '/docs/getting-started/first-run' },
  { label: 'Build a guest', href: '/docs/getting-started/build-a-guest' },
  { label: 'Verify a download', href: '/docs/getting-started/verify' },
  { label: 'Runtime model', href: '/docs/concepts/runtime-model' },
  { label: 'Host-owned UI', href: '/docs/concepts/host-owned-ui' },
  { label: 'Architecture', href: '/docs/concepts/architecture' },
  { label: 'Failure and recovery', href: '/docs/concepts/failure-and-recovery' },
  { label: 'Command line', href: '/docs/reference/cli' },
  { label: 'UI vocabulary', href: '/docs/reference/ui-vocabulary' },
  { label: 'Wire protocol', href: '/docs/reference/protocol' },
  { label: 'Error taxonomy', href: '/docs/reference/errors' },
  { label: 'WIT contract', href: '/docs/reference/wit' },
  { label: 'Distribution', href: '/docs/reference/distribution' },
  { label: 'Troubleshooting', href: '/docs/reference/troubleshooting' },
  { label: 'Glossary', href: '/docs/reference/glossary' },
  { label: 'Build from source', href: '/docs/development/build-from-source' },
  { label: 'Tests and gates', href: '/docs/development/testing' },
  { label: 'Repository map', href: '/docs/development/repository-map' },
  { label: 'Contributing', href: '/docs/development/contributing' },
  { label: 'Project status', href: '/docs/project/status' },
  { label: 'Questions', href: '/docs/project/faq' },
];
