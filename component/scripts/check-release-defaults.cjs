// Keep the supported component build policy consistent across local builds and CI.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '..');
const read = name => fs.readFileSync(path.join(root, name), 'utf8');
const config = JSON.parse(read('ui/tauri.conf.json'));
const version = file => read(file).match(/^version\s*=\s*"([^"]+)"/m)?.[1];
assert.equal(version('addin/Cargo.toml'), config.version, 'DLL and Tauri versions differ');
assert.equal(version('ui/Cargo.toml'), config.version, 'UI and Tauri versions differ');
assert.match(read('.cargo/config.toml'), /target-feature=\+crt-static/, 'Static CRT must be enabled');
assert.match(read('ui/Cargo.toml'), /^tauri\s*=.*"devtools"/m, 'Release DevTools feature must be enabled');
for (const window of config.app.windows) {
  assert.equal(window.useHttpsScheme, true, 'HTTPS origin must be enabled');
  assert.equal(window.devtools, true, 'DevTools must be enabled');
  assert.equal(window.dataDirectory, undefined, 'Authentication profile must be assigned at runtime');
}
const frameSrc = config.app.security.csp.split(';').map(s => s.trim()).find(s => s.startsWith('frame-src '));
for (const origin of ['https://analytics.easy-busy.cloud', 'https://www.youtube.com']) {
  assert.ok(frameSrc?.split(/\s+/).includes(origin), `Missing iframe permission: ${origin}`);
}
console.log(`PASS: ${config.version} default policy: static CRT, HTTPS, analytics/YouTube iframe, DevTools`);
