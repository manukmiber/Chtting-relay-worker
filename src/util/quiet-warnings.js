/**
 * `node:sqlite` is still flagged experimental and prints a warning on first
 * use. In a relay that runs for days that is pure noise, so exactly that one
 * warning is dropped and everything else is passed through untouched.
 *
 * Imported first by the CLI: ES module imports are hoisted, so this runs
 * before any module that might touch node:sqlite.
 */
const passthrough = process.listeners('warning');
process.removeAllListeners('warning');
process.on('warning', (warning) => {
  if (warning.name === 'ExperimentalWarning' && /SQLite/i.test(warning.message)) return;
  for (const listener of passthrough) listener(warning);
});
