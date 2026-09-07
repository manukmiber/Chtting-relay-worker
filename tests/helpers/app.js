import { mkdtemp, rm, mkdir, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createApp } from '../../src/index.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');

/** Boot a fully wired relay in a throwaway directory, on random free ports. */
export async function startTestApp(configPatch = {}) {
  const dir = await mkdtemp(path.join(tmpdir(), 'chtting-test-'));
  await mkdir(path.join(dir, 'config'), { recursive: true });
  await mkdir(path.join(dir, 'data'), { recursive: true });

  const configFile = path.join(dir, 'config', 'config.json');
  await writeFile(configFile, JSON.stringify({
    server: { host: '127.0.0.1', port: 0 },
    dashboard: { enabled: true, host: '127.0.0.1', port: 0, password: '' },
    logging: { level: 'silent', fileEnabled: false },
    ...configPatch,
  }));

  const app = await createApp({
    config: configFile,
    data: path.join(dir, 'data'),
    tokenizers: path.join(ROOT, 'data', 'tokenizers'),
    public: path.join(ROOT, 'public'),
  });
  await app.start();

  const relayPort = app.relayServer.address().port;
  const dashPort = app.dashboardServer.address().port;

  return {
    app,
    dir,
    relayUrl: `http://127.0.0.1:${relayPort}`,
    dashboardUrl: `http://127.0.0.1:${dashPort}`,
    async close() {
      await app.stop();
      await rm(dir, { recursive: true, force: true });
    },
  };
}
