//! # pg-queue
//!
//! PostgreSQL-based job queue, pub/sub, and cache — a Redis replacement.
//!
//! - **Queues**: SKIP LOCKED for concurrent job processing
//! - **Pub/Sub**: LISTEN/NOTIFY for real-time messaging
//! - **Cache**: UNLOGGED tables with per-query TTL checks
//! - **Request-Response**: RPC over PostgreSQL with correlation IDs
//!
//! ## Setup
//!
//! Run `migrations/setup.sql` against your database, then create queues:
//!
//! ```sql
//! SELECT pg_queue_create_queue('emails');
//! SELECT pg_queue_create_queue('jobs');
//! ```
//!
//! ## Usage
//!
//! ```rust,no_run
//! use pg_queue::{PgQueueManager, QueueName};
//!
//! # async fn example() -> pg_queue::Result<()> {
//! let pool = sqlx::PgPool::connect("postgres://localhost/mydb").await.unwrap();
//! let mgr = PgQueueManager::new(pool);
//!
//! let emails = QueueName::new("emails")?;
//!
//! // Push a job
//! mgr.queue.push(&emails, &serde_json::json!({"to": "user@example.com"})).await?;
//!
//! // Pop and process
//! if let Some(job) = mgr.queue.pop::<serde_json::Value>(&emails).await? {
//!     println!("Processing job {}: {:?}", job.id, job.payload);
//!     mgr.queue.complete(&emails, job.id).await?;
//! }
//! # Ok(())
//! # }
//! ```

pub mod cache;
pub mod errors;
pub mod listen;
pub mod notify;
pub mod queue;
pub mod request_response;

pub use cache::CacheRepository;
pub use errors::{PgQueueError, Result};
pub use listen::{ListenerService, Notification};
pub use notify::NotifyService;
pub use queue::{Job, JobStatus, QueueName, QueueRepository};
pub use request_response::{RequestResponseService, RequestWrapper};

pub use sqlx::PgPool;

/// Main manager combining all PostgreSQL queue/pubsub/cache functionality
#[derive(Clone)]
pub struct PgQueueManager {
    pub queue: QueueRepository,
    pub notify: NotifyService,
    pub cache: CacheRepository,
    pub request_response: RequestResponseService,
    pool: PgPool,
}

impl PgQueueManager {
    /// Create a new PgQueueManager with the given connection pool
    pub fn new(pool: PgPool) -> Self {
        let queue = QueueRepository::new(pool.clone());
        Self {
            notify: NotifyService::new(pool.clone()),
            cache: CacheRepository::new(pool.clone()),
            request_response: RequestResponseService::new(pool.clone(), queue.clone()),
            queue,
            pool,
        }
    }

    /// Get a reference to the connection pool
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a new listener service for pub/sub
    pub async fn create_listener(&self) -> Result<ListenerService> {
        ListenerService::new(&self.pool).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_name_construction() {
        let q = QueueName::new("orders").unwrap();
        assert_eq!(q.table_name(), "queue_orders");
        assert_eq!(q.channel_name(), "queue_orders");
    }
}
