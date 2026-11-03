//! Call state in Redis (spec §5): the record with its compare-and-set version, the busy
//! locks, the due set and the device registry. Every key carries the hash tag `{calls}`,
//! so the scripts touch one cluster slot. Redis `TIME` is the only clock (spec D4).

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use push_domain::{DeviceId, UserId};
use push_proto::presence_conns_key;
use redis::aio::ConnectionManager;
use redis::{ErrorKind, FromRedisValue, RedisError, RedisResult, ServerErrorKind, Value};
use uuid::Uuid;

/// The sorted set of live calls by their next deadline.
pub const DUE_KEY: &str = "calls:{calls}:due";

/// How long a device registration lives after its last attach.
const DEVICES_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// How long one Redis operation of the call engine may take.
const REDIS_TIMEOUT: Duration = Duration::from_secs(2);

/// A call's record hash: `ver` and `rec`.
pub fn call_key(call: Uuid) -> String {
    format!("calls:{{calls}}:call:{call}")
}

/// A user's busy lock: the id of the user's call.
pub fn user_key(user: UserId) -> String {
    format!("calls:{{calls}}:user:{}", user.0)
}

/// A user's attached devices: device id → connection id.
pub fn devices_key(user: UserId) -> String {
    format!("calls:{{calls}}:devices:{}", user.0)
}

/// `{ver or false, rec or false, now_ms}` of a call.
const LOAD: &str = r"
local t = redis.call('TIME'); local now = t[1]*1000 + math.floor(t[2]/1000)
local v = redis.call('HMGET', KEYS[1], 'ver', 'rec')
return {v[1], v[2], now}
";

/// Creates, updates or ends a call atomically. KEYS: the call, the due set, then user
/// keys. ARGV: 1 the expected version ('' creates), 2 the new record ('' ends), 3 the TTL
/// in ms, 4 the due score, 5 the call id, 6 how many user keys to lock (the caller first),
/// 7 how many further user keys to release.
const APPLY: &str = r"
local ver = redis.call('HGET', KEYS[1], 'ver')
if ARGV[1] == '' then
  if ver then return 'EXISTS' end
else
  if ver ~= ARGV[1] then return 'CONFLICT' end
end
local nlock, nrel = tonumber(ARGV[6]), tonumber(ARGV[7])
if ARGV[1] == '' then
  for i = 1, nlock do
    local cur = redis.call('GET', KEYS[2 + i])
    if cur and cur ~= ARGV[5] then
      if i == 1 then return 'IN_CALL' else return 'BUSY' end
    end
  end
end
if ARGV[2] == '' then
  redis.call('DEL', KEYS[1]); redis.call('ZREM', KEYS[2], ARGV[5])
  for i = 1, nlock + nrel do
    if redis.call('GET', KEYS[2 + i]) == ARGV[5] then redis.call('DEL', KEYS[2 + i]) end
  end
  return 'OK'
end
local nv = (tonumber(ver) or 0) + 1
redis.call('HSET', KEYS[1], 'ver', nv, 'rec', ARGV[2]); redis.call('PEXPIRE', KEYS[1], ARGV[3])
for i = 1, nlock do
  local cur = redis.call('GET', KEYS[2 + i])
  -- An update refreshes the locks it still holds; it never takes another call's.
  if (not cur) or cur == ARGV[5] then redis.call('SET', KEYS[2 + i], ARGV[5], 'PX', ARGV[3]) end
end
redis.call('ZADD', KEYS[2], ARGV[4], ARGV[5])
return 'OK'
";

/// Claims up to `ARGV[1]` due calls, each pushed `ARGV[2]` ms ahead (a lease):
/// `{now_ms, id1, id2, …}`.
const CLAIM: &str = r"
local t = redis.call('TIME'); local now = t[1]*1000 + math.floor(t[2]/1000)
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', now, 'LIMIT', 0, tonumber(ARGV[1]))
for _, id in ipairs(ids) do redis.call('ZADD', KEYS[1], 'XX', now + tonumber(ARGV[2]), id) end
table.insert(ids, 1, now); return ids
";

/// Registers a device's connection (`ARGV[1]` device, `ARGV[2]` connection, `ARGV[3]` the
/// registry's TTL in ms).
const ATTACH: &str = r"
redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
redis.call('PEXPIRE', KEYS[1], ARGV[3])
return 1
";

/// Removes a device's registration if it still names the connection: 1 if it did.
const DETACH: &str = r"
if redis.call('HGET', KEYS[1], ARGV[1]) == ARGV[2] then
  redis.call('HDEL', KEYS[1], ARGV[1])
  return 1
end
return 0
";

/// A Lua script run with `EVALSHA`, loaded with `SCRIPT LOAD` when Redis lacks it.
struct Script {
    source: &'static str,
    /// The script's SHA1 once Redis loaded it; cleared when Redis lost it.
    sha: Mutex<Option<String>>,
}

impl Script {
    const fn new(source: &'static str) -> Self {
        Self {
            source,
            sha: Mutex::new(None),
        }
    }

    /// Runs the script; once more when the connection was dropped, and then `true`: the
    /// first attempt may have run as well.
    async fn run<T: FromRedisValue>(
        &self,
        redis: &ConnectionManager,
        keys: &[String],
        args: &[String],
    ) -> RedisResult<(T, bool)> {
        match self.run_loaded(redis, keys, args).await {
            Err(error) if push_credentials::is_dropped_connection(&error) => {
                tracing::debug!(%error, "the Redis connection was closed; running once more");
                Ok((self.run_loaded(redis, keys, args).await?, true))
            }
            result => result.map(|value| (value, false)),
        }
    }

    /// Runs the script, loading it once when Redis does not have it.
    async fn run_loaded<T: FromRedisValue>(
        &self,
        redis: &ConnectionManager,
        keys: &[String],
        args: &[String],
    ) -> RedisResult<T> {
        match self.evalsha(redis, keys, args).await {
            Err(error) if error.kind() == ErrorKind::Server(ServerErrorKind::NoScript) => {
                self.sha.lock().expect("call script lock").take();
                self.evalsha(redis, keys, args).await
            }
            result => result,
        }
    }

    async fn evalsha<T: FromRedisValue>(
        &self,
        redis: &ConnectionManager,
        keys: &[String],
        args: &[String],
    ) -> RedisResult<T> {
        let sha = self.sha(redis).await?;
        redis::cmd("EVALSHA")
            .arg(sha)
            .arg(keys.len())
            .arg(keys)
            .arg(args)
            .query_async(&mut redis.clone())
            .await
    }

    /// The SHA1 of the loaded script, loading it first if needed.
    async fn sha(&self, redis: &ConnectionManager) -> RedisResult<String> {
        if let Some(sha) = self.sha.lock().expect("call script lock").clone() {
            return Ok(sha);
        }
        let mut load = redis::cmd("SCRIPT");
        load.arg("LOAD").arg(self.source);
        let load = &load;
        // Loading twice is harmless.
        let sha: String = push_credentials::retry_if_dropped(|| async move {
            load.query_async(&mut redis.clone()).await
        })
        .await?;
        *self.sha.lock().expect("call script lock") = Some(sha.clone());
        Ok(sha)
    }
}

/// What [`RedisCalls::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Written.
    Ok,
    /// The record's version is not the expected one: read again.
    Conflict,
    /// A create whose caller is already in a call.
    InCall,
    /// A create whose callee is in a call.
    Busy,
    /// A create of a call id that exists.
    Exists,
}

/// One compare-and-set write of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write<'a> {
    pub call: Uuid,
    /// The version read; `None` creates the call (it must not exist).
    pub expected: Option<u64>,
    /// The new JSON record; `None` ends the call (deletes it and its due entry).
    pub record: Option<&'a str>,
    /// The record's and the locks' expiry from now.
    pub ttl_ms: i64,
    /// The call's next deadline (its due score).
    pub due_ms: i64,
    /// Users whose busy lock must be free or this call's on a create (the caller first),
    /// and is set or refreshed when it is.
    pub lock: Vec<UserId>,
    /// Users whose lock is deleted on an end when it names this call.
    pub release: Vec<UserId>,
}

/// The call engine's Redis operations.
pub struct RedisCalls {
    connection: ConnectionManager,
    load: Script,
    apply: Script,
    claim: Script,
    attach: Script,
    detach: Script,
}

impl RedisCalls {
    /// On `connection` (shared with the notifier).
    pub fn new(connection: ConnectionManager) -> Self {
        Self {
            connection,
            load: Script::new(LOAD),
            apply: Script::new(APPLY),
            claim: Script::new(CLAIM),
            attach: Script::new(ATTACH),
            detach: Script::new(DETACH),
        }
    }

    /// Connects to Redis at `url` (`redis://:password@host:6379`); reconnects by itself.
    pub async fn connect(url: &str) -> RedisResult<Self> {
        let connection = push_credentials::RedisConnector::password(url)?
            .connection_manager(redis::aio::ConnectionManagerConfig::new())
            .await?;
        Ok(Self::new(connection))
    }

    /// The call's `(version, JSON record)` if it exists, and Redis time in ms.
    pub async fn load(&self, call: Uuid) -> RedisResult<(Option<(u64, String)>, i64)> {
        let keys = [call_key(call)];
        let ((ver, rec, now), _): ((Option<String>, Option<String>, i64), bool) =
            bounded(self.load.run(&self.connection, &keys, &[])).await?;
        let stored = match (ver, rec) {
            (Some(ver), Some(rec)) => Some((parse_version(&ver)?, rec)),
            _ => None,
        };
        Ok((stored, now))
    }

    /// Redis time in ms.
    pub async fn now(&self) -> RedisResult<i64> {
        let (seconds, micros): (i64, i64) = self.query(&redis::cmd("TIME")).await?;
        Ok(seconds * 1000 + micros / 1000)
    }

    /// The call `user` is in, by the user's busy lock.
    pub async fn user_call(&self, user: UserId) -> RedisResult<Option<Uuid>> {
        let mut get = redis::cmd("GET");
        get.arg(user_key(user));
        let id: Option<String> = self.query(&get).await?;
        id.map(|id| parse_uuid(&id)).transpose()
    }

    /// Writes `write` atomically. When the answer is lost (a timeout, a dropped connection,
    /// a failed retry) or a write sent twice finds a conflict, the record is read again:
    /// a write that landed is `Ok`, so its frames are still published. A failed read
    /// answers the error.
    pub async fn apply(&self, write: &Write<'_>) -> RedisResult<Applied> {
        let mut keys = vec![call_key(write.call), DUE_KEY.to_owned()];
        keys.extend(write.lock.iter().map(|user| user_key(*user)));
        keys.extend(write.release.iter().map(|user| user_key(*user)));
        let args = [
            write
                .expected
                .map(|ver| ver.to_string())
                .unwrap_or_default(),
            write.record.unwrap_or_default().to_owned(),
            write.ttl_ms.max(1).to_string(),
            write.due_ms.to_string(),
            write.call.to_string(),
            write.lock.len().to_string(),
            write.release.len().to_string(),
        ];
        let (answer, retried): (String, bool) =
            match bounded(self.apply.run(&self.connection, &keys, &args)).await {
                Ok(answered) => answered,
                // The script may have run. The read goes on the same multiplexed
                // connection after it, so it sees the write if it was sent.
                Err(error) => {
                    return if self.landed(write).await? {
                        Ok(Applied::Ok)
                    } else {
                        Err(error)
                    };
                }
            };
        let applied = match answer.as_str() {
            "OK" => Applied::Ok,
            "CONFLICT" => Applied::Conflict,
            "IN_CALL" => Applied::InCall,
            "BUSY" => Applied::Busy,
            "EXISTS" => Applied::Exists,
            _ => {
                return Err(RedisError::from((
                    ErrorKind::Parse,
                    "an unexpected answer of the apply script",
                )));
            }
        };
        if retried
            && matches!(applied, Applied::Conflict | Applied::Exists)
            && self.landed(write).await?
        {
            return Ok(Applied::Ok);
        }
        Ok(applied)
    }

    /// Whether the call is as `write` left it: deleted for an end, else the written
    /// record at the version after the expected one.
    async fn landed(&self, write: &Write<'_>) -> RedisResult<bool> {
        let (stored, _) = self.load(write.call).await?;
        let version = write.expected.map_or(1, |expected| expected + 1);
        Ok(match (write.record, stored) {
            (None, None) => true,
            (Some(record), Some((stored_version, stored))) => {
                stored_version == version && stored == record
            }
            _ => false,
        })
    }

    /// Up to `max` due calls, each leased for `lease_ms`, and Redis time in ms.
    pub async fn claim(&self, max: u32, lease_ms: i64) -> RedisResult<(i64, Vec<Uuid>)> {
        let keys = [DUE_KEY.to_owned()];
        let args = [max.to_string(), lease_ms.to_string()];
        let (values, _): (Vec<Value>, bool) =
            bounded(self.claim.run(&self.connection, &keys, &args)).await?;
        let mut values = values.into_iter();
        let Some(now) = values.next() else {
            return Err(RedisError::from((
                ErrorKind::Parse,
                "the claim script answered nothing",
            )));
        };
        let now = i64::from_redis_value(now)?;
        let mut ids = Vec::new();
        for id in values {
            ids.push(parse_uuid(&String::from_redis_value(id)?)?);
        }
        Ok((now, ids))
    }

    /// Registers `conn` as `device`'s calls-active connection (replacing an earlier one).
    pub async fn attach(&self, user: UserId, device: DeviceId, conn: Uuid) -> RedisResult<()> {
        let keys = [devices_key(user)];
        let args = [
            device.0.to_string(),
            conn.to_string(),
            DEVICES_TTL_MS.to_string(),
        ];
        let _: (i64, bool) = bounded(self.attach.run(&self.connection, &keys, &args)).await?;
        Ok(())
    }

    /// Removes `device`'s registration if it still names `conn`; whether it did.
    pub async fn detach(&self, user: UserId, device: DeviceId, conn: Uuid) -> RedisResult<bool> {
        let keys = [devices_key(user)];
        let args = [device.0.to_string(), conn.to_string()];
        let (removed, _): (i64, bool) =
            bounded(self.detach.run(&self.connection, &keys, &args)).await?;
        Ok(removed == 1)
    }

    /// The registered connection of `device`.
    pub async fn connection_of(&self, user: UserId, device: DeviceId) -> RedisResult<Option<Uuid>> {
        let mut get = redis::cmd("HGET");
        get.arg(devices_key(user)).arg(device.0.to_string());
        let conn: Option<String> = self.query(&get).await?;
        conn.map(|conn| parse_uuid(&conn)).transpose()
    }

    /// Whether `conn` of `user` holds a presence lease that ends after `now_ms`.
    pub async fn connection_live(
        &self,
        user: UserId,
        conn: Uuid,
        now_ms: i64,
    ) -> RedisResult<bool> {
        let mut score = redis::cmd("ZSCORE");
        score.arg(presence_conns_key(user.0)).arg(conn.to_string());
        let lease: Option<f64> = self.query(&score).await?;
        Ok(lease.is_some_and(|end| end > now_ms as f64))
    }

    /// `user`'s attached devices whose connection holds a live presence lease, in id
    /// order.
    pub async fn live_devices(&self, user: UserId, now_ms: i64) -> RedisResult<Vec<DeviceId>> {
        let mut all = redis::cmd("HGETALL");
        all.arg(devices_key(user));
        let registered: Vec<(String, String)> = self.query(&all).await?;
        if registered.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipeline = redis::pipe();
        for (_, conn) in &registered {
            pipeline
                .cmd("ZSCORE")
                .arg(presence_conns_key(user.0))
                .arg(conn);
        }
        let pipeline = &pipeline;
        let leases: Vec<Option<f64>> = bounded(push_credentials::retry_if_dropped(|| async move {
            pipeline.query_async(&mut self.connection.clone()).await
        }))
        .await?;
        let mut live = Vec::new();
        for ((device, _), lease) in registered.iter().zip(leases) {
            if lease.is_some_and(|end| end > now_ms as f64) {
                live.push(DeviceId(parse_uuid(device)?));
            }
        }
        live.sort_by_key(|device| device.0);
        Ok(live)
    }

    /// How many calls are live (the due set's size).
    pub async fn live_count(&self) -> RedisResult<u64> {
        let mut card = redis::cmd("ZCARD");
        card.arg(DUE_KEY);
        self.query(&card).await
    }

    /// Removes a due entry whose record is gone.
    pub async fn zrem_due(&self, call: Uuid) -> RedisResult<()> {
        let mut zrem = redis::cmd("ZREM");
        zrem.arg(DUE_KEY).arg(call.to_string());
        let _: i64 = self.query(&zrem).await?;
        Ok(())
    }

    /// Runs a read or idempotent command, once more on a dropped connection.
    async fn query<T: FromRedisValue>(&self, command: &redis::Cmd) -> RedisResult<T> {
        bounded(push_credentials::retry_if_dropped(|| async move {
            command.query_async(&mut self.connection.clone()).await
        }))
        .await
    }
}

/// `operation`, failed as a timeout after [`REDIS_TIMEOUT`].
async fn bounded<T>(operation: impl Future<Output = RedisResult<T>>) -> RedisResult<T> {
    match tokio::time::timeout(REDIS_TIMEOUT, operation).await {
        Ok(result) => result,
        Err(_) => Err(RedisError::from(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "a call engine Redis operation timed out",
        ))),
    }
}

fn parse_uuid(text: &str) -> RedisResult<Uuid> {
    Uuid::parse_str(text)
        .map_err(|_| RedisError::from((ErrorKind::Parse, "not a UUID in the call state")))
}

fn parse_version(text: &str) -> RedisResult<u64> {
    text.parse()
        .map_err(|_| RedisError::from((ErrorKind::Parse, "not a call record version")))
}
