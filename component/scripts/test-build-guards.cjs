// Runs the actual compiled build script with invalid inputs; no UI is built.
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '..');
const targetRoot = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
const buildRoot = path.join(targetRoot, 'debug/build');
const candidates = fs.readdirSync(buildRoot).filter(name => name.startsWith('easy_busy_chat_addin-'))
  .map(name => path.join(buildRoot, name, 'build-script-build.exe')).filter(p => fs.existsSync(p))
  .sort((a,b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs);
assert.ok(candidates[0], 'Build the add-in in debug mode before checking build guards');
const out = path.join(targetRoot, 'build-guard-test');
fs.mkdirSync(out, { recursive: true });
const invalid = path.join(out, 'invalid.exe');
fs.writeFileSync(invalid, 'not an executable');
const wrongArch = path.join(out, 'x86-header.exe');
const header = Buffer.alloc(128);
header.write('MZ'); header.writeUInt32LE(64,60); header.write('PE\0\0',64); header.writeUInt16LE(0x014c,68);
fs.writeFileSync(wrongArch, header);
function reject(name, overrides, message) {
  const env = { ...process.env, CARGO_MANIFEST_DIR: path.join(root,'addin'), OUT_DIR: out,
    TARGET: 'x86_64-pc-windows-msvc', PROFILE: 'debug', EASYBUSYCHAT_SKIP_UI_BUILD:'0', EASYBUSYCHAT_UI_EXE:invalid, ...overrides };
  const result = spawnSync(candidates[0], [], { env, cwd: root, encoding:'utf8' });
  assert.equal(result.error, undefined);
  assert.notEqual(result.status, 0, name);
  assert.ok(result.stderr.includes(message), `${name}: unexpected diagnostic ${result.stderr}`);
  console.log('PASS '+name);
}
reject('release forbids placeholder', { PROFILE:'release', EASYBUSYCHAT_SKIP_UI_BUILD:'1' }, 'UI placeholder is forbidden');
reject('SKIP=0 still validates executable', {}, 'UI artifact is not an executable');
reject('x64 DLL rejects x86 UI', { EASYBUSYCHAT_UI_EXE:wrongArch }, 'UI artifact architecture does not match DLL');
