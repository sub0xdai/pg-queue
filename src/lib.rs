//! PostgreSQL-based job queue, pub/sub, cache, and request-response.
//!
//! Run `migrations/setup.sql`, then `SELECT pg_queue_create_queue('emails');`.

pub mod cache;
pub mod errors;
pub mod listen;
pub mod notify;
pub mod queue;
pub mod request_response;
pub mod worker;

pub use cache::CacheRepository;
pub use errors::{PgQueueError, Result};
pub use listen::{ListenerService, Notification};
pub use notify::NotifyService;
pub use queue::{
    Backoff, CompleteOutcome, FailOutcome, Job, JobStatus, QueueName, QueueRepository, RetryPolicy,
    MAX_QUEUE_NAME_LEN,
};
pub use request_response::{RequestResponseService, RequestWrapper};
pub use worker::run_worker;

pub use sqlx::PgPool;

#[derive(Clone)]
pub struct PgQueueManager {
    pub queue: QueueRepository,
    pub notify: NotifyService,
    pub cache: CacheRepository,
    pub request_response: RequestResponseService,
    pool: PgPool,
}

impl PgQueueManager {
    pub fn new(pool: PgPool) -> Self {
        let queue = QueueRepository::new(pool.clone());
        Self {
            notify: NotifyService::new(),
            cache: CacheRepository::new(pool.clone()),
            request_response: RequestResponseService::new(pool.clone(), queue.clone()),
            queue,
            pool,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

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
