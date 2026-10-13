use pg_queue::{
    run_worker, Backoff, CompleteOutcome, FailOutcome, ListenerService, QueueName,
    QueueRepository, RetryPolicy, MAX_QUEUE_NAME_LEN,
};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SETUP_SQL: &str = include_str!("../migrations/setup.sql");
static MIGRATION: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
static DDL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn database_url() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            eprintln!("skipping integration test: DATABASE_URL unset");
            None
        }
    }
}

async fn connect() -> Option<PgPool> {
    let url = database_url()?;
    let pool = PgPool::connect(&url).await.expect("connect to DATABASE_URL");
    let _guard = DDL_LOCK.lock().await;
    MIGRATION
        .get_or_init(|| async {
            sqlx::raw_sql(SETUP_SQL)
                .execute(&pool)
                .await
                .expect("apply migrations/setup.sql");
        })
        .await;
    Some(pool)
}

async fn new_queue(pool: &PgPool) -> QueueName {
    let name = QueueName::new(format!("it_{}", uuid::Uuid::new_v4().simple())).unwrap();
    create_queue(pool, name.name()).await.expect("create queue");
    name
}

async fn create_queue(pool: &PgPool, name: &str) -> Result<(), sqlx::Error> {
    let _guard = DDL_LOCK.lock().await;
    sqlx::query("SELECT pg_queue_create_queue($1)")
        .bind(name)
        .execute(pool)
        .await
        .map(|_| ())
}

async fn drop_queue(pool: &PgPool, queue: &QueueName) {
    let _guard = DDL_LOCK.lock().await;
    sqlx::query(&format!("DROP TABLE IF EXISTS {}", queue.table_name()))
        .execute(pool)
        .await
        .expect("drop queue table");
    sqlx::query(&format!("DROP FUNCTION IF EXISTS notify_queue_{}()", queue.name()))
        .execute(pool)
        .await
        .expect("drop notify function");
}

async fn status_of(pool: &PgPool, queue: &QueueName, id: i64) -> String {
    sqlx::query_scalar(&format!(
        "SELECT status FROM {} WHERE id = $1",
        queue.table_name()
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read status")
}

async fn attempts_of(pool: &PgPool, queue: &QueueName, id: i64) -> i32 {
    sqlx::query_scalar(&format!(
        "SELECT delivery_attempts FROM {} WHERE id = $1",
        queue.table_name()
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read attempts")
}

async fn make_due(pool: &PgPool, queue: &QueueName, id: i64) {
    sqlx::query(&format!(
        "UPDATE {} SET deliver_at = NOW() - interval '1 second' WHERE id = $1",
        queue.table_name()
    ))
    .bind(id)
    .execute(pool)
    .await
    .expect("make due");
}

async fn set_processed_at_past(pool: &PgPool, queue: &QueueName, id: i64) {
    sqlx::query(&format!(
        "UPDATE {} SET processed_at = NOW() - interval '10 minutes' WHERE id = $1",
        queue.table_name()
    ))
    .bind(id)
    .execute(pool)
    .await
    .expect("age processed_at");
}

fn policy(max_attempts: u32, backoff: Backoff) -> RetryPolicy {
    RetryPolicy::new(max_attempts, backoff).expect("valid retry policy")
}

#[tokio::test]
async fn push_pop_complete_round_trip() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());

    let id = repo
        .push(&pool, &queue, &serde_json::json!({"n": 1}))
        .await
        .unwrap();
    assert_eq!(repo.pending_count(&queue).await.unwrap(), 1);

    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(job.id, id);
    assert_eq!(job.attempts, 1, "first claim is attempt 1");
    assert_eq!(job.payload, serde_json::json!({"n": 1}));
    assert_eq!(status_of(&pool, &queue, id).await, "processing");

    assert_eq!(
        repo.complete(&pool, &queue, id).await.unwrap(),
        CompleteOutcome::Completed
    );
    assert_eq!(status_of(&pool, &queue, id).await, "completed");
    assert_eq!(repo.pending_count(&queue).await.unwrap(), 0);
    assert_eq!(repo.dead_count(&queue).await.unwrap(), 0);
    assert!(repo.pop::<serde_json::Value>(&queue).await.unwrap().is_none());

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn complete_twice_reports_a_stale_claim() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());

    let _id = repo
        .push(&pool, &queue, &serde_json::json!({"n": 1}))
        .await
        .unwrap();
    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();

    assert_eq!(
        repo.complete(&pool, &queue, job.id).await.unwrap(),
        CompleteOutcome::Completed
    );
    assert_eq!(
        repo.complete(&pool, &queue, job.id).await.unwrap(),
        CompleteOutcome::StaleClaim,
        "a second completion must not claim to have done work"
    );

    let missing = repo.complete(&pool, &queue, 999_999_999).await.unwrap();
    assert_eq!(missing, CompleteOutcome::StaleClaim);

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn scheduled_job_waits_for_deliver_at() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());

    let id = repo
        .push_at(
            &pool,
            &queue,
            &serde_json::json!("later"),
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();

    assert!(repo.pop::<serde_json::Value>(&queue).await.unwrap().is_none());

    make_due(&pool, &queue, id).await;
    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(job.id, id);

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn push_uses_the_database_clock() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());

    let id = repo
        .push(&pool, &queue, &serde_json::json!("now"))
        .await
        .unwrap();

    let ready: bool = sqlx::query_scalar(&format!(
        "SELECT deliver_at <= NOW() FROM {} WHERE id = $1",
        queue.table_name()
    ))
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(ready, "a plain push must be claimable immediately");

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn fail_backs_off_then_buries_the_job() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::with_retry_policy(
        pool.clone(),
        policy(3, Backoff::Constant(Duration::from_secs(60))),
    );

    let id = repo
        .push(&pool, &queue, &serde_json::json!("boom"))
        .await
        .unwrap();

    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(job.attempts, 1);
    let outcome = repo.fail(&pool, &queue, job.id, job.attempts).await.unwrap();
    assert!(matches!(
        outcome,
        FailOutcome::Retried { attempts: 1, .. }
    ));
    assert_eq!(status_of(&pool, &queue, id).await, "pending");
    assert!(
        repo.pop::<serde_json::Value>(&queue).await.unwrap().is_none(),
        "backoff must hold the job back"
    );

    make_due(&pool, &queue, id).await;
    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(job.attempts, 2);
    repo.fail(&pool, &queue, job.id, job.attempts).await.unwrap();

    make_due(&pool, &queue, id).await;
    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(job.attempts, 3);
    let outcome = repo.fail(&pool, &queue, job.id, job.attempts).await.unwrap();
    assert_eq!(outcome, FailOutcome::Dead { attempts: 3 });

    assert_eq!(status_of(&pool, &queue, id).await, "dead");
    assert_eq!(repo.dead_count(&queue).await.unwrap(), 1);
    assert_eq!(repo.pending_count(&queue).await.unwrap(), 0);
    assert!(repo.pop::<serde_json::Value>(&queue).await.unwrap().is_none());

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn a_late_fail_cannot_resurrect_a_dead_job() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::with_retry_policy(
        pool.clone(),
        policy(3, Backoff::Constant(Duration::from_secs(1))),
    );

    let id = repo
        .push(&pool, &queue, &serde_json::json!("slow"))
        .await
        .unwrap();

    let first = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(first.attempts, 1);

    set_processed_at_past(&pool, &queue, id).await;
    repo.reap_stale(&queue, Duration::from_secs(60)).await.unwrap();

    let second = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    repo.fail(&pool, &queue, second.id, second.attempts)
        .await
        .unwrap();
    make_due(&pool, &queue, id).await;

    let third = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(
        repo.fail(&pool, &queue, third.id, third.attempts).await.unwrap(),
        FailOutcome::Dead { attempts: 3 }
    );
    assert_eq!(status_of(&pool, &queue, id).await, "dead");

    let late = repo.fail(&pool, &queue, id, first.attempts).await.unwrap();
    assert_eq!(late, FailOutcome::StaleClaim);
    assert_eq!(
        status_of(&pool, &queue, id).await,
        "dead",
        "a stale claim must not return a buried job to pending"
    );

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn fail_with_a_mismatched_attempts_is_a_stale_claim() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::with_retry_policy(
        pool.clone(),
        policy(1, Backoff::Constant(Duration::from_secs(1))),
    );

    let id = repo
        .push(&pool, &queue, &serde_json::json!("boom"))
        .await
        .unwrap();
    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(attempts_of(&pool, &queue, id).await, 1);
    assert_eq!(
        repo.fail(&pool, &queue, id, 0).await.unwrap(),
        FailOutcome::StaleClaim,
        "an attempt count that does not match the row must not be acted on"
    );
    assert_eq!(status_of(&pool, &queue, id).await, "processing");

    assert_eq!(
        repo.fail(&pool, &queue, id, u32::MAX).await.unwrap(),
        FailOutcome::StaleClaim
    );
    assert_eq!(status_of(&pool, &queue, id).await, "processing");

    assert_eq!(
        repo.fail(&pool, &queue, job.id, job.attempts).await.unwrap(),
        FailOutcome::Dead { attempts: 1 }
    );
    assert_eq!(status_of(&pool, &queue, id).await, "dead");

    drop_queue(&pool, &queue).await;
}

#[derive(serde::Deserialize, Debug)]
struct OnlyObject {
    #[allow(dead_code)]
    n: i64,
}

#[tokio::test]
async fn an_undecodable_payload_is_released_instead_of_stranded() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::with_retry_policy(
        pool.clone(),
        policy(3, Backoff::Constant(Duration::from_secs(60))),
    );

    let id = repo
        .push(&pool, &queue, &serde_json::json!("not an object"))
        .await
        .unwrap();

    let popped = repo.pop::<OnlyObject>(&queue).await;
    assert!(popped.is_err(), "the decode failure must reach the caller");
    assert_eq!(
        status_of(&pool, &queue, id).await,
        "pending",
        "the claim must be released, not left in processing"
    );
    assert_eq!(attempts_of(&pool, &queue, id).await, 1);
    assert!(
        repo.pop::<OnlyObject>(&queue).await.unwrap().is_none(),
        "the released job must honour its backoff"
    );

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn reap_stale_reclaims_and_reap_exhausted_buries() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::with_retry_policy(
        pool.clone(),
        policy(2, Backoff::Constant(Duration::from_secs(1))),
    );

    let id = repo
        .push(&pool, &queue, &serde_json::json!("abandoned"))
        .await
        .unwrap();

    let claimed = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(claimed.attempts, 1);
    set_processed_at_past(&pool, &queue, id).await;

    assert_eq!(
        repo.reap_stale(&queue, Duration::from_secs(60)).await.unwrap(),
        vec![id]
    );
    assert_eq!(status_of(&pool, &queue, id).await, "pending");

    let reclaimed = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    assert_eq!(reclaimed.attempts, 2, "reclaim does not add an attempt");
    assert_eq!(attempts_of(&pool, &queue, id).await, 2);

    set_processed_at_past(&pool, &queue, id).await;
    assert!(
        repo.reap_stale(&queue, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty(),
        "a job at max_attempts is not returned to pending"
    );
    assert_eq!(
        repo.reap_exhausted(&queue, Duration::from_secs(60))
            .await
            .unwrap(),
        vec![id]
    );
    assert_eq!(status_of(&pool, &queue, id).await, "dead");

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn push_joins_the_callers_transaction() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());
    let domain_table = format!("{}_domain", queue.table_name());

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS {domain_table} (id BIGSERIAL PRIMARY KEY, note TEXT)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("INSERT INTO {domain_table} (note) VALUES ('rolled back')"))
        .execute(&mut *tx)
        .await
        .unwrap();
    repo.push(&mut *tx, &queue, &serde_json::json!("rolled back"))
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(repo.pending_count(&queue).await.unwrap(), 0);
    let domain_rows: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {domain_table}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(domain_rows, 0);

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("INSERT INTO {domain_table} (note) VALUES ('committed')"))
        .execute(&mut *tx)
        .await
        .unwrap();
    repo.push(&mut *tx, &queue, &serde_json::json!("committed"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(repo.pending_count(&queue).await.unwrap(), 1);
    let domain_rows: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {domain_table}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(domain_rows, 1);

    sqlx::query(&format!("DROP TABLE {domain_table}"))
        .execute(&pool)
        .await
        .unwrap();
    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn notify_fires_on_insert_and_on_return_to_pending() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());

    let mut listener = ListenerService::new(&pool).await.unwrap();
    listener.listen(&queue.channel_name()).await.unwrap();

    let id = repo
        .push(&pool, &queue, &serde_json::json!("notify"))
        .await
        .unwrap();
    let first = listener
        .recv_timeout(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("insert notification");
    assert_eq!(first.payload, id.to_string());

    let job = repo.pop::<serde_json::Value>(&queue).await.unwrap().unwrap();
    repo.fail(&pool, &queue, job.id, job.attempts).await.unwrap();
    let second = listener
        .recv_timeout(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("retry notification");
    assert_eq!(second.payload, id.to_string());

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn worker_handles_backlog_and_scheduled_jobs() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());

    let backlog = repo
        .push(&pool, &queue, &serde_json::json!("backlog"))
        .await
        .unwrap();
    let scheduled = repo
        .push_at(
            &pool,
            &queue,
            &serde_json::json!("scheduled"),
            chrono::Utc::now() + chrono::Duration::milliseconds(800),
        )
        .await
        .unwrap();

    let listener = ListenerService::new(&pool).await.unwrap();

    let handled = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&handled);
    let worker_queue = queue.clone();
    let worker_repo = repo.clone();
    let worker = tokio::spawn(async move {
        let mut listener = listener;
        run_worker::<serde_json::Value, _, _>(
            &worker_repo,
            &worker_queue,
            &mut listener,
            Duration::from_secs(60),
            Duration::from_millis(250),
            10,
            |job| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(job.id);
                    Ok(())
                }
            },
        )
        .await
    });

    let fast = repo
        .push(&pool, &queue, &serde_json::json!("fast path"))
        .await
        .unwrap();

    for _ in 0..40 {
        if handled.lock().unwrap().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    worker.abort();

    let mut ids = handled.lock().unwrap().clone();
    ids.sort_unstable();
    let mut expected = vec![backlog, scheduled, fast];
    expected.sort_unstable();
    assert_eq!(ids, expected);

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn worker_rejects_a_zero_sized_configuration() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());
    let mut listener = ListenerService::new(&pool).await.unwrap();

    let zero_lease = run_worker::<serde_json::Value, _, _>(
        &repo,
        &queue,
        &mut listener,
        Duration::ZERO,
        Duration::from_secs(1),
        1,
        |_| async { Ok(()) },
    )
    .await;
    assert!(zero_lease.is_err());

    let zero_poll = run_worker::<serde_json::Value, _, _>(
        &repo,
        &queue,
        &mut listener,
        Duration::from_secs(1),
        Duration::ZERO,
        1,
        |_| async { Ok(()) },
    )
    .await;
    assert!(zero_poll.is_err());

    let zero_cycle = run_worker::<serde_json::Value, _, _>(
        &repo,
        &queue,
        &mut listener,
        Duration::from_secs(1),
        Duration::from_secs(1),
        0,
        |_| async { Ok(()) },
    )
    .await;
    assert!(zero_cycle.is_err());

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn queue_names_are_length_limited_on_both_sides() {
    let Some(pool) = connect().await else { return };

    let longest = "q".repeat(MAX_QUEUE_NAME_LEN);
    let queue = QueueName::new(&longest).unwrap();
    create_queue(&pool, queue.name()).await.expect("max length is usable");

    let triggers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger WHERE tgrelid = $1::regclass AND NOT tgisinternal",
    )
    .bind(queue.table_name())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(triggers, 2, "both notify triggers are distinct at the limit");

    let indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes WHERE tablename = $1 AND indexname LIKE 'idx_queue_%'",
    )
    .bind(queue.table_name())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, 2, "both partial indexes are distinct at the limit");

    let functions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pg_proc WHERE proname = $1")
            .bind(format!("notify_queue_{longest}"))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(functions, 1);

    let too_long = "q".repeat(MAX_QUEUE_NAME_LEN + 1);
    assert!(QueueName::new(&too_long).is_err());
    assert!(
        create_queue(&pool, &too_long).await.is_err(),
        "the DDL must refuse a name the Rust API refuses"
    );

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn upgrade_leaves_unrelated_queue_tables_alone() {
    let Some(pool) = connect().await else { return };
    let table = format!("queue_unrelated_{}", std::process::id());

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (id BIGSERIAL PRIMARY KEY, note TEXT)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let _guard = DDL_LOCK.lock().await;
    sqlx::raw_sql(SETUP_SQL)
        .execute(&pool)
        .await
        .expect("setup.sql must survive an unrelated queue_* table");
    drop(_guard);

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = $1 ORDER BY column_name",
    )
    .bind(&table)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns, vec!["id".to_string(), "note".to_string()]);

    let triggers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger WHERE tgrelid = $1::regclass AND NOT tgisinternal",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(triggers, 0, "no notify triggers on a non-queue table");

    sqlx::query(&format!("DROP TABLE {table}"))
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn store_response_is_idempotent() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let repo = QueueRepository::new(pool.clone());
    let service = pg_queue::RequestResponseService::new(pool.clone(), repo);
    let request_id = uuid::Uuid::new_v4();

    service
        .store_response(&pool, &request_id, &serde_json::json!({"v": 1}))
        .await
        .unwrap();
    service
        .store_response(&pool, &request_id, &serde_json::json!({"v": 2}))
        .await
        .expect("a retried handler must be able to store its response twice");

    let value: serde_json::Value = sqlx::query_scalar(
        "SELECT response FROM request_responses WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(value, serde_json::json!({"v": 2}));

    sqlx::query("DELETE FROM request_responses WHERE request_id = $1")
        .bind(request_id)
        .execute(&pool)
        .await
        .unwrap();

    drop_queue(&pool, &queue).await;
}

#[tokio::test]
async fn create_queue_is_idempotent_and_upgrades_v1() {
    let Some(pool) = connect().await else { return };
    let queue = new_queue(&pool).await;
    let v1 = format!("{}_v1", queue.table_name());

    sqlx::query(&format!(
        "CREATE TABLE {v1} (
            id BIGSERIAL PRIMARY KEY,
            payload JSONB NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            processed_at TIMESTAMPTZ
        )"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!("INSERT INTO {v1} (payload) VALUES ('{{}}'::jsonb)"))
        .execute(&pool)
        .await
        .unwrap();

    let v1_name = v1.trim_start_matches("queue_").to_string();
    for _ in 0..2 {
        create_queue(&pool, &v1_name)
            .await
            .expect("setup.sql must be idempotent");
    }

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = $1 AND column_name IN ('deliver_at', 'delivery_attempts')",
    )
    .bind(&v1)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns.len(), 2, "v1 table gained both new columns");

    let backfilled: (i32, i64) = sqlx::query_as(&format!(
        "SELECT delivery_attempts, EXTRACT(EPOCH FROM (NOW() - deliver_at))::bigint FROM {v1}"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(backfilled, (0, 0));

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes WHERE tablename = $1 AND indexname LIKE 'idx_queue_%'",
    )
    .bind(&v1)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes.len(), 2, "ready and stale indexes, no old one");

    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT tgname FROM pg_trigger WHERE tgrelid = $1::regclass AND NOT tgisinternal",
    )
    .bind(&v1)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(triggers.len(), 2, "insert and retry triggers");

    drop_queue(&pool, &QueueName::new(v1_name).unwrap()).await;
    drop_queue(&pool, &queue).await;
}
