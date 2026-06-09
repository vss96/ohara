'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');

const { resolveVersion, sweepOldCaches } = require('../bin/ohara-mcp');

test('resolveVersion honours OHARA_PLUGIN_VERSION override', () => {
  process.env.OHARA_PLUGIN_VERSION = '1.2.3';
  try {
    assert.strictEqual(resolveVersion(), '1.2.3');
  } finally {
    delete process.env.OHARA_PLUGIN_VERSION;
  }
});

test('resolveVersion falls back to plugin.json', () => {
  delete process.env.OHARA_PLUGIN_VERSION;
  const manifest = JSON.parse(
    fs.readFileSync(path.join(__dirname, '..', '.claude-plugin', 'plugin.json'), 'utf8')
  );
  assert.strictEqual(resolveVersion(), manifest.version);
});

test('sweepOldCaches removes only other v* dirs', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ohara-sweep-'));
  fs.mkdirSync(path.join(root, 'v0.7.4'));
  fs.mkdirSync(path.join(root, 'v0.9.0'));
  fs.writeFileSync(path.join(root, 'not-a-version'), '');
  sweepOldCaches(root, 'v0.9.0');
  assert.ok(!fs.existsSync(path.join(root, 'v0.7.4')), 'old version dir removed');
  assert.ok(fs.existsSync(path.join(root, 'v0.9.0')), 'current kept');
  assert.ok(fs.existsSync(path.join(root, 'not-a-version')), 'non-version entries kept');
});
