use crate::errors::Result;
use sqlx::Postgres;

#[derive(Clone, Copy, Default)]
pub struct NotifyService;

impl NotifyService {
    pub fn new() -> Self {
        Self
    }

    pub async fn notify<'e, E>(&self, executor: E, channel: &str, payload: &str) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(channel)
            .bind(payload)
            .execute(executor)
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
