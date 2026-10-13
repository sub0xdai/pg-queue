use thiserror::Error;

#[derive(Debug, Error)]
pub enum PgQueueError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Queue not found: {0}")]
    QueueNotFound(String),

    #[error("Listener error: {0}")]
    Listener(String),

    #[error("Timeout waiting for response")]
    Timeout,

    #[error("Invalid queue name: {0} (must be alphanumeric/underscore, non-empty)")]
    InvalidQueueName(String),

    #[error("Queue name is {len} characters, maximum is {max}")]
    QueueNameTooLong { len: usize, max: usize },

    #[error("max_attempts must be between 1 and {max}, got {value}")]
    InvalidMaxAttempts { value: u32, max: u32 },

    #[error("exponential backoff factor must be at least 1, got {0}")]
    InvalidBackoffFactor(u32),

    #[error("{0} must be greater than zero")]
    InvalidWorkerConfig(&'static str),

    #[error("Job ended in unexpected status: {0}")]
    UnexpectedStatus(String),

    #[error("Retry delay does not fit a chrono Duration")]
    DurationOutOfRange,
}

pub type Result<T> = std::result::Result<T, PgQueueError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = PgQueueError::QueueNotFound("orders".to_string());
        assert_eq!(err.to_string(), "Queue not found: orders");

        let err = PgQueueError::Timeout;
        assert_eq!(err.to_string(), "Timeout waiting for response");

        let err = PgQueueError::InvalidQueueName("bad;name".to_string());
        assert!(err.to_string().contains("bad;name"));
    }

    #[test]
    fn test_error_from_serde() {
        let json_err = serde_json::from_str::<String>("invalid").unwrap_err();
        let err: PgQueueError = json_err.into();
        assert!(matches!(err, PgQueueError::Serialization(_)));
    }
}
