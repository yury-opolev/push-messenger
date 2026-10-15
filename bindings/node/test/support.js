'use strict';

// What the tests share: the fake servers, an in-memory secret store and an event queue.

const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');

const targetDir = process.env.CARGO_TARGET_DIR || path.join(__dirname, '..', '..', '..', 'target');

/**
 * Starts push-sdk-fakes (built by `npm test`): a fake auth-svc and a gateway that answers by
 * itself. Resolves to their URLs and the identity they sign everyone in as.
 */
function startFakes() {
  const program = path.join(targetDir, 'debug', process.platform === 'win32' ? 'push-sdk-fakes.exe' : 'push-sdk-fakes');
  const child = spawn(program, [], { stdio: ['pipe', 'pipe', 'inherit'] });
  return new Promise((resolve, reject) => {
    child.once('error', reject);
    readline.createInterface({ input: child.stdout }).once('line', (line) => {
      const fakes = JSON.parse(line);
      // Closing its input ends it.
      fakes.stop = () => child.stdin.end();
      resolve(fakes);
    });
  });
}

/**
 * A secret store in memory, answering asynchronously after `delayMs`, as a keychain module
 * does. `failNext` makes the next save of a key throw (synchronously, or with `async: true`
 * reject).
 */
function memoryStore({ delayMs = 0, loaded } = {}) {
  const store = {
    values: new Map(),
    calls: [],
    failNext: null,
    later: () => new Promise((resolve) => setTimeout(resolve, delayMs)),
    async load(key) {
      store.calls.push({ op: 'load', key });
      await store.later();
      // `loaded` makes every load answer with that value, bytes or not.
      return loaded !== undefined ? loaded : (store.values.get(key) ?? null);
    },
    save(key, value) {
      store.calls.push({ op: 'save', key });
      if (store.failNext && store.failNext.key === key) {
        const failure = store.failNext;
        store.failNext = null;
        if (!failure.async) throw new Error('the keychain is locked');
        return store.later().then(() => Promise.reject(new Error('the keychain is locked')));
      }
      return store.later().then(() => {
        store.values.set(key, Buffer.from(value));
      });
    },
    async delete(key) {
      store.calls.push({ op: 'delete', key });
      await store.later();
      store.values.delete(key);
    },
  };
  return store;
}

/** Collects a client's events; `next(match)` resolves to the first one `match` accepts. */
function eventQueue(client) {
  const seen = [];
  const waiting = [];
  client.onEvent((event) => {
    seen.push(event);
    for (const waiter of [...waiting]) {
      if (waiter.match(event)) {
        waiting.splice(waiting.indexOf(waiter), 1);
        waiter.resolve(event);
      }
    }
  });
  return {
    seen,
    next(match, timeoutMs = 10000) {
      const found = seen.find(match);
      if (found) return Promise.resolve(found);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('no such event in time')), timeoutMs);
        waiting.push({ match, resolve: (event) => { clearTimeout(timer); resolve(event); } });
      });
    },
  };
}

/** A directory of its own for database files, under cargo's target directory. */
function scratchDir(name) {
  const dir = path.join(targetDir, 'tmp', 'push-sdk-node', name);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

module.exports = { startFakes, memoryStore, eventQueue, scratchDir };
