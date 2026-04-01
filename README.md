# pg-queue

I was already running postgres and didnt want to add redis just for bg jobs.

## How it works

| Feature | Mechanism | What it replaces |
|---------|-----------|-----------------|
| **Job queue** | `SELECT ... FOR UPDATE SKIP LOCKED` | Redis lists, Celery, Sidekiq |
| **Pub/Sub** | `LISTEN` / `NOTIFY` | Redis pub/sub, NATS |
| **Cache** | `UNLOGGED` table with TTL per row | Redis `GET`/`SET`/`EXPIRE` |
| **Request-Response** | Queue + `NOTIFY` with correlation ID | Redis streams, RPC frameworks |

`SKIP LOCKED` lets multiple workers grab jobs concurrently without blocking each other. `LISTEN/NOTIFY` pushes events to subscribers the instant a row is inserted — no polling. `UNLOGGED` tables skip write-ahead logging for cache-tier speed (data is not crash-safe, which is fine for a cache).

## Setup

Run the migration against your database:

```bash
psql -f migrations/setup.sql your_database
```

Then create as many queues as you need:

```sql
SELECT pg_queue_create_queue('emails');
SELECT pg_queue_create_queue('image_processing');
```

Each call creates a dedicated table (`queue_emails`, `queue_image_processing`) with a partial index on pending jobs and a trigger that fires `NOTIFY` on insert.

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
mgr.queue.push(&emails, &serde_json::json!({
    "to": "user@example.com",
    "template": "welcome"
})).await?;

// Consumer: claim and process
if let Some(job) = mgr.queue.pop::<serde_json::Value>(&emails).await? {
    // process job.payload ...
    mgr.queue.complete(&emails, job.id).await?;
}
```

Multiple consumers can call `pop` concurrently — `SKIP LOCKED` ensures each job is claimed by exactly one worker.

### Pub/Sub

```rust
// Publisher
mgr.notify.notify("events.user_signup", r#"{"user_id": 42}"#).await?;

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
    mgr.request_response.store_response(&job.payload.request_id, &response).await?;
    mgr.queue.complete(&workers, job.id).await?;
}
```

## Use cases

- **Background job processing** — email delivery, image resizing, PDF generation, webhook dispatch. Any work that should happen outside the request cycle.
- **Event-driven microservices** — publish domain events (order placed, user signed up) and let downstream services react via `LISTEN/NOTIFY` without polling.
- **Caching hot data** — store frequently-read, expensive-to-compute results (API responses, aggregations, session data) with automatic TTL expiry.
- **Task orchestration** — fan-out work to a pool of workers with guaranteed exactly-once delivery via `SKIP LOCKED`.
- **Reducing infrastructure** — if your Redis is only doing job queues and caching, pg-queue lets you drop it entirely and run everything on the Postgres you already have.

## Tradeoffs

This is the right tool when your job volume fits comfortably within PostgreSQL's throughput (tens of thousands of jobs per second on modern hardware). If you need millions of messages per second or global pub/sub across data centres, dedicated message brokers like Kafka or NATS are better suited.

The cache uses `UNLOGGED` tables — faster writes, but data is lost on crash. That's the correct tradeoff for a cache (the source of truth lives elsewhere), but don't store anything you can't recompute.

## License

MIT OR Apache-2.0
