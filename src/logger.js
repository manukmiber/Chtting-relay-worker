import { createWriteStream, mkdirSync } from 'node:fs';
import path from 'node:path';

const LEVELS = { debug: 10, info: 20, warn: 30, error: 40, silent: 100 };
const COLORS = { debug: '[90m', info: '[36m', warn: '[33m', error: '[31m' };
const RESET = '[0m';

/**
 * Console + file logger. Termux sessions get killed a lot, so the file log is
 * what you read after the fact; it keeps the last `keepBytes` and rolls over
 * rather than growing without bound on a phone.
 */
export class Logger {
  constructor({ level = 'info', dir = null, fileEnabled = true, color = process.stdout.isTTY } = {}) {
    this.level = LEVELS[level] ?? LEVELS.info;
    this.color = color;
    this.stream = null;
    if (dir && fileEnabled) {
      try {
        mkdirSync(dir, { recursive: true });
        this.file = path.join(dir, 'relay.log');
        this.stream = createWriteStream(this.file, { flags: 'a' });
      } catch { /* logging to disk is best effort */ }
    }
  }

  setLevel(level) {
    this.level = LEVELS[level] ?? this.level;
  }

  #write(level, args) {
    if ((LEVELS[level] ?? 0) < this.level) return;
    const ts = new Date().toISOString();
    const msg = args.map(fmt).join(' ');
    const line = `${ts} ${level.toUpperCase().padEnd(5)} ${msg}`;
    const out = level === 'error' || level === 'warn' ? process.stderr : process.stdout;
    out.write(this.color ? `${COLORS[level] ?? ''}${line}${RESET}\n` : `${line}\n`);
    this.stream?.write(`${line}\n`);
  }

  debug(...a) { this.#write('debug', a); }
  info(...a) { this.#write('info', a); }
  warn(...a) { this.#write('warn', a); }
  error(...a) { this.#write('error', a); }

  close() {
    this.stream?.end();
  }
}

function fmt(v) {
  if (typeof v === 'string') return v;
  if (v instanceof Error) return v.stack ?? v.message;
  try { return JSON.stringify(v); } catch { return String(v); }
}
