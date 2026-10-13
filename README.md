# pg-queue

PostgreSQL job queue, pub/sub, cache, and request-response for when you
already have Postgres and don't want to add Redis just for background jobs.

**This is a thin wrapper around sqlx.** It prioritizes 5-minute setup over
feature breadth. If you need priority levels, job heartbeats, idempotency
keys, or partitioning, use [sqlxmq](https://crates.io/crates/sqlxmq)
instead — it's the production-grade option in this space.

## What it is

A single-crate sqlx wrapper that wires up four Postgres features:

| Feature | Mechanism | When it makes sense |
|---------|-----------|--------------------|
| **Job queue** | `SELECT ... FOR UPDATE SKIP LOCKED` | Fire-and-forget work: emails, webhooks, cleanup |
| **Pub/Sub** | `LISTEN` / `NOTIFY` | Lightweight eventing between services that share a database |
| **Cache** | `UNLOGGED` table with per-row TTL | Caching expensive queries, not replacing a dedicated cache |
| **Request-Response** | Queue + `NOTIFY` with correlation ID | Simple RPC when both sides already talk to the same Postgres |

On top of the queue:

| Feature | Mechanism |
|---------|-----------|
| **Retries with backoff** | Constant, linear, or exponential `Backoff` applied by `fail` |
| **Dead lettering** | A job that burns `max_attempts` claims moves to `dead` and is never claimed again |
| **Scheduled and delayed jobs** | `push_at` sets `deliver_at`; `pop` only claims jobs whose time has come |
| **Crash recovery** | `reap_stale` returns abandoned claims to `pending`; `reap_exhausted` buries the ones that keep killing workers |
| **Consumer loop** | `run_worker` combines reaping, draining, and `LISTEN/NOTIFY` |

`SKIP LOCKED` lets multiple workers grab jobs concurrently without blocking
each other. `LISTEN/NOTIFY` pushes events to subscribers when a row becomes
claimable — notify-driven, with a poll fallback for scheduled jobs and
recovered claims. `UNLOGGED` tables skip write-ahead logging for cache-tier
speed (data is not crash-safe, which is fine for a cache).

The one thing this gives you that Redis can't: **atomic enqueue within a
transaction.** Insert your domain row and enqueue its side-effect job in
the same commit. Both land or neither does — no outbox pattern, no
two-phase commit.

```rust
let mut tx = pool.begin().await?;
sqlx::query("INSERT INTO orders (id, total) VALUES ($1, $2)")
    .bind(order.id)
    .bind(order.total)
    .execute(&mut *tx)
    .await?;

mgr.queue.push(&mut *tx, &emails, &order).await?;

tx.commit().await?; // order row and job row land together
```

`push`, `push_at`, `complete`, `fail`, and `notify` all take an executor, so
`&PgPool`, `&mut PgConnection`, and `&mut Transaction<'_, Postgres>` all work.
Claims (`pop`, `reap_stale`, `reap_exhausted`) are always short autocommit
statements and deliberately take no executor.

## Setup

Run the migration against your database:

```bash
psql -f migrations/setup.sql your_database
```

The migration is idempotent and upgrades existing queue tables in place:
re-running it adds any missing columns and recreates the triggers.

Then create as many queues as you need:

```sql
SELECT pg_queue_create_queue('emails');
SELECT pg_queue_create_queue('image_processing');
```

Each call creates a dedicated table (`queue_emails`, `queue_image_processing`)
with a partial index on ready jobs, a partial index on claimed jobs, and
triggers that fire `NOTIFY` both on insert and whenever a job returns to
`pending`.

Queue names must be 1-43 characters of ASCII alphanumerics or underscores.
43 is the longest name whose derived trigger, index, and function names still
fit PostgreSQL's 63-byte identifier limit; both `QueueName::new` and
`pg_queue_create_queue` reject longer names. The upgrade loop in `setup.sql`
only rewrites tables that carry the queue columns, so an unrelated table that
happens to be named `queue_something` is left alone. `migrations/teardown.sql`
leaves `pg_queue_drop_queue()` in place so you can drop queues one at a time.

A queue name is also an access boundary: any code holding a pool can read and
write any queue table in the schema. Do not build a `QueueName` directly from
untrusted input without checking it against an allow-list.

Add to your `Cargo.toml`:

```toml
[dependencies]
pg-queue = "0.1"
```

## Usage

### Job queue

```rust
use pg_queue::{PgQueueManager, QueueName};

let pool = sqlx::PgPool::connect("postgres://localhost/mydb").await?;
let mgr = PgQueueManager::new(pool);
let emails = QueueName::new("emails")?;

// Producer: enqueue a job
mgr.queue.push(mgr.pool(), &emails, &serde_json::json!({
    "to": "user@example.com",
    "template": "welcome"
})).await?;

// Consumer: claim and process
if let Some(job) = mgr.queue.pop::<serde_json::Value>(&emails).await? {
    // process job.payload ...
    mgr.queue.complete(mgr.pool(), &emails, job.id).await?;
}
```

Multiple consumers can call `pop` concurrently — `SKIP LOCKED` ensures each job is claimed by exactly one worker.

`complete` only completes a job the caller still holds in `processing`. If the
lease expired and another worker reclaimed it, you get
`CompleteOutcome::StaleClaim` and nothing is written.

### Worker loop

`run_worker` runs the whole consumer loop: subscribe to the queue's channel,
reap abandoned claims, drain ready jobs, then wait on `LISTEN/NOTIFY` for up to
the poll interval. The timeout is what picks up jobs inserted before the worker
started, jobs whose `deliver_at` has come due, and jobs returned to `pending`
by a reap.

```rust
use pg_queue::{run_worker, PgQueueManager, QueueName};
use std::time::Duration;

let mut listener = mgr.create_listener().await?;

run_worker::<MyJob, _, _>(
    &mgr.queue,
    &emails,
    &mut listener,
    Duration::from_secs(60),   // lease: must exceed the longest handler run
    Duration::from_secs(1),    // poll interval
    100,                       // max jobs per cycle
    |job| async move {
        send_email(job.payload).await
    },
).await?;
```

`run_worker` subscribes the listener itself, so the caller does not have to.
All three durations and counts must be greater than zero; it rejects zero
configuration instead of busy-looping or stealing live claims. A listener
failure is reported once and the worker keeps running on the poll path.

A handler that returns `Ok(())` completes the job. A handler that returns
`Err` calls `fail`, which reschedules with backoff or buries the job as `dead`.
A payload that cannot be decoded is released through the same retry policy and
logged, so it neither stops the worker nor strands the row in `processing`.

### Retries and scheduling

```rust
use pg_queue::{Backoff, FailOutcome, QueueRepository, RetryPolicy};
use std::time::Duration;

// Deliver an hour from now instead of immediately
mgr.queue.push_at(
    mgr.pool(),
    &emails,
    &payload,
    chrono::Utc::now() + chrono::Duration::hours(1),
).await?;

// Retry policy: exponential 1s, 2s, 4s, capped at 5 minutes, 5 attempts
let queue = QueueRepository::with_retry_policy(
    mgr.pool().clone(),
    RetryPolicy::new(
        5,
        Backoff::Exponential {
            base: Duration::from_secs(1),
            factor: 2,
            max: Duration::from_secs(300),
        },
    )?,
);

match queue.fail(mgr.pool(), &emails, job.id, job.attempts).await? {
    FailOutcome::Retried { attempts, deliver_at } => println!("attempt {attempts}, next at {deliver_at}"),
    FailOutcome::Dead { attempts } => println!("dead after {attempts} attempts"),
    FailOutcome::StaleClaim => println!("the lease expired and another worker owns this job"),
}
```

`job.attempts` comes from the claim (`delivery_attempts` in the table), so pass
it straight back to `fail`. `fail` checks it against the row and writes only if
they agree, which means a late call from a handler that overran its lease is
reported as `FailOutcome::StaleClaim` instead of resurrecting a job that has
already been buried. `RetryPolicy::new` rejects zero and anything above
`i32::MAX` attempts, and `Backoff::Exponential`'s `factor` is the growth factor
per attempt (`factor: 2` doubles).

A job that reaches `dead` is the signal that a queue is not draining. It is no
longer `pending`, so read `dead_count` (or `FailOutcome::Dead`) rather than
`pending_count` to see it. `pending_count` counts every `pending` row,
including jobs whose `deliver_at` is still in the future.

### Pub/Sub

```rust
// Publisher
mgr.notify.notify(mgr.pool(), "events.user_signup", r#"{"user_id": 42}"#).await?;

// Subscriber
let mut listener = mgr.create_listener().await?;
listener.listen("events.user_signup").await?;

loop {
    if let Some(msg) = listener.recv_timeout(Duration::from_secs(5)).await? {
        println!("{}: {}", msg.channel, msg.payload);
    }
}
```

### Cache

```rust
// Set with 5 minute TTL
mgr.cache.set("user:42:profile", &profile_data, 300).await?;

// Get (returns None if expired or missing)
let cached: Option<Profile> = mgr.cache.get("user:42:profile").await?;

// Get-or-set with fallback
let profile = mgr.cache.get_or_set("user:42:profile", 300, || async {
    fetch_profile_from_db(42).await
}).await?;
```

### Request-Response (RPC)

```rust
let workers = QueueName::new("rpc_workers")?;

// Caller: push request and block until response
let result: MyResponse = mgr.request_response
    .push_and_wait(&workers, &my_request, Duration::from_secs(10))
    .await?;

// Worker: process and respond
if let Some(job) = mgr.queue.pop::<RequestWrapper>(&workers).await? {
    let response = handle(job.payload);
    mgr.request_response
        .store_response(mgr.pool(), &job.payload.request_id, &response)
        .await?;
    mgr.queue.complete(mgr.pool(), &workers, job.id).await?;
}
```

## Who this is for

A solo dev or small team shipping a side project or internal tool.
You already run Postgres, your job volume is tens to hundreds per second,
and you need basic queue/pubsub/cache without adding infrastructure.

## The lease contract

`reap_stale` returns any job still in `processing` after the lease expires.
The lease must therefore exceed the longest legitimate handler run. If a
handler can run longer than the lease, the reaper hands a still-running job to
a second worker and it runs twice.

This crate cannot enforce that, so it is on you. A handler that needs to
outlive a fixed lease needs a heartbeat, which is not implemented here — pick
a lease with real headroom, or use sqlxmq.

## What's missing

This is deliberately minimal. Things you won't find here:

- Priority levels
- Job timeouts or heartbeats (see the lease contract above)
- Idempotency keys (duplicate push protection)
- Partitioning or sharding across queue tables
- Topic-to-queue fan-out with a persisted routing table
- Replaying dead jobs (the `dead` status covers the need until you want it)
- Queue names longer than 43 characters, or names the database truncates
- Connection reuse in request-response (opens a new PG connection per RPC call)
- Cache eviction beyond TTL expiry (no LRU, no max-memory cap, no thundering
  herd protection in `get_or_set`)

When any of those becomes a requirement, graduate to sqlxmq or Redis —
the migration surface is small and the API patterns are similar.

## Tradeoffs

The cache uses `UNLOGGED` tables — faster writes, but data is lost on
crash. That's the correct tradeoff for a cache (the source of truth lives
elsewhere), but don't store anything you can't recompute.

Pub/sub payloads are capped at 8KB (PostgreSQL NOTIFY limit). For larger
messages, store the payload in a table and notify with a row ID instead.

## Testing

`cargo test` runs the unit tests without a database. The integration tests in
`tests/queue_integration.rs` are gated on `DATABASE_URL` and skip cleanly when
it is unset:

```bash
DATABASE_URL=postgres://localhost/pgq_test cargo test
```

## License

MIT
