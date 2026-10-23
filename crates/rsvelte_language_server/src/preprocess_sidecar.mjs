import { createRequire } from 'node:module';
import { createInterface } from 'node:readline';
import { existsSync, readFileSync, realpathSync } from 'node:fs';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

// In each directory a `vite.config.*` that configures SvelteKit or
// vite-plugin-svelte wins over a `svelte.config.*`, as in `@sveltejs/load-config`,
// which svelte-language-server uses: SvelteKit 2.62+ and vite-plugin-svelte take
// their options (preprocess included) inline in the Vite config.
const VITE_CONFIG_NAMES = [
  'vite.config.js',
  'vite.config.mjs',
  'vite.config.ts',
  'vite.config.cjs',
  'vite.config.mts',
  'vite.config.cts',
];
const SVELTE_CONFIG_NAMES = [
  'svelte.config.js',
  'svelte.config.mjs',
  'svelte.config.cjs',
  'svelte.config.ts',
  'svelte.config.mts',
];

const configs = new Map();
const compilers = new Map();
const protocolWrite = process.stdout.write.bind(process.stdout);

// Config files routinely log while loading. Keep fd 1 recoverable for framing.
process.stdout.write = function divertedStdout(chunk, encoding, callback) {
  return process.stderr.write(chunk, encoding, callback);
};

function send(value) {
  protocolWrite(`\u001eRSVELTE${JSON.stringify(value)}\n`);
}

function insideRoot(root, candidate) {
  const fromRoot = relative(root, candidate);
  return !(
    fromRoot === '..' ||
    fromRoot.startsWith(`..${process.platform === 'win32' ? '\\' : '/'}`) ||
    isAbsolute(fromRoot)
  );
}

function firstExisting(directory, names, root) {
  for (const name of names) {
    const candidate = join(directory, name);
    if (!existsSync(candidate)) continue;
    const canonical = realpathSync(candidate);
    if (insideRoot(root, canonical)) return canonical;
  }
  return null;
}

/// The nearest config with Svelte options, walking up from the component within
/// the workspace: per directory a Vite config whose Svelte plugin has options,
/// else a Svelte config.
async function findConfig(filename, workspace) {
  const root = realpathSync(resolve(workspace));
  let current = realpathSync(dirname(resolve(filename)));
  while (current === root || current.startsWith(`${root}/`) || current.startsWith(`${root}\\`)) {
    const vite = firstExisting(current, VITE_CONFIG_NAMES, root);
    // Only a Vite config that names preprocessors is resolved (the server applies
    // the same test before it asks): resolving loads Vite and every plugin.
    if (vite && readFileSync(vite, 'utf8').includes('preprocess')) {
      const options = await loadViteConfig(current, vite);
      if (options) return { path: vite, config: options };
    }
    const svelte = firstExisting(current, SVELTE_CONFIG_NAMES, root);
    if (svelte) return { path: svelte, config: await loadSvelteConfig(svelte) };
    const parent = dirname(current);
    if (parent === current) break;
    current = parent;
  }
  return null;
}

async function loadSvelteConfig(path) {
  let cached = configs.get(path);
  if (!cached) {
    cached = import(pathToFileURL(path).href).then((module) => module.default ?? module);
    configs.set(path, cached);
  }
  return cached;
}

function exportPath(entry) {
  if (typeof entry === 'string') return entry;
  if (!entry || typeof entry !== 'object') return undefined;
  for (const condition of ['import', 'default', 'node']) {
    const found = exportPath(entry[condition]);
    if (found) return found;
  }
  return undefined;
}

/// The project's own Vite, resolved from the config's directory.
async function importVite(root) {
  const require = createRequire(join(root, '__rsvelte_preprocess__.cjs'));
  const manifest = require.resolve('vite/package.json');
  const pkg = JSON.parse(readFileSync(manifest, 'utf8'));
  const entry = exportPath(pkg.exports?.['.']) ?? pkg.module ?? pkg.main;
  if (!entry) return null;
  return import(pathToFileURL(join(dirname(manifest), entry)).href);
}

/// The Svelte options of a Vite config, as `@sveltejs/load-config` reads them:
/// resolve the config with the project's Vite and take the options the SvelteKit
/// or vite-plugin-svelte plugin exposes. `null` when it has no Svelte plugin or
/// cannot be loaded, so the directory's Svelte config (if any) is used instead.
async function loadViteConfig(root, configFile) {
  let cached = configs.get(configFile);
  if (!cached) {
    cached = (async () => {
      let vite;
      try {
        vite = await importVite(root);
      } catch {
        return null;
      }
      if (!vite?.resolveConfig) return null;
      // SvelteKit < 3 looks its config up from the cwd. Requests are handled one
      // at a time, so changing it here cannot race another resolution.
      const cwd = process.cwd();
      try {
        process.chdir(root);
      } catch {}
      try {
        const resolved = await vite.resolveConfig({ root, configFile, logLevel: 'error' }, 'serve');
        const plugins = resolved?.plugins ?? [];
        return (
          plugins.find((plugin) => plugin.name === 'vite-plugin-sveltekit-setup')?.api?.options ??
          plugins.find((plugin) => plugin.name === 'vite-plugin-svelte:config')?.api?.options ??
          null
        );
      } catch (error) {
        process.stderr.write(
          `rsvelte preprocess: could not load ${configFile}: ${error instanceof Error ? error.message : error}\n`,
        );
        return null;
      } finally {
        try {
          process.chdir(cwd);
        } catch {}
      }
    })();
    configs.set(configFile, cached);
  }
  return cached;
}

async function loadCompiler(workspace) {
  let cached = compilers.get(workspace);
  if (!cached) {
    cached = (async () => {
      const require = createRequire(join(resolve(workspace), '__rsvelte_preprocess__.cjs'));
      for (const specifier of ['svelte/compiler', '@rsvelte/vite-plugin-svelte-native']) {
        try {
          const imported = await import(pathToFileURL(require.resolve(specifier)).href);
          // npm ships `svelte/compiler` as a CJS bundle, whose namespace carries
          // `default` alone — reading `preprocess` off it is undefined for every
          // real project, so this loop always fell through to the throw below.
          const module = typeof imported.preprocess === 'function' ? imported : imported.default;
          if (typeof module?.preprocess === 'function') return module;
        } catch {}
      }
      throw new Error(
        `Cannot resolve svelte/compiler or @rsvelte/vite-plugin-svelte-native from ${workspace}`,
      );
    })();
    compilers.set(workspace, cached);
  }
  return cached;
}

function normalizeMap(map) {
  if (!map) return null;
  if (typeof map === 'string') return map;
  if (typeof map.toString === 'function') {
    const stringified = map.toString();
    if (stringified !== '[object Object]') return stringified;
  }
  return JSON.stringify(map);
}

async function preprocess(request) {
  const found = await findConfig(request.filename, request.workspace);
  const configPath = found?.path ?? null;
  if (!found) {
    return {
      type: 'result',
      id: request.id,
      filename: request.filename,
      version: request.version,
      code: request.text,
      map: null,
      configPath: null,
      hasPreprocessor: false,
    };
  }

  const groups = found.config?.preprocess;
  if (!groups) {
    return {
      type: 'result',
      id: request.id,
      filename: request.filename,
      version: request.version,
      code: request.text,
      map: null,
      configPath,
      hasPreprocessor: false,
    };
  }

  const compiler = await loadCompiler(request.workspace);
  const result = await compiler.preprocess(request.text, groups, {
    filename: request.filename,
  });
  return {
    type: 'result',
    id: request.id,
    filename: request.filename,
    version: request.version,
    code: result?.code ?? String(result ?? ''),
    map: normalizeMap(result?.map),
    dependencies: Array.isArray(result?.dependencies) ? result.dependencies : [],
    configPath,
    hasPreprocessor: true,
  };
}

send({ type: 'ready', pid: process.pid });

let queue = Promise.resolve();
createInterface({ input: process.stdin, crlfDelay: Infinity }).on('line', (line) => {
  queue = queue.then(async () => {
    let request;
    try {
      request = JSON.parse(line);
      if (request?.type !== 'preprocess') return;
      send(await preprocess(request));
    } catch (error) {
      send({
        type: 'error',
        id: request?.id ?? null,
        filename: request?.filename ?? null,
        version: request?.version ?? null,
        message: error instanceof Error ? error.message : String(error),
        stack: error instanceof Error ? error.stack : undefined,
      });
    }
  });
});
