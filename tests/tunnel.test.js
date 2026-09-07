import test from 'node:test';
import assert from 'node:assert/strict';
import { TunnelManager } from '../src/tunnel/cloudflared.js';

const silent = {
  info() {}, warn() {}, error() {}, debug() {},
};

function managerWith(tunnel, serverPort = 8787) {
  const data = { tunnel: { binary: 'cloudflared', extraArgs: [], ...tunnel }, server: { port: serverPort } };
  return new TunnelManager({ config: { get: () => data }, logger: silent });
}

test('quick mode publishes only the relay port', () => {
  const args = managerWith({ mode: 'quick' }, 9001).buildArgs();
  assert.deepEqual(args, ['--no-autoupdate', 'tunnel', '--url', 'http://127.0.0.1:9001', '--protocol', 'http2']);
  assert.ok(!args.join(' ').includes('8788'), 'the dashboard port is never published');
});

test('named mode runs the token-backed tunnel', () => {
  const args = managerWith({ mode: 'named', token: 'eyJhIjoiabc' }).buildArgs();
  assert.deepEqual(args, ['--no-autoupdate', 'tunnel', 'run', '--token', 'eyJhIjoiabc']);
});

test('named mode falls back to a config file when no token is set', () => {
  const args = managerWith({ mode: 'named', configFile: '/home/u/.cloudflared/config.yml' }).buildArgs();
  assert.deepEqual(args, ['--no-autoupdate', '--config', '/home/u/.cloudflared/config.yml', 'tunnel', 'run']);
});

test('extra arguments are passed through', () => {
  const args = managerWith({ mode: 'quick', extraArgs: ['--loglevel', 'debug'] }).buildArgs();
  assert.ok(args.includes('--loglevel') && args.includes('debug'));
});

test('a missing binary is reported, not thrown as a crash', async () => {
  const manager = managerWith({ mode: 'quick', binary: 'definitely-not-installed-cloudflared' });
  const version = await manager.version();
  assert.equal(version.installed, false);
  await assert.rejects(() => manager.start(), /not found|Install it/i);
});

test('mode "off" refuses to start', async () => {
  await assert.rejects(() => managerWith({ mode: 'off' }).start(), /"off"/);
});

test('status is reportable before anything has run', () => {
  const status = managerWith({ mode: 'quick' }).status();
  assert.equal(status.state, 'stopped');
  assert.equal(status.url, '');
  assert.equal(status.pid, null);
  assert.deepEqual(status.logs, []);
});
