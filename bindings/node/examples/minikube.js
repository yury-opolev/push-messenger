'use strict';

// Two Node.js apps on push-sdk against a running deployment (the minikube cluster, through
// its ingress): they sign up, verify their email from the DEV ONLY mailbox as a browser
// would open the link, sign in, become contacts by invite and chat with a read receipt.
// deploy/minikube/node-example.ps1 forwards the ports and runs it:
//
//   node examples/minikube.js --auth http://127.0.0.1:18080 --socket ws://127.0.0.1:18080/v1/socket \
//     --host push.local --mailbox http://127.0.0.1:18081
//
// It prints ids, seqs, codes and timings, never a token, a password or a message's text.

const http = require('node:http');
const { randomUUID } = require('node:crypto');

const { Client, PushError } = require('..');

const args = Object.fromEntries(
  process.argv.slice(2).reduce((pairs, value, index, all) => (index % 2 === 0 ? [...pairs, [value.replace(/^--/, ''), all[index + 1]]] : pairs), []),
);
for (const name of ['auth', 'socket', 'mailbox']) {
  if (!args[name]) {
    console.error('usage: node examples/minikube.js --auth <url> --socket <ws-url> --mailbox <url> [--host <host>]');
    process.exit(2);
  }
}

const started = Date.now();
const step = (text) => console.log(`node example: ${text}`);

/** A plain HTTP request with the ingress's virtual host; resolves to [status, JSON body]. */
function request(method, url, body, host) {
  const target = new URL(url);
  const payload = body == null ? null : JSON.stringify(body);
  return new Promise((resolve, reject) => {
    const outgoing = http.request(
      target,
      {
        method,
        headers: {
          ...(host ? { host } : {}),
          ...(payload ? { 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) } : {}),
        },
      },
      (response) => {
        let text = '';
        response.setEncoding('utf8');
        response.on('data', (chunk) => (text += chunk));
        response.on('end', () => resolve([response.statusCode, text ? JSON.parse(text) : null]));
      },
    );
    outgoing.on('error', reject);
    if (payload) outgoing.write(payload);
    outgoing.end();
  });
}

/** Opens the verification link the worker delivered to the DEV ONLY mailbox. */
async function verify(email) {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    const [, mailbox] = await request('GET', `${args.mailbox}/dev/mailbox?to=${encodeURIComponent(email)}`);
    const found = (mailbox.emails || []).filter((sent) => sent.template === 'verify_email').pop();
    if (found) {
      const link = found.text.split('\n').find((line) => line.startsWith('http'));
      const token = new URL(link).searchParams.get('token');
      const [status] = await request('POST', `${args.auth}/v1/auth/verify`, { token }, args.host);
      if (status !== 200) throw new Error(`verify answered ${status}`);
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`no verification email for ${email} within 30 s`);
}

/** A store in memory: an app would use the platform's keychain (docs/sdk-bindings.md). */
function memoryStore() {
  const values = new Map();
  return {
    load: async (key) => values.get(key) ?? null,
    save: async (key, value) => {
      values.set(key, Buffer.from(value));
    },
    delete: async (key) => {
      values.delete(key);
    },
  };
}

/** An app with its own client: signed up, verified, signed in and connected. */
async function app(name) {
  const suffix = randomUUID().replace(/-/g, '').slice(-10);
  const email = `${name}.${suffix}@node.example`;
  const password = `node-${randomUUID()}`;
  const client = await Client.open(
    { authUrl: args.auth, socketUrl: args.socket, host: args.host, databasePath: ':memory:' },
    memoryStore(),
  );
  const events = [];
  const waiting = [];
  client.onEvent((event) => {
    events.push(event);
    for (const waiter of [...waiting]) {
      if (waiter.match(event)) {
        waiting.splice(waiting.indexOf(waiter), 1);
        waiter.resolve(event);
      }
    }
  });
  const next = (match) =>
    events.find(match) ||
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${name}: no such event within 15 s`)), 15000);
      waiting.push({ match, resolve: (event) => (clearTimeout(timer), resolve(event)) });
    });
  const begun = Date.now();
  await client.signUp(email, password, `${name}_${suffix}`, name);
  await verify(email);
  const account = await client.signIn(email, password, `${name}'s laptop`, 'desktop');
  client.connect();
  await client.waitConnected(30000);
  const reconnects = events.filter((event) => event.type === 'connectionStateChanged' && event.state === 'reconnecting').length;
  step(
    `${name} signed up, verified, signed in and connected (device ${account.deviceId}, ${reconnects} reconnects while the device was not relayed yet) in ${Date.now() - begun} ms`,
  );
  return { name, client, account, next };
}

async function main() {
  const ann = await app('ann');
  const ben = await app('ben');

  const invite = await ann.client.createInvite(3600, 1);
  const inviter = await ben.client.redeemInvite(invite.code);
  if (inviter !== ann.account.userId) throw new Error('the invite names someone else');
  await ann.next((event) => event.type === 'contactChanged' && event.contact.userId === ben.account.userId && event.contact.state === 'accepted');
  step("ben redeemed ann's invite; ann's app heard the new contact");

  const conversation = await ben.client.openDirect(ann.account.userId);
  const sentAt = Date.now();
  const echo = await ben.client.send(conversation.id, 'hello from Node on minikube');
  const sent = await ben.next((event) => event.type === 'messageSent' && event.message.clientMsgId === echo.clientMsgId);
  const received = await ann.next((event) => event.type === 'messageAdded' && event.message.clientMsgId === echo.clientMsgId);
  step(`ben's message was stored (seq ${sent.message.seq}) and ann's app received it (${received.message.body.length} characters) after ${Date.now() - sentAt} ms`);

  await ann.client.markRead(conversation.id, received.message.seq);
  const receipt = await ben.next((event) => event.type === 'receiptChanged' && event.peerReadSeq >= received.message.seq);
  step(`ann read it; ben's app heard the receipt (read up to seq ${receipt.peerReadSeq})`);

  try {
    await ann.client.redeemInvite('AAAAAAAAAA');
    throw new Error('an unknown invite was redeemed');
  } catch (error) {
    if (!(error instanceof PushError) || error.code !== 'invite_not_found') throw error;
    step(`an unknown invite failed with ${error.code} (retryable: ${error.retryable})`);
  }

  await Promise.all([ann.client.close(), ben.client.close()]);
  step(`PASS in ${Date.now() - started} ms`);
}

main().catch((error) => {
  console.error(`node example: FAIL: ${error instanceof PushError ? `${error.code}: ${error.message}` : error.message}`);
  process.exit(1);
});
