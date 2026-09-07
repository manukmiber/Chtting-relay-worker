import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Config } from './config.js';
import { Logger } from './logger.js';
import { openStore } from './store/index.js';
import { TokenCounter } from './tokenizer/index.js';
import { Upstream } from './relay/upstream.js';
import { RelayHandler } from './relay/handler.js';
import { RateLimiter } from './util/ratelimit.js';
import { createApiServer } from './server/api.js';
import { createDashboardServer } from './server/dashboard.js';
import { TunnelManager } from './tunnel/cloudflared.js';

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export function resolvePaths(overrides = {}) {
  const root = overrides.root ?? process.env.CHTTING_HOME ?? ROOT;
  return {
    root,
    config: overrides.config ?? process.env.CHTTING_CONFIG ?? path.join(root, 'config', 'config.json'),
    data: overrides.data ?? process.env.CHTTING_DATA ?? path.join(root, 'data'),
    logs: overrides.logs ?? path.join(overrides.data ?? path.join(root, 'data'), 'logs'),
    tokenizers: overrides.tokenizers ?? process.env.CHTTING_TOKENIZER_DIR ?? path.join(root, 'data', 'tokenizers'),
    public: overrides.public ?? path.join(root, 'public'),
  };
}

/**
 * Build the whole relay without listening yet, so tests can drive it in-process
 * and the CLI can decide which pieces to start.
 */
export async function createApp(overrides = {}) {
  const paths = resolvePaths(overrides);
  const config = await Config.load(paths.config);
  const cfg = config.get();

  const logger = new Logger({
    level: cfg.logging.level,
    dir: paths.logs,
    fileEnabled: cfg.logging.fileEnabled,
  });

  const store = await openStore(paths.data, logger);
  const counter = TokenCounter.create({
    dir: paths.tokenizers,
    rules: cfg.tokenizer.rules,
    fallback: cfg.tokenizer.fallback,
    logger,
  });
  const limiter = new RateLimiter();
  const upstream = new Upstream({ config, logger });
  const handler = new RelayHandler({ config, store, counter, upstream, logger, limiter });
  const tunnel = new TunnelManager({ config, logger });

  const relayServer = createApiServer({ config, handler, logger, store, counter });
  const dashboardServer = createDashboardServer({
    config, store, counter, tunnel, logger, publicDir: paths.public, paths, relayServer,
  });

  config.on('change', (next) => {
    counter.registry.setRules(next.tokenizer.rules, next.tokenizer.fallback);
    logger.setLevel(next.logging.level);
  });

  const timers = [];
  const app = {
    paths, config, logger, store, counter, upstream, handler, tunnel, limiter,
    relayServer, dashboardServer,

    async start() {
      const c = config.get();
      await listen(relayServer, c.server.port, c.server.host);
      logger.info(`relay API listening on http://${c.server.host}:${c.server.port}`);

      if (c.dashboard.enabled) {
        await listen(dashboardServer, c.dashboard.port, c.dashboard.host);
        logger.info(`dashboard on http://${c.dashboard.host}:${c.dashboard.port}`);
        if (!c.dashboard.password && c.dashboard.host !== '127.0.0.1') {
          logger.warn('dashboard has no password and is not bound to localhost - set dashboard.password');
        }
      }

      if (c.tunnel.autoStart && c.tunnel.mode !== 'off') {
        tunnel.start()
          .then((s) => { if (s.url) logger.info(`tunnel public URL: ${s.url}`); })
          .catch((err) => logger.error(`tunnel failed to start: ${err.message}`));
      }

      // housekeeping: trim old rows daily, and keep the limiter map bounded
      timers.push(setInterval(() => {
        Promise.resolve(store.prune(config.get().logging.retentionDays))
          .then((n) => { if (n) logger.info(`pruned ${n} old request rows`); })
          .catch((err) => logger.warn(`prune failed: ${err.message}`));
      }, 6 * 3600 * 1000));
      timers.push(setInterval(() => limiter.sweep(), 120000));
      for (const t of timers) t.unref?.();

      return app;
    },

    async stop() {
      for (const t of timers) clearInterval(t);
      await tunnel.stop().catch(() => {});
      await Promise.all([close(relayServer), close(dashboardServer)]);
      store.close();
      logger.close();
    },
  };

  return app;
}

function listen(server, port, host) {
  return new Promise((resolve, reject) => {
    const onError = (err) => {
      server.off('listening', onListening);
      reject(err.code === 'EADDRINUSE'
        ? new Error(`port ${port} is already in use - stop the other process or change the port`)
        : err);
    };
    const onListening = () => {
      server.off('error', onError);
      resolve(server);
    };
    server.once('error', onError);
    server.once('listening', onListening);
    server.listen(port, host);
  });
}

function close(server) {
  return new Promise((resolve) => {
    if (!server.listening) return resolve();
    server.closeAllConnections?.();
    return server.close(resolve);
  });
}
