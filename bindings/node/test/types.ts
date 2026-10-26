// Compiled by `npm run typecheck` (never run): the typings describe a real app's use.

import {
  Account,
  ActivityState,
  Client,
  Presence,
  PresenceState,
  Profile,
  PushError,
  PushEvent,
  SecretStore,
  deriveDatabaseKey,
  newSalt,
  SECRET_KEYS,
} from '../index';

const values = new Map<string, Uint8Array>();
const store: SecretStore = {
  load: async (key) => values.get(key) ?? null,
  save: (key, value) => {
    values.set(key, value);
  },
  delete: async (key) => {
    values.delete(key);
  },
};

async function app(): Promise<void> {
  const databaseKey = await deriveDatabaseKey('2468', newSalt());
  const client = await Client.open(
    { authUrl: 'https://push.example', socketUrl: 'wss://push.example/v1/socket', databasePath: 'app.db', databaseKey },
    store,
  );
  client.onEvent((event: PushEvent) => {
    switch (event.type) {
      case 'messageAdded':
        console.log(event.message.clientMsgId, event.message.status);
        break;
      case 'connectionStateChanged':
        if (event.state === 'reconnecting') console.log(event.retryInMs);
        break;
      case 'sendFailed':
        console.log(event.error.code);
        break;
      case 'activityChanged':
        if (event.activity.state !== 'idle') console.log(event.activity.label ?? event.activity.state, event.activity.expiresAtMs);
        break;
      case 'presenceChanged':
        if (event.presence.state === 'offline') console.log(event.presence.lastSeenMs);
        break;
    }
  });
  const account = await client.signIn('a@example.com', 'password', 'laptop', 'desktop');
  const viaEntra: Account = await client.signInWithEntra('token', null, 'laptop', 'desktop');
  console.log(viaEntra.deviceId);
  const renamed: Profile = await client.updateProfile(null, 'erin');
  console.log(renamed.handle);
  console.log(account.userId, client.account()?.deviceId, SECRET_KEYS.length);
  client.connect();
  const conversation = await client.openDirect('0199e1c2-0000-7000-8000-000000000001');
  const echo = await client.send(conversation.id, 'hi');
  const card = await client.sendRich(
    conversation.id,
    JSON.stringify({ tpl: 'agent.approval@1', params: {}, fallback: 'Deploy?' }),
  );
  if (card.kind === 'rich' && card.answerSeq === null && card.messageId) {
    const answer = await client.sendAnswer(
      conversation.id,
      JSON.stringify({ card: card.messageId, values: ['approve'], fallback: 'Approved' }),
    );
    const cardSeq: number | null = answer.cardSeq;
    console.log(cardSeq);
  }
  const history = await client.messages(conversation.id, null, 50);
  console.log(echo.seq, history.length);
  const doing: ActivityState = 'working';
  await client.sendActivity(conversation.id, doing, 'Running tests');
  await client.sendActivity(conversation.id, 'idle');
  const presences: Presence[] = await client.getPresence([conversation.peer ?? account.userId]);
  const state: PresenceState = presences[0].state;
  const sharing: boolean = await client.setPresenceSharing(!(await client.presenceSharing()));
  console.log(state, sharing);
  try {
    await client.redeemInvite('NOSUCHCODE');
  } catch (error) {
    if (error instanceof PushError && error.code === 'invite_not_found') console.log(error.retryable);
  }
  await client.close();
  const made = new PushError({ code: 'timeout', message: 'no answer' });
  console.log(made.code, made.retryable, made.retryAfterS, await deriveDatabaseKey('2468', 'a salt'));
}

void app;
