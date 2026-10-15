/*
 * A C app on push-sdk's C API, run by tests/c_program.rs against the SDK's fake auth-svc and
 * gateway: usage `flow <authUrl> <socketUrl> <scratch directory>`. Prints one line per
 * step and "c flow: PASS" at the end; exits 1 at the first failure.
 */
#include "push_sdk.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
typedef CRITICAL_SECTION lock_t;
static void lock_init(lock_t *lock) { InitializeCriticalSection(lock); }
static void lock_take(lock_t *lock) { EnterCriticalSection(lock); }
static void lock_give(lock_t *lock) { LeaveCriticalSection(lock); }
static void pause_ms(unsigned ms) { Sleep(ms); }
#else
#include <pthread.h>
#include <time.h>
typedef pthread_mutex_t lock_t;
static void lock_init(lock_t *lock) { pthread_mutex_init(lock, NULL); }
static void lock_take(lock_t *lock) { pthread_mutex_lock(lock); }
static void lock_give(lock_t *lock) { pthread_mutex_unlock(lock); }
static void pause_ms(unsigned ms) {
    struct timespec wait = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000L};
    nanosleep(&wait, NULL);
}
#endif

static const char *auth_url;
static const char *socket_url;
static const char *scratch;

static void fail(const char *step, PushError *error) {
    fprintf(stderr, "c flow: FAIL at %s: %s (%s)\n", step,
            error ? push_error_message(error) : "unexpected result",
            error ? push_error_code(error) : "-");
    exit(1);
}

static void step(const char *done) { printf("c flow: %s\n", done); }

/* The value of a string field in a JSON object (enough for the SDK's flat ids). */
static int field(const char *json, const char *name, char *out, size_t size) {
    char pattern[64];
    const char *start;
    const char *end;
    snprintf(pattern, sizeof pattern, "\"%s\":\"", name);
    start = strstr(json, pattern);
    if (!start) return 0;
    start += strlen(pattern);
    end = strchr(start, '"');
    if (!end || (size_t)(end - start) >= size) return 0;
    memcpy(out, start, (size_t)(end - start));
    out[end - start] = '\0';
    return 1;
}

/*
 * Expects a failure with `code`. Takes the error's address, read once the call returned: C
 * leaves the order in which a call's arguments are evaluated open.
 */
static void expect_code(const char *what, int status, PushError **error, const char *code) {
    if (status != PUSH_ERROR || !*error || strcmp(push_error_code(*error), code) != 0) {
        fprintf(stderr, "c flow: FAIL at %s: wanted %s, got %s\n", what, code,
                *error ? push_error_code(*error) : "success");
        exit(1);
    }
    printf("c flow: %s failed with %s (retryable: %s)\n", what, code,
           push_error_retryable(*error) ? "yes" : "no");
    push_error_free(*error);
    *error = NULL;
}

/* ----- an in-memory secret store, as a keychain would be ----- */

struct entry {
    char key[64];
    uint8_t *value;
    size_t len;
};

struct store {
    lock_t lock;
    struct entry entries[8];
    int saves;
    int released;
};

static struct entry *find(struct store *store, const char *key) {
    int i;
    for (i = 0; i < 8; i++) {
        if (store->entries[i].value && strcmp(store->entries[i].key, key) == 0) return &store->entries[i];
    }
    return NULL;
}

static int32_t store_load(void *context, const char *key, uint8_t **value, size_t *value_len) {
    struct store *store = context;
    struct entry *found;
    int32_t status = PUSH_NOT_FOUND;
    lock_take(&store->lock);
    found = find(store, key);
    if (found) {
        *value = malloc(found->len ? found->len : 1);
        memcpy(*value, found->value, found->len);
        *value_len = found->len;
        status = PUSH_OK;
    }
    lock_give(&store->lock);
    return status;
}

static void store_free_value(void *context, uint8_t *value, size_t value_len) {
    (void)context;
    (void)value_len;
    free(value);
}

static int32_t store_remove(void *context, const char *key) {
    struct store *store = context;
    struct entry *found;
    lock_take(&store->lock);
    found = find(store, key);
    if (found) {
        free(found->value);
        found->value = NULL;
    }
    lock_give(&store->lock);
    return PUSH_OK;
}

static int32_t store_save(void *context, const char *key, const uint8_t *value, size_t value_len) {
    struct store *store = context;
    int i;
    store_remove(context, key);
    lock_take(&store->lock);
    for (i = 0; i < 8; i++) {
        if (!store->entries[i].value) {
            snprintf(store->entries[i].key, sizeof store->entries[i].key, "%s", key);
            store->entries[i].value = malloc(value_len ? value_len : 1);
            memcpy(store->entries[i].value, value, value_len);
            store->entries[i].len = value_len;
            store->saves++;
            break;
        }
    }
    lock_give(&store->lock);
    return i < 8 ? PUSH_OK : PUSH_ERROR;
}

static void store_release(void *context) {
    struct store *store = context;
    lock_take(&store->lock);
    store->released++;
    lock_give(&store->lock);
}

static PushSecretStore secret_store(struct store *store) {
    PushSecretStore functions;
    functions.context = store;
    functions.load = store_load;
    functions.save = store_save;
    functions.remove = store_remove;
    functions.free_value = store_free_value;
    functions.release = store_release;
    return functions;
}

static PushClient *open_client(struct store *store, const char *database, const uint8_t *key) {
    char config[1024];
    PushClient *client = NULL;
    PushError *error = NULL;
    snprintf(config, sizeof config,
             "{\"authUrl\":\"%s\",\"socketUrl\":\"%s\",\"databasePath\":\"%s\"}", auth_url,
             socket_url, database);
    if (push_client_open(config, key, key ? PUSH_DATABASE_KEY_LEN : 0, secret_store(store), &client,
                         &error) != PUSH_OK)
        fail("open", error);
    return client;
}

/* ----- events through a callback ----- */

struct seen {
    lock_t lock;
    int connected;
    int events;
    int released;
    /* For a callback that destroys its own client. */
    PushClient *client;
    int destroyed;
    int released_inside;
};

static void on_event(void *context, const char *event_json) {
    struct seen *seen = context;
    lock_take(&seen->lock);
    seen->events++;
    if (strstr(event_json, "\"type\":\"connectionStateChanged\"") &&
        strstr(event_json, "\"state\":\"connected\""))
        seen->connected++;
    lock_give(&seen->lock);
    /* Slow, so that events are still queued when destroy is called. */
    pause_ms(100);
}

/* Destroys the client on the first "connected": destroy must not wait for this callback. */
static void on_event_destroying(void *context, const char *event_json) {
    struct seen *seen = context;
    int destroy;
    lock_take(&seen->lock);
    destroy = strstr(event_json, "\"state\":\"connected\"") && !seen->destroyed;
    if (destroy) seen->destroyed = 1;
    lock_give(&seen->lock);
    if (!destroy) return;
    push_client_destroy(seen->client);
    lock_take(&seen->lock);
    seen->released_inside = seen->released;
    lock_give(&seen->lock);
}

static void on_release(void *context) {
    struct seen *seen = context;
    lock_take(&seen->lock);
    seen->released++;
    lock_give(&seen->lock);
}

int main(int argc, char **argv) {
    static struct store store;
    static struct store other_store;
    static struct store key_store;
    static struct store inside_store;
    static struct seen seen;
    PushClient *client;
    PushError *error = NULL;
    char *text = NULL;
    char database[512];
    char conversation[64];
    char client_msg_id[64];
    char user_id[64];
    int sent = 0, answered = 0, waited;
    uint8_t salt[PUSH_SALT_LEN];
    uint8_t key[PUSH_DATABASE_KEY_LEN];
    uint8_t wrong_key[PUSH_DATABASE_KEY_LEN];

    if (argc != 4) {
        fprintf(stderr, "usage: flow <authUrl> <socketUrl> <scratch directory>\n");
        return 2;
    }
    auth_url = argv[1];
    socket_url = argv[2];
    scratch = argv[3];
    lock_init(&store.lock);
    lock_init(&other_store.lock);
    lock_init(&key_store.lock);
    lock_init(&inside_store.lock);
    lock_init(&seen.lock);
    printf("c flow: push-sdk %s\n", push_version());

    /* Refusals carry the SDK's codes. */
    expect_code("an open with a config that is not JSON",
                push_client_open("{", NULL, 0, secret_store(&store), &client, &error), &error,
                "invalid");
    if (store.released != 1) fail("the store is released when the open fails", NULL);
    store.released = 0;

    client = open_client(&store, ":memory:", NULL);
    error = NULL;
    text = (char *)"not the SDK's";
    expect_code("a sign-in with a wrong password",
                push_client_sign_in(client, "c@example.com", "wrong password", "c app", "desktop",
                                    &text, &error),
                &error, "invalid_credentials");
    if (text != NULL) fail("a failed call leaves its result place NULL", NULL);
    error = NULL;
    if (push_client_sign_in(client, "c@example.com", "password", "c app", "desktop", &text,
                            &error) != PUSH_OK)
        fail("sign in", error);
    if (!field(text, "userId", user_id, sizeof user_id)) fail("the account names the user", NULL);
    push_string_free(text);
    if (store.saves < 1) fail("the session is saved in the app's store", NULL);
    step("signed in; the session is in the app's secret store");

    if (push_client_connect(client, &error) != PUSH_OK) fail("connect", error);
    if (push_client_wait_connected(client, 10000, &error) != PUSH_OK) fail("wait connected", error);
    if (push_client_open_direct(client, "0199e1c2-0000-7000-8000-000000000001", &text, &error) !=
        PUSH_OK)
        fail("open a direct conversation", error);
    if (!field(text, "id", conversation, sizeof conversation)) fail("the conversation's id", NULL);
    push_string_free(text);
    step("connected and opened a direct conversation");

    if (push_client_send(client, conversation, "hello from C", &text, &error) != PUSH_OK)
        fail("send", error);
    if (!field(text, "clientMsgId", client_msg_id, sizeof client_msg_id) ||
        !strstr(text, "\"status\":\"pending\""))
        fail("the local echo", NULL);
    push_string_free(text);
    for (waited = 0; waited < 100 && !(sent && answered); waited++) {
        char *event = NULL;
        if (push_client_next_event(client, 100, &event, &error) != PUSH_OK) fail("next event", error);
        if (!event) continue;
        if (strstr(event, "\"type\":\"messageSent\"") && strstr(event, client_msg_id)) sent = 1;
        if (strstr(event, "\"type\":\"messageAdded\"") && strstr(event, "hello from C") &&
            !strstr(event, user_id))
            answered = 1;
        push_string_free(event);
    }
    if (!sent || !answered) fail("messageSent and the peer's answer as events", NULL);
    if (push_client_messages(client, conversation, NULL, 10, &text, &error) != PUSH_OK)
        fail("messages", error);
    if (!strstr(text, "\"seq\":1") || !strstr(text, "\"seq\":2")) fail("both messages stored", NULL);
    push_string_free(text);
    step("sent a message: messageSent (seq 1), the peer's answer (seq 2), both stored");

    expect_code("a send to an unknown conversation",
                push_client_send(client, "0199e1c2-0000-7000-8000-0000000000ff", "x", &text, &error),
                &error, "invalid");
    error = NULL;
    expect_code("redeeming an unknown invite",
                push_client_redeem_invite(client, "NOSUCHCODE", &text, &error), &error,
                "invite_not_found");
    error = NULL;
    expect_code("a callback after polling",
                push_client_set_event_callback(client, on_event, &seen, on_release, &error), &error,
                "invalid");
    error = NULL;
    push_client_destroy(client);
    if (store.released != 1) fail("the store is released by destroy", NULL);
    step("destroyed the client; the store was released");

    /* Events through a callback, on an SDK thread. */
    client = open_client(&other_store, ":memory:", NULL);
    if (push_client_set_event_callback(client, on_event, &seen, on_release, &error) != PUSH_OK)
        fail("set the event callback", error);
    if (push_client_sign_in(client, "c@example.com", "password", "c app", "desktop", &text,
                            &error) != PUSH_OK)
        fail("sign in again", error);
    push_string_free(text);
    if (push_client_connect(client, &error) != PUSH_OK) fail("connect again", error);
    for (waited = 0; waited < 100; waited++) {
        int connected;
        lock_take(&seen.lock);
        connected = seen.connected;
        lock_give(&seen.lock);
        if (connected) break;
        pause_ms(100);
    }
    if (!seen.connected) fail("the callback heard connected", NULL);
    push_client_destroy(client);
    /* destroy has stopped the thread: released already, and nothing more is delivered. */
    if (seen.released != 1) fail("the callback's context is released when destroy returns", NULL);
    {
        int events = seen.events;
        pause_ms(300);
        if (seen.events != events || seen.released != 1)
            fail("no callback runs after destroy returned", NULL);
    }
    step("the callback heard connected on an SDK thread; destroy returned after release");

    /* A callback that destroys its own client: no deadlock; release comes after it returns. */
    client = open_client(&inside_store, ":memory:", NULL);
    seen.released = 0;
    seen.client = client;
    if (push_client_set_event_callback(client, on_event_destroying, &seen, on_release, &error) !=
        PUSH_OK)
        fail("set the destroying callback", error);
    if (push_client_sign_in(client, "c@example.com", "password", "c app", "desktop", &text,
                            &error) != PUSH_OK)
        fail("sign in for the destroying callback", error);
    push_string_free(text);
    if (push_client_connect(client, &error) != PUSH_OK) fail("connect for the destroying callback", error);
    for (waited = 0; waited < 100 && !seen.released; waited++) pause_ms(50);
    pause_ms(200);
    if (seen.released != 1) fail("release is called once after the destroying callback", NULL);
    if (seen.released_inside != 0) fail("release waits for the destroying callback to return", NULL);
    step("a callback destroyed its client; release ran once, after it returned");

    /* An encrypted database opens only with its key. */
    if (push_new_salt(salt) != PUSH_OK) fail("new salt", NULL);
    if (push_derive_database_key((const uint8_t *)"2468", 4, salt, sizeof salt, key, &error) !=
        PUSH_OK)
        fail("derive a key", error);
    if (push_derive_database_key((const uint8_t *)"1357", 4, salt, sizeof salt, wrong_key,
                                 &error) != PUSH_OK)
        fail("derive another key", error);
    snprintf(database, sizeof database, "%s/encrypted.db", scratch);
    client = open_client(&key_store, database, key);
    push_client_destroy(client);
    {
        char config[1024];
        snprintf(config, sizeof config,
                 "{\"authUrl\":\"%s\",\"socketUrl\":\"%s\",\"databasePath\":\"%s\"}", auth_url,
                 socket_url, database);
        expect_code("an open with the wrong key",
                    push_client_open(config, wrong_key, sizeof wrong_key, secret_store(&key_store),
                                     &client, &error),
                    &error, "wrong_key");
    }
    client = open_client(&key_store, database, key);
    push_client_destroy(client);
    step("an encrypted database opened with its key, and only with it");

    printf("c flow: PASS\n");
    return 0;
}
