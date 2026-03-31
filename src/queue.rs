use crate::errors::Result;
use serde::{de::DeserializeOwned, Serialize};
use sqlx::PgPool;

/// A named queue backed by a PostgreSQL table.
///
/// Each `QueueName` maps to a table called `queue_{name}`.
/// Create the table using the `pg_queue_create_queue()` SQL function
/// from `migrations/setup.sql`.
///
/// # Example
/// ```
/// use pg_queue::QueueName;
///
/// let emails = QueueName::new("emails");
/// assert_eq!(emails.table_name(), "queue_emails");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueName {
    name: String,
}

impl QueueName {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Returns the backing table name: `queue_{name}`
    pub fn table_name(&self) -> String {
        format!("queue_{}", self.name)
    }

    /// Returns the NOTIFY channel name (same as table name by convention)
    pub fn channel_name(&self) -> String {
        format!("queue_{}", self.name)
    }

    /// Returns the raw queue name
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for QueueName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.table_name())
    }
}

/// A job retrieved from the queue
#[derive(Debug)]
pub struct Job<T> {
    pub id: i64,
    pub payload: T,
}

/// Queue repository for push/pop operations using SKIP LOCKED
pub struct QueueRepository {
    pool: PgPool,
}

impl QueueRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Push a job to the queue
    pub async fn push<T: Serialize>(&self, queue: &QueueName, payload: &T) -> Result<i64> {
        let json = serde_json::to_value(payload)?;

        let row: (i64,) = sqlx::query_as(&format!(
            "INSERT INTO {} (payload) VALUES ($1) RETURNING id",
            queue.table_name()
        ))
        .bind(json)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Pop a job from the queue using SKIP LOCKED for concurrent safety.
    /// Returns None if no pending jobs are available.
    pub async fn pop<T: DeserializeOwned>(&self, queue: &QueueName) -> Result<Option<Job<T>>> {
        let table = queue.table_name();

        let row: Option<(i64, serde_json::Value)> = sqlx::query_as(&format!(
            r#"
            UPDATE {table} SET status = 'processing', processed_at = NOW()
            WHERE id = (
                SELECT id FROM {table} WHERE status = 'pending'
                ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1
            )
            RETURNING id, payload
            "#
        ))
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((id, payload)) => {
                let parsed: T = serde_json::from_value(payload)?;
                Ok(Some(Job {
                    id,
                    payload: parsed,
                }))
            }
            None => Ok(None),
        }
    }

    /// Mark a job as completed
    pub async fn complete(&self, queue: &QueueName, job_id: i64) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {} SET status = 'completed' WHERE id = $1",
            queue.table_name()
        ))
        .bind(job_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Mark a job as failed, resetting it to pending for retry
    pub async fn fail(&self, queue: &QueueName, job_id: i64) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {} SET status = 'pending', processed_at = NULL WHERE id = $1",
            queue.table_name()
        ))
        .bind(job_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get the count of pending jobs in a queue
    pub async fn pending_count(&self, queue: &QueueName) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {} WHERE status = 'pending'",
            queue.table_name()
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
    fn test_queue_name_table() {
        let q = QueueName::new("orders");
        assert_eq!(q.table_name(), "queue_orders");
        assert_eq!(q.name(), "orders");
    }

    #[test]
    fn test_queue_name_channel() {
        let q = QueueName::new("emails");
        assert_eq!(q.channel_name(), "queue_emails");
    }

    #[test]
    fn test_queue_name_display() {
        let q = QueueName::new("tasks");
        assert_eq!(format!("{}", q), "queue_tasks");
    }

    #[test]
    fn test_queue_name_equality() {
        let a = QueueName::new("jobs");
        let b = QueueName::new("jobs");
        let c = QueueName::new("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
