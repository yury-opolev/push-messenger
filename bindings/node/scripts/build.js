'use strict';

// Builds the addon with cargo and copies it next to index.js as push_sdk.node.
// `--release` builds optimised; `--with-fakes` also builds push-sdk-fakes, the fake auth-svc
// and gateway the tests run against.

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..', '..', '..');
const release = process.argv.includes('--release');
const profile = release ? 'release' : 'debug';
const cargo = (args) =>
  execFileSync('cargo', [...args, ...(release ? ['--release'] : [])], { cwd: root, stdio: 'inherit' });

cargo(['build', '--locked', '-p', 'push-sdk-node']);
if (process.argv.includes('--with-fakes')) {
  cargo(['build', '--locked', '-p', 'push-sdk', '--features', 'testing', '--bin', 'push-sdk-fakes']);
}

const targetDir = process.env.CARGO_TARGET_DIR || path.join(root, 'target');
const library = {
  win32: 'push_sdk_node.dll',
  darwin: 'libpush_sdk_node.dylib',
}[process.platform] || 'libpush_sdk_node.so';
fs.copyFileSync(path.join(targetDir, profile, library), path.join(__dirname, '..', 'push_sdk.node'));
