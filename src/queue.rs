use crate::errors::{PgQueueError, Result};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use sqlx::{PgPool, Postgres};
use std::time::Duration;

/// PostgreSQL truncates identifiers at 63 bytes, so `queue_` + name +
/// `_notify_insert` (20 bytes) is the longest derived name: 43 characters.
pub const MAX_QUEUE_NAME_LEN: usize = 43;

/// # Example
/// ```
/// use pg_queue::QueueName;
///
/// let emails = QueueName::new("emails").unwrap();
/// assert_eq!(emails.table_name(), "queue_emails");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueName {
    name: String,
}

impl QueueName {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(PgQueueError::InvalidQueueName(name));
        }
        if name.len() > MAX_QUEUE_NAME_LEN {
            return Err(PgQueueError::QueueNameTooLong {
                len: name.len(),
                max: MAX_QUEUE_NAME_LEN,
            });
        }
        Ok(Self { name })
    }

    pub fn table_name(&self) -> String {
        format!("queue_{}", self.name)
    }

    pub fn channel_name(&self) -> String {
        self.table_name()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for QueueName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.table_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Processing,
    Completed,
    Dead,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Dead => "dead",
        }
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    Constant(Duration),
    Linear {
        base: Duration,
        step: Duration,
        max: Duration,
    },
    Exponential {
        base: Duration,
        factor: u32,
        max: Duration,
    },
}

impl Backoff {
    pub fn deliver_after(&self, attempt: u32) -> Duration {
        match self {
            Self::Constant(d) => *d,
            Self::Linear { base, step, max } => base
                .saturating_add(step.saturating_mul(attempt.saturating_sub(1)))
                .min(*max),
            Self::Exponential { base, factor, max } => base
                .saturating_mul(factor.saturating_pow(attempt.saturating_sub(1)))
                .min(*max),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff: Backoff,
}

impl RetryPolicy {
    /// At least one attempt, at most `i32::MAX` (the column is a signed integer).
    pub fn new(max_attempts: u32, backoff: Backoff) -> Result<Self> {
        if max_attempts == 0 || max_attempts > i32::MAX as u32 {
            return Err(PgQueueError::InvalidMaxAttempts {
                value: max_attempts,
                max: i32::MAX as u32,
            });
        }
        if let Backoff::Exponential { factor: 0, .. } = backoff {
            return Err(PgQueueError::InvalidBackoffFactor(0));
        }
        Ok(Self {
            max_attempts,
            backoff,
        })
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub fn backoff(&self) -> Backoff {
        self.backoff
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            backoff: Backoff::Exponential {
                base: Duration::from_secs(1),
                factor: 2,
                max: Duration::from_secs(300),
            },
        }
    }
}

/// What `fail` did, or `StaleClaim` when the row was no longer this caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    Retried {
        attempts: u32,
        deliver_at: DateTime<Utc>,
    },
    Dead {
        attempts: u32,
    },
    StaleClaim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    Completed,
    StaleClaim,
}

#[derive(Debug)]
pub struct Job<T> {
    pub id: i64,
    pub payload: T,
    pub attempts: u32,
}

const REAP_BATCH: i64 = 1000;

#[derive(Clone)]
pub struct QueueRepository {
    pool: PgPool,
    retry: RetryPolicy,
}

impl QueueRepository {
    pub fn new(pool: PgPool) -> Self {
        Self::with_retry_policy(pool, RetryPolicy::default())
    }

    pub fn with_retry_policy(pool: PgPool, retry: RetryPolicy) -> Self {
        Self { pool, retry }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// `deliver_at` comes from the database clock, not the caller's.
    pub async fn push<'e, E, T>(
        &self,
        executor: E,
        queue: &QueueName,
        payload: &T,
    ) -> Result<i64>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
        T: Serialize,
    {
        self.insert(executor, queue, payload, None).await
    }

    pub async fn push_at<'e, E, T>(
        &self,
        executor: E,
        queue: &QueueName,
        payload: &T,
        deliver_at: DateTime<Utc>,
    ) -> Result<i64>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
        T: Serialize,
    {
        self.insert(executor, queue, payload, Some(deliver_at)).await
    }

    async fn insert<'e, E, T>(
        &self,
        executor: E,
        queue: &QueueName,
        payload: &T,
        deliver_at: Option<DateTime<Utc>>,
    ) -> Result<i64>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
        T: Serialize,
    {
        let json = serde_json::to_value(payload)?;

        let row: (i64,) = sqlx::query_as(&format!(
            "INSERT INTO {} (payload, deliver_at) VALUES ($1, COALESCE($2, NOW())) RETURNING id",
            queue.table_name()
        ))
        .bind(json)
        .bind(deliver_at)
        .fetch_one(executor)
        .await?;

        Ok(row.0)
    }

    /// An undecodable payload is released through the retry policy, not stranded.
    pub async fn pop<T: DeserializeOwned>(&self, queue: &QueueName) -> Result<Option<Job<T>>> {
        let table = queue.table_name();

        let row: Option<(i64, serde_json::Value, i32)> = sqlx::query_as(&format!(
            r#"
            UPDATE {table}
            SET status = '{processing}',
                processed_at = NOW(),
                delivery_attempts = delivery_attempts + 1
            WHERE id = (
                SELECT id FROM {table}
                WHERE status = '{pending}' AND deliver_at <= NOW()
                ORDER BY deliver_at, created_at
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING id, payload, delivery_attempts
            "#,
            table = table,
            processing = JobStatus::Processing,
            pending = JobStatus::Pending,
        ))
        .fetch_optional(&self.pool)
        .await?;

        let Some((id, payload, attempts)) = row else {
            return Ok(None);
        };

        let attempts = attempts as u32;
        match serde_json::from_value(payload) {
            Ok(payload) => Ok(Some(Job {
                id,
                payload,
                attempts,
            })),
            Err(err) => {
                self.fail(&self.pool, queue, id, attempts).await?;
                Err(err.into())
            }
        }
    }

    pub async fn complete<'e, E>(
        &self,
        executor: E,
        queue: &QueueName,
        job_id: i64,
    ) -> Result<CompleteOutcome>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        let result = sqlx::query(&format!(
            "UPDATE {} SET status = '{}' WHERE id = $1 AND status = '{}'",
            queue.table_name(),
            JobStatus::Completed,
            JobStatus::Processing,
        ))
        .bind(job_id)
        .execute(executor)
        .await?;

        if result.rows_affected() == 1 {
            Ok(CompleteOutcome::Completed)
        } else {
            Ok(CompleteOutcome::StaleClaim)
        }
    }

    /// `attempts` must be the claim's own count (`Job::attempts`); a mismatch
    /// returns `StaleClaim` instead of writing.
    pub async fn fail<'e, E>(
        &self,
        executor: E,
        queue: &QueueName,
        job_id: i64,
        attempts: u32,
    ) -> Result<FailOutcome>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        let delay = self.retry.backoff.deliver_after(attempts).as_secs_f64();

        let row: Option<(String, DateTime<Utc>, i32)> = sqlx::query_as(&format!(
            r#"
            UPDATE {table}
            SET status = CASE WHEN delivery_attempts >= $3 THEN '{dead}' ELSE '{pending}' END,
                processed_at = NULL,
                deliver_at = CASE WHEN delivery_attempts >= $3 THEN deliver_at
                                  ELSE NOW() + make_interval(secs => $2) END
            WHERE id = $1
              AND status = '{processing}'
              AND delivery_attempts = $4
            RETURNING status, deliver_at, delivery_attempts
            "#,
            table = queue.table_name(),
            dead = JobStatus::Dead,
            pending = JobStatus::Pending,
            processing = JobStatus::Processing,
        ))
        .bind(job_id)
        .bind(delay)
        .bind(self.retry.max_attempts as i32)
        .bind(attempts as i32)
        .fetch_optional(executor)
        .await?;

        let Some((status, deliver_at, attempts)) = row else {
            return Ok(FailOutcome::StaleClaim);
        };

        match status.as_str() {
            "dead" => Ok(FailOutcome::Dead {
                attempts: attempts as u32,
            }),
            "pending" => Ok(FailOutcome::Retried {
                attempts: attempts as u32,
                deliver_at,
            }),
            other => Err(PgQueueError::UnexpectedStatus(other.to_string())),
        }
    }

    pub async fn reap_stale(&self, queue: &QueueName, lease: Duration) -> Result<Vec<i64>> {
        let table = queue.table_name();

        let rows: Vec<(i64,)> = sqlx::query_as(&format!(
            r#"
            UPDATE {table}
            SET status = '{pending}', processed_at = NULL
            WHERE id IN (
                SELECT id FROM {table}
                WHERE status = '{processing}'
                  AND processed_at < NOW() - make_interval(secs => $1)
                  AND delivery_attempts < $2
                ORDER BY processed_at
                LIMIT {batch}
            )
            RETURNING id
            "#,
            table = table,
            pending = JobStatus::Pending,
            processing = JobStatus::Processing,
            batch = REAP_BATCH,
        ))
        .bind(lease.as_secs() as f64)
        .bind(self.retry.max_attempts as i32)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    pub async fn reap_exhausted(&self, queue: &QueueName, lease: Duration) -> Result<Vec<i64>> {
        let table = queue.table_name();

        let rows: Vec<(i64,)> = sqlx::query_as(&format!(
            r#"
            UPDATE {table}
            SET status = '{dead}'
            WHERE id IN (
                SELECT id FROM {table}
                WHERE status = '{processing}'
                  AND processed_at < NOW() - make_interval(secs => $1)
                  AND delivery_attempts >= $2
                ORDER BY processed_at
                LIMIT {batch}
            )
            RETURNING id
            "#,
            table = table,
            dead = JobStatus::Dead,
            processing = JobStatus::Processing,
            batch = REAP_BATCH,
        ))
        .bind(lease.as_secs() as f64)
        .bind(self.retry.max_attempts as i32)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Includes jobs whose `deliver_at` is still in the future.
    pub async fn pending_count(&self, queue: &QueueName) -> Result<i64> {
        self.count_status(queue, JobStatus::Pending).await
    }

    pub async fn dead_count(&self, queue: &QueueName) -> Result<i64> {
        self.count_status(queue, JobStatus::Dead).await
    }

    async fn count_status(&self, queue: &QueueName, status: JobStatus) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {} WHERE status = '{}'",
            queue.table_name(),
            status,
        ))
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_name_valid() {
        let q = QueueName::new("orders").unwrap();
        assert_eq!(q.table_name(), "queue_orders");
        assert_eq!(q.name(), "orders");
    }

    #[test]
    fn test_queue_name_rejects_empty() {
        assert!(QueueName::new("").is_err());
    }

    #[test]
    fn test_queue_name_rejects_sql_injection() {
        assert!(QueueName::new("x; DROP TABLE users; --").is_err());
        assert!(QueueName::new("name with spaces").is_err());
        assert!(QueueName::new("bad'name").is_err());
    }

    #[test]
    fn test_queue_name_allows_underscores() {
        let q = QueueName::new("my_queue_123").unwrap();
        assert_eq!(q.table_name(), "queue_my_queue_123");
    }

    #[test]
    fn test_queue_name_channel() {
        let q = QueueName::new("emails").unwrap();
        assert_eq!(q.channel_name(), "queue_emails");
        assert_eq!(q.channel_name(), q.table_name());
    }

    #[test]
    fn test_queue_name_display() {
        let q = QueueName::new("tasks").unwrap();
        assert_eq!(format!("{}", q), "queue_tasks");
    }

    #[test]
    fn test_queue_name_equality() {
        let a = QueueName::new("jobs").unwrap();
        let b = QueueName::new("jobs").unwrap();
        let c = QueueName::new("other").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_job_status_as_str() {
        assert_eq!(JobStatus::Pending.as_str(), "pending");
        assert_eq!(JobStatus::Processing.as_str(), "processing");
        assert_eq!(JobStatus::Completed.as_str(), "completed");
        assert_eq!(JobStatus::Dead.as_str(), "dead");
    }

    #[test]
    fn test_job_status_display() {
        assert_eq!(format!("{}", JobStatus::Pending), "pending");
    }

    #[test]
    fn test_backoff_constant() {
        let b = Backoff::Constant(Duration::from_secs(7));
        assert_eq!(b.deliver_after(1), Duration::from_secs(7));
        assert_eq!(b.deliver_after(9), Duration::from_secs(7));
    }

    #[test]
    fn test_backoff_linear() {
        let b = Backoff::Linear {
            base: Duration::from_secs(10),
            step: Duration::from_secs(5),
            max: Duration::from_secs(20),
        };
        assert_eq!(b.deliver_after(1), Duration::from_secs(10));
        assert_eq!(b.deliver_after(2), Duration::from_secs(15));
        assert_eq!(b.deliver_after(3), Duration::from_secs(20));
        assert_eq!(b.deliver_after(99), Duration::from_secs(20));
    }

    #[test]
    fn test_backoff_exponential() {
        let b = Backoff::Exponential {
            base: Duration::from_secs(1),
            factor: 2,
            max: Duration::from_secs(300),
        };
        assert_eq!(b.deliver_after(1), Duration::from_secs(1));
        assert_eq!(b.deliver_after(2), Duration::from_secs(2));
        assert_eq!(b.deliver_after(3), Duration::from_secs(4));
        assert_eq!(b.deliver_after(11), Duration::from_secs(300));
    }

    #[test]
    fn test_backoff_exponential_factor_one_is_constant() {
        let b = Backoff::Exponential {
            base: Duration::from_secs(3),
            factor: 1,
            max: Duration::from_secs(300),
        };
        assert_eq!(b.deliver_after(1), Duration::from_secs(3));
        assert_eq!(b.deliver_after(9), Duration::from_secs(3));
    }

    #[test]
    fn test_backoff_exponential_saturates() {
        let b = Backoff::Exponential {
            base: Duration::from_secs(u64::MAX),
            factor: u32::MAX,
            max: Duration::from_secs(u64::MAX),
        };
        assert_eq!(b.deliver_after(u32::MAX).as_secs(), u64::MAX);
    }

    #[test]
    fn test_retry_policy_default() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts(), 5);
        assert_eq!(p.backoff().deliver_after(1), Duration::from_secs(1));
        assert_eq!(p.backoff().deliver_after(2), Duration::from_secs(2));
    }

    #[test]
    fn test_retry_policy_rejects_unusable_max_attempts() {
        let backoff = Backoff::Constant(Duration::from_secs(1));
        assert!(RetryPolicy::new(0, backoff).is_err());
        assert!(RetryPolicy::new(i32::MAX as u32 + 1, backoff).is_err());
        assert!(RetryPolicy::new(u32::MAX, backoff).is_err());
        assert_eq!(
            RetryPolicy::new(i32::MAX as u32, backoff).unwrap().max_attempts(),
            i32::MAX as u32
        );
    }

    #[test]
    fn test_retry_policy_rejects_a_zero_backoff_factor() {
        let zero = Backoff::Exponential {
            base: Duration::from_secs(1),
            factor: 0,
            max: Duration::from_secs(60),
        };
        assert!(matches!(
            RetryPolicy::new(3, zero),
            Err(PgQueueError::InvalidBackoffFactor(0))
        ));
    }

    #[test]
    fn test_queue_name_length_limit() {
        let longest = "a".repeat(MAX_QUEUE_NAME_LEN);
        assert_eq!(QueueName::new(&longest).unwrap().name().len(), 43);

        let too_long = "a".repeat(MAX_QUEUE_NAME_LEN + 1);
        assert!(matches!(
            QueueName::new(&too_long),
            Err(PgQueueError::QueueNameTooLong { len: 44, max: 43 })
        ));
    }

    #[test]
    fn test_writes_accept_a_transaction() {
        async fn in_a_transaction(pool: &PgPool, q: &QueueName) -> Result<()> {
            let mut tx = pool.begin().await?;
            let repo = QueueRepository::new(pool.clone());
            let id = repo.push(&mut *tx, q, &serde_json::json!({})).await?;
            let complete: CompleteOutcome = repo.complete(&mut *tx, q, id).await?;
            let fail: FailOutcome = repo.fail(&mut *tx, q, id, 1).await?;
            assert!(matches!(complete, CompleteOutcome::Completed | CompleteOutcome::StaleClaim));
            assert!(matches!(
                fail,
                FailOutcome::Retried { .. } | FailOutcome::Dead { .. } | FailOutcome::StaleClaim
            ));
            tx.commit().await?;
            Ok(())
        }

        let _ = in_a_transaction;
    }

    #[test]
    fn test_writes_accept_a_pool() {
        async fn with_a_pool(pool: &PgPool, q: &QueueName) -> Result<()> {
            let repo = QueueRepository::new(pool.clone());
            let id = repo.push(pool, q, &serde_json::json!({})).await?;
            repo.complete(pool, q, id).await?;
            repo.fail(pool, q, id, 1).await?;
            Ok(())
        }

        let _ = with_a_pool;
    }
}
