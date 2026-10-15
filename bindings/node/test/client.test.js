'use strict';

// The client's core through Node: opening, the secret store bridge, sign-in, the
// connection, events and errors, against push-sdk's fake auth-svc and gateway.

const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const net = require('node:net');
const path = require('node:path');

const { Client, PushError, deriveDatabaseKey, newSalt, SECRET_KEYS } = require('..');
const { startFakes, memoryStore, eventQueue, scratchDir } = require('./support');

let fakes;
before(async () => {
  fakes = await startFakes();
});
after(() => fakes.stop());

const config = (extra = {}) => ({
  authUrl: fakes.authUrl,
  socketUrl: fakes.socketUrl,
  databasePath: ':memory:',
  ...extra,
});

const code = (expected) => (error) => {
  assert.ok(error instanceof PushError, `a PushError, not ${error}`);
  assert.equal(error.code, expected);
  return true;
};

test('signs in through an asynchronous store while the JavaScript thread keeps working', async () => {
  const store = memoryStore({ delayMs: 20 });
  const client = await Client.open(config(), store);
  assert.equal(client.account(), null);
  let synchronousCalls = 0;
  // While an SDK thread waits for the store's promises, the JavaScript thread runs the
  // client's synchronous calls: nothing waits for the other.
  const ticker = setInterval(() => {
    client.account();
    client.connectionState();
    synchronousCalls += 1;
  }, 1);
  const account = await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  clearInterval(ticker);
  assert.equal(account.userId, fakes.userId);
  assert.deepEqual(client.account(), account);
  assert.ok(synchronousCalls > 0);
  assert.ok(store.values.has('push.session'), 'the session is in the app store');
  assert.ok(!store.values.has('push.registration'), 'the registration is done');
  await client.close();

  // Opened again, the client loads the session from the store before it answers.
  const again = await Client.open(config(), store);
  assert.deepEqual(again.account(), account);
  await again.close();
});

test('a store that fails a save fails the sign-in with code secrets, and a retry succeeds', async () => {
  for (const async of [false, true]) {
    const store = memoryStore();
    const client = await Client.open(config(), store);
    store.failNext = { key: 'push.session', async };
    await assert.rejects(
      client.signIn('a@example.com', 'password', 'node test', 'desktop'),
      (error) => code('secrets')(error) && error.retryable === false,
    );
    assert.ok(store.values.has('push.registration'), 'a retry names the same registration');
    const account = await client.signIn('a@example.com', 'password', 'node test', 'desktop');
    assert.equal(account.userId, fakes.userId);
    assert.ok(store.values.has('push.session'));
    await client.close();
  }
});

test('connects and reports the states as events, in order', async (t) => {
  const client = await Client.open(config(), memoryStore());
  // A listener keeps Node running until the client is closed, even when the test fails.
  t.after(() => client.close());
  const events = eventQueue(client);
  await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  client.connect();
  assert.equal(client.connectionState().state, 'connecting');
  await events.next((event) => event.type === 'connectionStateChanged' && event.state === 'connected');
  await client.waitConnected(5000);
  const states = events.seen.filter((event) => event.type === 'connectionStateChanged').map((event) => event.state);
  assert.deepEqual(states.slice(0, 2), ['connecting', 'connected']);
  client.disconnect();
  await events.next((event) => event.type === 'connectionStateChanged' && event.state === 'offline');
  await client.close();
  await assert.rejects(client.signOut(), code('closed'));
  assert.throws(() => client.connect(), code('closed'));
});

test('errors carry the SDK codes', async () => {
  const client = await Client.open(config(), memoryStore());
  await assert.rejects(
    client.signIn('a@example.com', 'wrong password', 'node test', 'desktop'),
    (error) => code('invalid_credentials')(error) && error.retryable === false,
  );
  await assert.rejects(client.signIn('a@example.com', 'password', 'node test', 'toaster'), code('invalid'));
  await assert.rejects(client.waitConnected(50), code('offline'));
  await client.close();
  await assert.rejects(Client.open(config({ databaseKey: new Uint8Array(31) }), memoryStore()), code('invalid'));
  await assert.rejects(Client.open(config({ databaseFile: 'x' }), memoryStore()), code('invalid'));
  await assert.rejects(Client.open(config(), { load() {} }), code('invalid'));
});

test('an encrypted database opens only with its key', async () => {
  const salt = newSalt();
  assert.equal(salt.length, 16);
  const key = await deriveDatabaseKey('2468', salt);
  assert.equal(key.length, 32);
  assert.deepEqual(await deriveDatabaseKey('2468', salt), key);
  const wrong = await deriveDatabaseKey('1357', salt);
  const databasePath = path.join(scratchDir('key'), 'encrypted.db');
  const first = await Client.open(config({ databasePath, databaseKey: key }), memoryStore());
  await first.close();
  await assert.rejects(Client.open(config({ databasePath, databaseKey: wrong }), memoryStore()), code('wrong_key'));
  await assert.rejects(Client.open(config({ databasePath }), memoryStore()), code('wrong_key'));
  const again = await Client.open(config({ databasePath, databaseKey: key }), memoryStore());
  await again.changeDatabaseKey(wrong);
  await again.close();
  const changed = await Client.open(config({ databasePath, databaseKey: wrong }), memoryStore());
  await changed.close();
  assert.deepEqual([...SECRET_KEYS], ['push.session', 'push.device', 'push.registration']);
});

test('a closed client lets Node exit', () => {
  // A listener keeps Node running like an open socket; close() releases it.
  const script = `
    const { Client } = require(${JSON.stringify(path.join(__dirname, '..'))});
    const values = new Map();
    const store = { load: (k) => values.get(k), save: (k, v) => { values.set(k, v); }, delete: (k) => { values.delete(k); } };
    (async () => {
      const client = await Client.open({ authUrl: ${JSON.stringify(fakes.authUrl)}, socketUrl: ${JSON.stringify(fakes.socketUrl)}, databasePath: ':memory:' }, store);
      client.onEvent(() => {});
      await client.signIn('a@example.com', 'password', 'exit test', 'desktop');
      client.connect();
      await client.waitConnected(5000);
      await client.close();
      console.log('closed');
    })();
  `;
  const output = execFileSync(process.execPath, ['-e', script], { timeout: 15000, encoding: 'utf8' });
  assert.match(output, /closed/);
});

test('close() while a call is pending resolves quickly and the call rejects with closed', async () => {
  const client = await Client.open(config({ socketUrl: 'ws://127.0.0.1:1/' }), memoryStore());
  await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  client.connect();
  const pending = client.waitConnected(30000);
  const rejected = assert.rejects(pending, code('closed'));
  await new Promise((resolve) => setTimeout(resolve, 100));
  const started = Date.now();
  await client.close();
  assert.ok(Date.now() - started < 2000, 'close() does not wait for the pending call');
  await rejected;
  await assert.rejects(client.waitConnected(50), code('closed'));
});
test('close() while a sign-in waits for a server that never answers resolves, and the database opens again', async (t) => {
  const sockets = new Set();
  const silent = net.createServer((socket) => {
    sockets.add(socket);
    socket.on('error', () => {});
    socket.on('close', () => sockets.delete(socket));
  });
  await new Promise((resolve) => silent.listen(0, '127.0.0.1', resolve));
  t.after(() => {
    for (const socket of sockets) socket.destroy();
    silent.close();
  });
  const databasePath = path.join(scratchDir('close-in-flight'), 'client.db');
  const stalled = config({ authUrl: `http://127.0.0.1:${silent.address().port}`, databasePath });
  const client = await Client.open(stalled, memoryStore());
  const signIn = client.signIn('a@example.com', 'password', 'node test', 'desktop');
  const rejected = assert.rejects(signIn, code('closed'));
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.equal(sockets.size, 1, 'the request reached the server');
  const started = Date.now();
  await client.close();
  assert.ok(Date.now() - started < 4000, 'close() does not wait for the request');
  await rejected;
  const again = await Client.open(config({ databasePath }), memoryStore());
  await again.close();
});

test('a store whose load returns something that is not bytes fails the open with secrets', { timeout: 10000 }, async () => {
  for (const loaded of [42, { not: 'bytes' }, true]) {
    await assert.rejects(Client.open(config(), memoryStore({ loaded })), code('secrets'));
  }
});

test('a store whose load throws or rejects fails the open with secrets', { timeout: 10000 }, async () => {
  const throwing = memoryStore();
  throwing.load = () => {
    throw new Error('the keychain is locked');
  };
  await assert.rejects(Client.open(config(), throwing), code('secrets'));
  const rejecting = memoryStore();
  rejecting.load = () => Promise.reject(new Error('the keychain is locked'));
  await assert.rejects(Client.open(config(), rejecting), code('secrets'));
});

test('close() twice, also at once, gives every caller the same outcome', async () => {
  const client = await Client.open(config(), memoryStore());
  const first = client.close();
  const second = client.close();
  assert.equal(first, second, 'one promise for every caller');
  await first;
  await client.close();
  await assert.rejects(client.signOut(), code('closed'));
});

test('onEvent after close() fails with closed', async () => {
  const client = await Client.open(config(), memoryStore());
  await client.close();
  assert.throws(() => client.onEvent(() => {}), code('closed'));
});