use crate::errors::Result;
use sqlx::PgPool;

/// Service for sending PostgreSQL NOTIFY messages.
///
/// Note: PostgreSQL NOTIFY has an 8KB payload limit.
pub struct NotifyService {
    pool: PgPool,
}

impl NotifyService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Send a notification to a channel
    pub async fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(channel)
            .bind(payload)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_channel_formatting() {
        let channel = format!("events.{}", "user_123");
        assert_eq!(channel, "events.user_123");
    }
}
