import { spawn, execFile } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);
const QUICK_URL_RE = /https:\/\/[a-z0-9-]+\.trycloudflare\.com/i;

/**
 * Supervises a `cloudflared` child process so the relay on your phone gets a
 * public HTTPS URL without port forwarding.
 *
 * Three modes:
 *   quick  - throwaway *.trycloudflare.com URL, no Cloudflare account needed
 *   named  - a token-backed named tunnel bound to your own hostname
 *   config - a cloudflared config.yml you manage yourself
 *
 * Only the relay port is ever published; the dashboard binds to localhost and
 * is deliberately not routed through the tunnel.
 */
export class TunnelManager extends EventEmitter {
  constructor({ config, logger }) {
    super();
    this.config = config;
    this.logger = logger;
    this.proc = null;
    this.url = '';
    this.state = 'stopped'; // stopped | starting | running | failed
    this.lastError = '';
    this.startedAt = 0;
    this.lines = [];
    this.restarts = 0;
    this.stopping = false;
  }

  status() {
    return {
      state: this.state,
      url: this.url,
      mode: this.config.get().tunnel.mode,
      pid: this.proc?.pid ?? null,
      startedAt: this.startedAt,
      uptime_s: this.startedAt ? Math.round((Date.now() - this.startedAt) / 1000) : 0,
      restarts: this.restarts,
      lastError: this.lastError,
      logs: this.lines.slice(-200),
    };
  }

  async version() {
    const bin = this.config.get().tunnel.binary || 'cloudflared';
    try {
      const { stdout } = await execFileAsync(bin, ['--version'], { timeout: 10000 });
      return { installed: true, version: stdout.trim(), binary: bin };
    } catch (err) {
      return { installed: false, version: '', binary: bin, error: err.message };
    }
  }

  #log(line) {
    const text = String(line).trimEnd();
    if (!text) return;
    this.lines.push(`${new Date().toISOString()} ${text}`);
    if (this.lines.length > 500) this.lines.splice(0, this.lines.length - 500);

    const match = QUICK_URL_RE.exec(text);
    if (match && this.url !== match[0]) {
      this.url = match[0];
      this.state = 'running';
      this.logger.info(`cloudflared tunnel URL: ${this.url}`);
      this.emit('url', this.url);
    }
    if (/Registered tunnel connection|Connection .* registered/i.test(text) && this.state !== 'running') {
      this.state = 'running';
      this.emit('running');
    }
    this.emit('log', text);
  }

  buildArgs() {
    const t = this.config.get().tunnel;
    const port = this.config.get().server.port;
    const base = ['--no-autoupdate'];

    if (t.mode === 'named' && t.token) {
      return [...base, 'tunnel', 'run', '--token', t.token, ...(t.extraArgs ?? [])];
    }
    if (t.mode === 'named' && t.configFile) {
      return [...base, '--config', t.configFile, 'tunnel', 'run', ...(t.extraArgs ?? [])];
    }
    return [
      ...base,
      'tunnel',
      '--url', `http://127.0.0.1:${port}`,
      // trycloudflare needs no credentials but does need a protocol it can use
      // on mobile networks; http2 survives carrier NAT better than quic.
      '--protocol', 'http2',
      ...(t.extraArgs ?? []),
    ];
  }

  async start() {
    if (this.proc) return this.status();
    const t = this.config.get().tunnel;
    if (t.mode === 'off') throw Object.assign(new Error('tunnel mode is "off"'), { status: 400 });

    const check = await this.version();
    if (!check.installed) {
      throw Object.assign(
        new Error(`cloudflared not found (tried "${check.binary}"). Install it with: pkg install cloudflared`),
        { status: 400 },
      );
    }

    const args = this.buildArgs();
    this.state = 'starting';
    this.url = '';
    this.lastError = '';
    this.stopping = false;
    this.startedAt = Date.now();
    this.#log(`starting: ${t.binary || 'cloudflared'} ${args.map(redactArg).join(' ')}`);

    const proc = spawn(t.binary || 'cloudflared', args, {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, TUNNEL_METRICS: '' },
    });
    this.proc = proc;

    const onData = (buf) => {
      for (const line of buf.toString('utf8').split('\n')) this.#log(line);
    };
    proc.stdout.on('data', onData);
    proc.stderr.on('data', onData);

    proc.on('exit', (code, signal) => {
      this.proc = null;
      const wasRunning = this.state === 'running';
      this.state = this.stopping ? 'stopped' : 'failed';
      this.url = '';
      this.#log(`cloudflared exited (code=${code} signal=${signal})`);
      if (!this.stopping) {
        this.lastError = `cloudflared exited with code ${code}`;
        this.emit('exit', code);
        // Mobile links drop; bring the tunnel back unless it never worked.
        if (wasRunning && this.config.get().tunnel.autoStart) {
          const delay = Math.min(30000, 2000 * 2 ** Math.min(this.restarts, 4));
          this.restarts += 1;
          this.#log(`restarting in ${delay}ms (attempt ${this.restarts})`);
          setTimeout(() => { this.start().catch((e) => this.#log(`restart failed: ${e.message}`)); }, delay);
        }
      }
    });

    proc.on('error', (err) => {
      this.lastError = err.message;
      this.state = 'failed';
      this.#log(`spawn error: ${err.message}`);
    });

    if (t.mode !== 'named') {
      // Quick tunnels print their URL within a few seconds; surface it to the
      // caller so the dashboard can show a link immediately.
      await this.waitForUrl(20000).catch(() => {});
    }
    return this.status();
  }

  waitForUrl(timeoutMs) {
    if (this.url) return Promise.resolve(this.url);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.off('url', onUrl);
        reject(new Error('timed out waiting for the tunnel URL'));
      }, timeoutMs);
      const onUrl = (url) => {
        clearTimeout(timer);
        resolve(url);
      };
      this.once('url', onUrl);
    });
  }

  async stop() {
    this.stopping = true;
    const proc = this.proc;
    if (!proc) {
      this.state = 'stopped';
      return this.status();
    }
    proc.kill('SIGTERM');
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        try { proc.kill('SIGKILL'); } catch { /* already gone */ }
        resolve();
      }, 5000);
      proc.once('exit', () => { clearTimeout(timer); resolve(); });
    });
    this.proc = null;
    this.state = 'stopped';
    this.url = '';
    return this.status();
  }

  async restart() {
    await this.stop();
    this.restarts = 0;
    return this.start();
  }
}

function redactArg(arg) {
  return arg.length > 40 && /^[A-Za-z0-9+/=_-]+$/.test(arg) ? `${arg.slice(0, 6)}…` : arg;
}
