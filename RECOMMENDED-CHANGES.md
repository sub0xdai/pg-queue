# pg-queue: recommended changes

Status: proposal only, nothing here is implemented yet.
Written 2026-10-13 against `pg-queue` at revision `d747d60`.
Compared against the article "A zero-dependency Pub/Sub implementation on top of PostgreSQL" (markusy.dev) and the source of `markus-ylisiurunen/go-opinionatedevents` at revision `403a3fc` (2033 lines, non-test).

## Why change anything

`README.md:26-29` leads with the claim that this crate gives you "atomic enqueue within a transaction".
`src/queue.rs:102-114` has no transaction parameter, no `Executor` generic, and no `&mut PgConnection` overload, so the claim cannot be met.
The Go library implements that feature for real.
Its `postgresTransactionProvider.do` (`destination_postgres.go:110-142`) detects a caller-supplied transaction and refuses to commit or roll back on the caller's behalf.
Separately, `pop` commits `status = 'processing'` as a standalone statement (`src/queue.rs:121-135`), so a consumer that dies mid-job leaves that row stranded forever, because nothing in the crate reclaims it.
The Go library avoids that by running the handler inside the claim transaction (`source_postgres.go:336-380`).
That Go approach holds a pooled connection for the whole job, which is the wrong trade for a small Rust pool, so this plan uses a lease instead.

The changes below close those three gaps, add scheduling, and make the README true.

## Summary of changes

| # | Change | Files | Effort |
|---|---|---|---|
| 1 | Executor-aware write API (transaction-safe publish) | `src/queue.rs`, `src/notify.rs`, `src/request_response.rs` | 1 hour |
| 2 | Schema v2: `deliver_at`, `delivery_attempts`, new indexes, two triggers, upgrade path | `migrations/setup.sql` | 1 hour |
| 3 | `Backoff` enum, `RetryPolicy`, `Job.attempts`, `FailOutcome` | `src/queue.rs`, `src/errors.rs` | 2 hours |
| 4 | `push_at` for scheduled delivery | `src/queue.rs` | 30 minutes |
| 5 | `run_worker` helper (the aggregated trigger) | `src/worker.rs`, `src/lib.rs` | 1 hour |
| 6 | Lease reclaim plus reaping of exhausted jobs | `src/queue.rs`, `src/worker.rs` | 1 hour |
| 7 | README truth-up | `README.md` | 30 minutes |

Changes 2, 3, 4, and 6 land together, because they touch the same SQL.
Changes 1 and 5 are independent.
Change 7 comes last, so the docs describe what shipped.

## Change 1: executor-aware write API

This is the highest-value change and it is also the smallest.
Every write method currently binds `&self.pool` internally.
Make the executor a parameter instead, so the caller decides.

```rust
pub async fn push<'e, E>(&self, executor: E, queue: &QueueName, payload: &T) -> Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
```

`&PgPool`, `&mut PgConnection`, and `&mut sqlx::Transaction<'_, Postgres>` all implement `Executor`, so a caller that has no transaction passes `&self.pool` and nothing else changes.
Apply the same shape to `complete`, `fail`, and `NotifyService::notify`.
Do not add an executor parameter to `pop`, `reap_stale`, or `pending_count`.
Claims must stay short autocommit statements, so a caller cannot hold a claim open while doing unrelated work.

Call sites to update:

- `src/request_response.rs:51` becomes `self.queue.push(&self.pool, queue, &wrapped).await?`.
- `src/request_response.rs:72-76` (`store_response`) becomes executor-aware for the same reason.
- `src/lib.rs:31-37` doc example becomes `mgr.queue.push(&mgr.pool(), &emails, &payload).await?`.

Add one unit test that pins the API shape, since the value is the type signature and not runtime behavior.

Acceptance: a caller can do `queue.push(&mut *tx, &q, &payload).await?` inside their own transaction, and `tx.commit()` persists both the domain row and the queue row, while `tx.rollback()` persists neither.

## Change 2: schema v2

### New columns

```sql
deliver_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
delivery_attempts INTEGER NOT NULL DEFAULT 0
```

`deliver_at` is the earliest time a job may be claimed.
It serves both retry backoff and future scheduling, which is exactly the "single timestamp column" the article calls out.
`delivery_attempts` counts claim attempts, not failures, so it doubles as the current attempt number handed to the handler.

### New status value

Add `Dead` to `JobStatus` (`src/queue.rs:59-81`) mapping to the string `dead`.
A job that exhausts `max_attempts` moves to `dead` and is never claimed again.
This is the poor man's dead letter queue, matching the Go library's `dropped` status (`source_postgres.go:480-492`).

### Indexes

Replace the current pending index with a ready index, and add a stale index.

```sql
CREATE INDEX IF NOT EXISTS idx_queue_demo_ready ON queue_demo(deliver_at, created_at) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_queue_demo_stale ON queue_demo(processed_at) WHERE status = 'processing';
```

Drop `idx_queue_{name}_pending` in the upgrade, since `(created_at) WHERE status = 'pending'` no longer matches the claim predicate.

### Triggers

The current single trigger fires only on `INSERT` (`migrations/setup.sql:46-55`), so a job returned to `pending` by `fail` never wakes a waiting consumer.
Use two triggers rather than one combined trigger.
A combined `AFTER INSERT OR UPDATE ... WHEN (... OLD.status ...)` is rejected by PostgreSQL with "INSERT trigger's WHEN condition cannot reference OLD values", so it must be split.

```sql
CREATE TRIGGER queue_demo_notify_insert
    AFTER INSERT ON queue_demo
    FOR EACH ROW WHEN (NEW.status = 'pending')
    EXECUTE FUNCTION notify_queue_demo();

CREATE TRIGGER queue_demo_notify_retry
    AFTER UPDATE ON queue_demo
    FOR EACH ROW WHEN (NEW.status = 'pending' AND OLD.status IS DISTINCT FROM 'pending')
    EXECUTE FUNCTION notify_queue_demo();
```

Do not filter the retry trigger on `deliver_at <= NOW()`.
A job scheduled an hour out must still wake consumers when it is scheduled, because the worker's poll interval is what eventually observes it as due.

### The updated creator function

`pg_queue_create_queue` must stay idempotent and must now upgrade an existing table.
Replace the body in `migrations/setup.sql:10-57` with this.

```sql
CREATE OR REPLACE FUNCTION pg_queue_create_queue(queue_name TEXT) RETURNS void AS $$
DECLARE
    table_name       TEXT := 'queue_' || queue_name;
    ready_index      TEXT := 'idx_queue_' || queue_name || '_ready';
    stale_index      TEXT := 'idx_queue_' || queue_name || '_stale';
    old_index        TEXT := 'idx_queue_' || queue_name || '_pending';
    func_name        TEXT := 'notify_queue_' || queue_name;
    insert_trig      TEXT := 'queue_' || queue_name || '_notify_insert';
    retry_trig       TEXT := 'queue_' || queue_name || '_notify_retry';
    old_trig         TEXT := 'queue_' || queue_name || '_notify';
BEGIN
    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I (
            id BIGSERIAL PRIMARY KEY,
            payload JSONB NOT NULL,
            status TEXT NOT NULL DEFAULT ''pending'',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            processed_at TIMESTAMPTZ,
            deliver_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            delivery_attempts INTEGER NOT NULL DEFAULT 0
        )', table_name
    );

    EXECUTE format('ALTER TABLE %I ADD COLUMN IF NOT EXISTS deliver_at TIMESTAMPTZ NOT NULL DEFAULT NOW()', table_name);
    EXECUTE format('ALTER TABLE %I ADD COLUMN IF NOT EXISTS delivery_attempts INTEGER NOT NULL DEFAULT 0', table_name);

    EXECUTE format('DROP INDEX IF EXISTS %I', old_index);
    EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %I(deliver_at, created_at) WHERE status = ''pending''', ready_index, table_name);
    EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %I(processed_at) WHERE status = ''processing''', stale_index, table_name);

    EXECUTE format(
        'CREATE OR REPLACE FUNCTION %I() RETURNS TRIGGER AS $fn$
         BEGIN
             PERFORM pg_notify(%L, NEW.id::text);
             RETURN NULL;
         END;
         $fn$ LANGUAGE plpgsql',
        func_name, table_name
    );

    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', old_trig, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', insert_trig, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', retry_trig, table_name);

    EXECUTE format(
        'CREATE TRIGGER %I AFTER INSERT ON %I
            FOR EACH ROW WHEN (NEW.status = ''pending'')
            EXECUTE FUNCTION %I()',
        insert_trig, table_name, func_name
    );

    EXECUTE format(
        'CREATE TRIGGER %I AFTER UPDATE ON %I
            FOR EACH ROW WHEN (NEW.status = ''pending'' AND OLD.status IS DISTINCT FROM ''pending'')
            EXECUTE FUNCTION %I()',
        retry_trig, table_name, func_name
    );
END;
$$ LANGUAGE plpgsql;
```

### Upgrading existing queues

Existing deployments already have `queue_*` tables without the new columns.
Re-running `pg_queue_create_queue` per queue fixes them, because the function now issues `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` and recreates the triggers.
Add this block to the end of `migrations/setup.sql` so the upgrade is one command.

```sql
DO $$
DECLARE t RECORD;
BEGIN
    FOR t IN
        SELECT table_name FROM information_schema.tables
        WHERE table_schema = 'public' AND table_name ~ '^queue_'
    LOOP
        PERFORM pg_queue_create_queue(substring(t.table_name from 7));
    END LOOP;
END;
$$;
```

`substring(t.table_name from 7)` strips the `queue_` prefix.
Keep the same loop in `migrations/teardown.sql` where it drops per-queue objects, so teardown also removes the two new triggers.

Acceptance: running `setup.sql` twice in a row succeeds, and an existing v1 table gains both columns with `deliver_at` backfilled to `NOW()` and `delivery_attempts` backfilled to `0`.

## Change 3: retry policy

Use a closed enum for backoff rather than a trait.
The set of strategies is known, so an enum keeps it exhaustive and avoids a trait object at the call site.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    Constant(Duration),
    Linear { base: Duration, step: Duration, max: Duration },
    Exponential { base: Duration, factor: u32, max: Duration },
}

impl Backoff {
    pub fn deliver_after(&self, attempt: u32) -> Duration {
        match self {
            Self::Constant(d) => *d,
            Self::Linear { base, step, max } => {
                let n = base.saturating_add(step.saturating_mul(attempt.saturating_sub(1)));
                n.min(*max)
            }
            Self::Exponential { base, factor, max } => {
                let shift = factor.saturating_mul(attempt.saturating_sub(1)).min(20);
                base.saturating_mul(2u32.saturating_pow(shift)).min(*max)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: Backoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            backoff: Backoff::Exponential {
                base: Duration::from_secs(1),
                factor: 1,
                max: Duration::from_secs(300),
            },
        }
    }
}
```

`factor` is the exponent multiplier per attempt, so `factor: 1` gives 1s, 2s, 4s, 8s and so on.
The shift is capped at 20 to keep the multiplication in range on `u32`.

`QueueRepository` gains a `retry: RetryPolicy` field.
`QueueRepository::new(pool)` uses `RetryPolicy::default()`, and `QueueRepository::with_retry_policy(pool, policy)` overrides it.

`Job` gains the attempt number.

```rust
#[derive(Debug)]
pub struct Job<T> {
    pub id: i64,
    pub payload: T,
    pub attempts: u32,
}
```

`fail` applies the policy in a single statement and reports what happened.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    Retried { attempt: u32, deliver_at: DateTime<Utc> },
    Dead { attempts: u32 },
}
```

```sql
UPDATE queue_demo
SET status = CASE WHEN $2 >= $4 THEN 'dead' ELSE 'pending' END,
    processed_at = NULL,
    deliver_at = CASE WHEN $2 >= $4 THEN deliver_at ELSE $3 END
WHERE id = $1
RETURNING status
```

Bind order is `$1` job id, `$2` attempts, `$3` computed `deliver_at`, `$4` max attempts.
Read the returned status and map it to `FailOutcome`, so the caller can log a dead job or alert on it rather than silently losing it.

Returning `FailOutcome` rather than `()` matters, because a job that reaches `dead` is the only signal that a queue is not draining.

Acceptance: a job that fails `max_attempts` times ends as `dead` and is never returned by `pop` again.

## Change 4: scheduled delivery

Add `push_at` and make `push` delegate to it.

```rust
pub async fn push_at<'e, E>(
    &self,
    executor: E,
    queue: &QueueName,
    payload: &T,
    deliver_at: DateTime<Utc>,
) -> Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
```

The statement becomes `INSERT INTO {table} (payload, deliver_at) VALUES ($1, $2) RETURNING id`.
`push` calls `push_at` with `Utc::now()`.

This is what unlocks the article's cron and scheduled-work patterns, and it costs one column and one method.

Acceptance: a job pushed with `deliver_at` an hour out is not returned by `pop` before that time and is returned after it.

## Change 5: worker helper

The crate currently ships no consumer loop, and the pub/sub example in `README.md:83-96` shows a bare `recv_timeout` loop.
That loop silently misses any job inserted before the listener started, and it never observes a job whose `deliver_at` has come due since the last wake.
Ship the loop so callers do not get it wrong.

Add `src/worker.rs` with a single function.

```rust
pub async fn run_worker<T, F, Fut>(
    queue: &QueueRepository,
    name: &QueueName,
    listener: &mut ListenerService,
    lease: Duration,
    poll_interval: Duration,
    max_per_cycle: usize,
    mut handle: F,
) -> Result<()>
where
    T: DeserializeOwned,
    F: FnMut(Job<T>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
```

The body does the following, in order, forever.

1. Call `queue.reap_stale(name, lease).await?`.
2. Pop and handle jobs until `pop` returns `None` or `max_per_cycle` is reached, whichever comes first.
3. On `Ok(())` from the handler call `complete`.
4. On `Err(_)` from the handler call `fail` and ignore `FailOutcome::Retried` while surfacing `FailOutcome::Dead` to the caller's logging.
5. When the pop loop finds nothing, call `listener.recv_timeout(poll_interval).await` and ignore the result.

Step 5 is the aggregated trigger. The timeout is the interval poll, and the notify is the fast path. This is the same composition as `postgresSourceAggregateTrigger` in the Go library (`source_postgres.go:108-140`).

Keep `max_per_cycle` explicit, because an unbounded inner drain loop starves the interval poll and makes the lease reclaim in step 1 stop running.

Acceptance: a job inserted before the worker starts is handled within one poll interval, and a job whose `deliver_at` passes while the worker is idle is handled within one poll interval.

## Change 6: lease reclaim

Keep `pop` as a fast autocommit claim, and move recovery onto the poll tick.
This keeps `pop` on a single partial index and avoids the planner working around an `OR` across two statuses on the hot path.

### `pop` claims only ready pending rows

```sql
UPDATE queue_demo
SET status = 'processing',
    processed_at = NOW(),
    delivery_attempts = delivery_attempts + 1
WHERE id = (
    SELECT id FROM queue_demo
    WHERE status = 'pending' AND deliver_at <= NOW()
    ORDER BY deliver_at, created_at
    FOR UPDATE SKIP LOCKED
    LIMIT 1
)
RETURNING id, payload, delivery_attempts
```

Returning `delivery_attempts` is what populates `Job.attempts`.

### `reap_stale` recovers abandoned claims

```sql
UPDATE queue_demo
SET status = 'pending', processed_at = NULL
WHERE status = 'processing'
  AND processed_at < NOW() - make_interval(secs => $1)
  AND delivery_attempts < $2
RETURNING id
```

Bind `$1` as the lease in seconds and `$2` as `max_attempts`.
Reclaim does not increment attempts, because the claim already counted the attempt that the crash interrupted.

### `reap_exhausted` buries jobs that keep crashing

```sql
UPDATE queue_demo
SET status = 'dead'
WHERE status = 'processing'
  AND processed_at < NOW() - make_interval(secs => $1)
  AND delivery_attempts >= $2
RETURNING id
```

Without this, a job that reliably kills the worker is reclaimed forever, because `fail` never runs and so `max_attempts` is never consulted.
Call both reap functions from step 1 of the worker.
Return the affected ids so the worker can log them.

Both statements fire the retry trigger on the transition back to `pending`, so a reclaimed job also wakes waiting consumers.

Acceptance: with a job claimed and then abandoned, setting `processed_at` into the past by more than the lease makes the next `reap_stale` return that row and the next `pop` claim it.

### The lease contract

The lease must exceed the longest legitimate handler run.
If a handler can run longer than the lease, `reap_stale` will hand a still-running job to a second worker.
This crate cannot enforce that, so state it in the API docs and in the README.
A handler that needs to outlive a fixed lease needs a heartbeat, which is out of scope here.

## Change 7: README truth-up

`README.md` has uncommitted changes in git, so rebase this edit onto those first.

- Lines 26-29: the atomic enqueue claim becomes true after change 1, so keep it and add the `push(&mut *tx, ...)` example so it is verifiable.
- Lines 12 and 137-152: move retries, dead lettering, and delayed jobs out of "What's missing" and into the feature table, since changes 2 through 4 add them.
- Lines 137-152: keep the remaining honest gaps, and add the lease contract from change 6 to that list, because a lease is a real constraint on handlers.
- Line 12: "A ~500 line sqlx wrapper" understates it. `src/` is 784 lines today, before these changes. State the real number or drop the number.
- Lines 21-24: "no polling" becomes "notify-driven with a poll fallback", since change 5 makes polling load-bearing for scheduled and reclaimed jobs.
- Lines 83-96: replace the bare `recv_timeout` loop with `run_worker`.
- Add a "Retries and scheduling" usage section covering `push_at`, `RetryPolicy`, and `FailOutcome`.

## Execution order

1. Change 1 alone. It is additive and compiles against the current schema.
2. Changes 2, 3, 4, and 6 together, since they share the same SQL surface and the crate will not behave correctly with only some of them.
3. Change 5, which depends on `reap_stale` from change 6.
4. Change 7 last.

After step 1, `cargo test` and `cargo clippy --all-targets -- -D warnings` must both pass before moving on.

## Verification

The crate has 17 unit tests and every one is a pure string or struct assertion, so none of this SQL is covered today.
Run the following before and after, and treat the SQL as untested until an integration test covers it.

Commands:

```
cargo test
cargo clippy --all-targets -- -D warnings
psql -f migrations/setup.sql "$DATABASE_URL"
psql -f migrations/setup.sql "$DATABASE_URL"
```

The double `setup.sql` run is the idempotency check.

Manual end-to-end checks, each as a separate `psql` session or script:

1. `SELECT pg_queue_create_queue('demo');` then `\d queue_demo` shows both new columns and two indexes.
2. Insert a ready job, confirm `pop` returns it, confirm `complete` moves it to `completed`.
3. Insert a job with `deliver_at = NOW() + interval '1 hour'`, confirm `pop` returns nothing, then set `deliver_at = NOW() - interval '1 second'` and confirm `pop` returns it.
4. Claim a job, call `fail` with `attempts = 1` under a policy of `max_attempts = 3`, confirm `deliver_at` moved into the future and status is `pending`.
5. Repeat step 4 until `attempts = 3`, confirm status becomes `dead` and `pop` never returns it again.
6. Claim a job, run `UPDATE queue_demo SET processed_at = NOW() - interval '10 minutes' WHERE id = $1`, then call `reap_stale` with a 60 second lease and confirm it returns that id and a subsequent `pop` claims it.
7. In session A run `LISTEN queue_demo;`. In session B insert a job and then fail it. Confirm session A receives two notifications, one for the insert and one for the return to `pending`.
8. Confirm `delivery_attempts` reads `1` after the first claim and `2` after a reclaim.

Add `tests/queue_integration.rs` gated on `std::env::var("DATABASE_URL")` so steps 2 through 8 run in CI when a database is present.
Skip the test body when the variable is unset, so the suite still passes without a database.

Atomicity check for change 1, which cannot be done in raw SQL:

Begin a transaction, insert into a scratch domain table and call `push` on the same transaction, then roll back, and assert the queue table gained no row.
Repeat with a commit and assert the row is present.

## Acceptance criteria

- A caller can enqueue inside their own transaction, and a rollback drops the queue row.
- A job that is claimed and then abandoned is reclaimed and re-handled after the lease elapses.
- A job that fails repeatedly ends in `dead` and is reported through `FailOutcome`.
- A job scheduled in the future is not claimed early and is claimed after its `deliver_at`.
- A job inserted while no consumer is listening is handled within one poll interval.
- `setup.sql` is idempotent and upgrades a v1 table in place.
- `cargo clippy --all-targets -- -D warnings` exits 0.
- No statement in the hot path holds a pooled connection for the duration of a handler run.

## Out of scope

Deliberately not in this plan, so the diff stays small.

- Topic-to-queue fan-out and a persisted routing table. The Go library has one, and the article itself calls the publisher-side variant a wart. Revisit only if one event must reach independent consumer groups.
- A separate dead letter table. The `dead` status covers the need until someone wants to replay.
- Priority levels, idempotency keys, job heartbeats, partitioning.
- Cache eviction beyond TTL, which stays as documented.
- Reworking the `UNLOGGED` cache or the request-response path beyond the executor parameter in change 1.
