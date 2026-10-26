'use strict';

// Messages, local data and requests through Node, against push-sdk's fake auth-svc and a
// gateway that stores every send and answers it with the peer's copy.

const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { Client, PushError } = require('..');
const { startFakes, memoryStore, eventQueue } = require('./support');

let fakes;
before(async () => {
  fakes = await startFakes();
});
after(() => fakes.stop());

const PEER = '0199e1c2-0000-7000-8000-000000000001';

/** A client signed in and connected, closed after the test (a listener keeps Node running). */
async function connected(t) {
  const client = await Client.open(
    { authUrl: fakes.authUrl, socketUrl: fakes.socketUrl, databasePath: ':memory:' },
    memoryStore(),
  );
  t.after(() => client.close());
  const events = eventQueue(client);
  await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  client.connect();
  await client.waitConnected(5000);
  return { client, events };
}

const code = (expected) => (error) => {
  assert.ok(error instanceof PushError, `a PushError, not ${error}`);
  assert.equal(error.code, expected);
  return true;
};

test('sends a message: the local echo, messageSent, the answer, the stored history', async (t) => {
  const { client, events } = await connected(t);
  const conversation = await client.openDirect(PEER);
  assert.equal(conversation.kind, 'direct');
  assert.equal(conversation.peer, PEER);
  const echo = await client.send(conversation.id, 'hello from Node');
  assert.equal(echo.status, 'pending');
  assert.equal(echo.body, 'hello from Node');
  const sent = await events.next((event) => event.type === 'messageSent' && event.message.clientMsgId === echo.clientMsgId);
  assert.equal(sent.message.seq, 1);
  const answer = await events.next((event) => event.type === 'messageAdded' && event.message.senderId === PEER);
  assert.equal(answer.message.body, 'hello from Node');
  assert.equal(answer.message.status, 'received');
  const history = await client.messages(conversation.id, null, 10);
  assert.deepEqual(history.map((message) => message.seq), [1, 2]);
  assert.equal(history[0].status, 'sent');
  assert.equal((await client.conversation(conversation.id)).lastSeq, 2);
  assert.equal((await client.conversations()).length, 1);

  const edit = await client.edit(conversation.id, echo.clientMsgId, 'hello again');
  assert.match(edit, /^[0-9a-f-]{36}$/);
  await events.next((event) => event.type === 'messageEdited' && event.message.clientMsgId === echo.clientMsgId);
  await client.delete(conversation.id, echo.clientMsgId);
  const deleted = await events.next((event) => event.type === 'messageDeleted');
  assert.equal(deleted.message.body, null);
  await client.markRead(conversation.id, 2);
  await client.close();
});

test('a message never sent can be taken back', async (t) => {
  const { client } = await connected(t);
  const conversation = await client.openDirect(PEER);
  client.disconnect();
  const queued = await client.send(conversation.id, 'never mind');
  assert.equal(await client.cancel(conversation.id, queued.clientMsgId), true);
  assert.deepEqual(await client.messages(conversation.id, null, 10), []);
  await client.close();
});

test('requests: invites and contacts, and their errors', async (t) => {
  const { client } = await connected(t);
  const invite = await client.createInvite(3600, 1);
  assert.equal(invite.code, 'ABCDEFGHJK');
  assert.equal(invite.maxUses, 1);
  assert.match(await client.redeemInvite('ABCDEFGHJK'), /^[0-9a-f-]{36}$/);
  await assert.rejects(client.redeemInvite('NOSUCHCODE'), (error) => code('invite_not_found')(error) && !error.retryable);
  await assert.rejects(client.revokeInvite('ABCDEFGHJK'), code('rejected'));
  assert.deepEqual(await client.listContacts(), []);
  assert.deepEqual(await client.listConversations(), []);
  await assert.rejects(client.send('0199e1c2-0000-7000-8000-0000000000ff', 'x'), code('invalid'));
  await assert.rejects(client.send('not an id', 'x'), code('invalid'));
  await assert.rejects(client.openDirect(42), code('invalid'));
  client.disconnect();
  await assert.rejects(client.listContacts(), (error) => code('offline')(error) && error.retryable);
  await client.close();
});

test('cards and answers: sendRich, the peer\'s card, sendAnswer, and invalid bodies', async (t) => {
  const { client, events } = await connected(t);
  const conversation = await client.openDirect(PEER);
  const body = JSON.stringify({ tpl: 'agent.approval@1', params: { title: 'Deploy?' }, fallback: 'Deploy?' });
  await assert.rejects(client.sendRich(conversation.id, 'Deploy?'), code('invalid'));
  await assert.rejects(client.sendRich(conversation.id, JSON.stringify({ tpl: 'agent.approval@1' })), code('invalid'));
  const card = await client.sendRich(conversation.id, body);
  assert.equal(card.kind, 'rich');
  assert.equal(card.answerSeq, null);
  assert.equal(card.status, 'pending');
  const own = await events.next((event) => event.type === 'messageSent' && event.message.clientMsgId === card.clientMsgId);
  // The fake gateway's peer sends the same card back: one to answer.
  const theirs = await events.next((event) => event.type === 'messageAdded' && event.message.senderId === PEER);
  assert.equal(theirs.message.kind, 'rich');
  const answer = (cardId) => JSON.stringify({ card: cardId, values: ['approve'], fallback: 'Approved' });
  await assert.rejects(client.sendAnswer(conversation.id, answer(own.message.messageId)), code('invalid'));
  await assert.rejects(client.sendAnswer(conversation.id, '{"values":["approve"]}'), code('invalid'));
  const sent = await client.sendAnswer(conversation.id, answer(theirs.message.messageId));
  assert.equal(sent.kind, 'answer');
  assert.equal(sent.cardSeq, theirs.message.seq);
  const closed = await events.next((event) => event.type === 'messageEdited' && event.message.clientMsgId === theirs.message.clientMsgId);
  assert.ok(closed.message.answerSeq > theirs.message.seq);
  await assert.rejects(client.sendAnswer(conversation.id, answer(theirs.message.messageId)), code('invalid'));
  await assert.rejects(client.edit(conversation.id, sent.clientMsgId, answer(theirs.message.messageId)), code('invalid'));
  await client.close();
});

test('activity and presence: sendActivity, getPresence, presence sharing', async (t) => {
  const { client } = await connected(t);
  const conversation = await client.openDirect(PEER);
  await client.sendActivity(conversation.id, 'working', 'Running tests');
  await client.sendActivity(conversation.id, 'typing');
  await client.sendActivity(conversation.id, 'idle', null);
  await assert.rejects(client.sendActivity(conversation.id, 'dancing'), code('invalid'));
  await assert.rejects(client.sendActivity(conversation.id, 'typing', 'a\nb'), code('invalid'));
  await assert.rejects(client.sendActivity('not an id', 'typing'), code('invalid'));
  const [offline] = await client.getPresence([PEER, PEER]);
  assert.equal(offline.userId, PEER);
  assert.equal(offline.state, 'offline');
  assert.equal(offline.lastSeenMs % 60000, 0);
  assert.ok(Math.abs(offline.lastSeenMs - Date.now()) < 120000);
  await assert.rejects(client.getPresence([]), code('invalid'));
  await assert.rejects(client.getPresence(['not an id']), code('invalid'));
  assert.equal(await client.presenceSharing(), true);
  assert.equal(await client.setPresenceSharing(false), false);
  assert.deepEqual(await client.getPresence([PEER]), [{ userId: PEER, state: 'unknown', lastSeenMs: null }]);
  assert.equal(await client.setPresenceSharing(true), true);
  client.disconnect();
  for (let i = 0; i < 250 && client.connectionState().state !== 'offline'; i++) {
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  await assert.rejects(client.sendActivity(conversation.id, 'typing'), code('offline'));
  await client.close();
});

test('the typings declare every call of the client, and only those', () => {
  const declared = fs.readFileSync(path.join(__dirname, '..', 'index.d.ts'), 'utf8');
  const body = declared.slice(declared.indexOf('export class Client {'));
  const typed = new Set([...body.slice(0, body.indexOf('\n}')).matchAll(/^ {2}(?:static )?(\w+)\(/gm)].map((match) => match[1]));
  typed.delete('constructor');
  const implemented = new Set(
    Object.getOwnPropertyNames(Client.prototype).filter((name) => name !== 'constructor').concat(['open']),
  );
  assert.deepEqual([...typed].sort(), [...implemented].sort());
});
