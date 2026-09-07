import path from 'node:path';
import { SqliteStore } from './sqlite.js';
import { JsonlStore } from './jsonl.js';

/**
 * Pick the best storage engine the runtime offers. `node:sqlite` ships with
 * Node 22+, which is what `pkg install nodejs` gives you on Termux; anything
 * older transparently falls back to append-only JSONL.
 */
export async function openStore(dataDir, logger = console) {
  try {
    await import('node:sqlite');
    const store = await SqliteStore.open(path.join(dataDir, 'metrics.db'));
    logger.info?.(`metrics store: sqlite (${store.file})`);
    return store;
  } catch (err) {
    const store = await JsonlStore.open(path.join(dataDir, 'metrics.jsonl'));
    logger.warn?.(`node:sqlite unavailable (${err.message}); using JSONL store at ${store.file}`);
    return store;
  }
}

export { SqliteStore, JsonlStore };
