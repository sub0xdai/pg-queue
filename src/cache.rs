use crate::errors::{PgQueueError, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{de::DeserializeOwned, Serialize};
use sqlx::PgPool;

#[derive(Clone)]
pub struct CacheRepository {
    pool: PgPool,
}

impl CacheRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let row: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT value FROM cache_entries WHERE key = $1 AND expires_at > NOW()")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some((value,)) => {
                let parsed: T = serde_json::from_value(value)?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    pub async fn set<T: Serialize>(&self, key: &str, value: &T, ttl_secs: u64) -> Result<()> {
        let ttl = i64::try_from(ttl_secs)
            .ok()
            .and_then(Duration::try_seconds)
            .ok_or(PgQueueError::DurationOutOfRange)?;
        let expires_at = Utc::now()
            .checked_add_signed(ttl)
            .ok_or(PgQueueError::DurationOutOfRange)?;
        self.set_with_expiry(key, value, expires_at).await
    }

    pub async fn set_with_expiry<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let json = serde_json::to_value(value)?;

        sqlx::query(
            r#"
            INSERT INTO cache_entries (key, value, expires_at)
            VALUES ($1, $2, $3)
            ON CONFLICT (key) DO UPDATE SET value = $2, expires_at = $3
            "#,
        )
        .bind(key)
        .bind(json)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM cache_entries WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn cleanup_expired(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM cache_entries WHERE expires_at <= NOW()")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    pub async fn get_or_set<T, F, Fut>(&self, key: &str, ttl_secs: u64, fetch: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if let Some(cached) = self.get::<T>(key).await? {
            return Ok(cached);
        }

        let value = fetch().await?;
        self.set(key, &value, ttl_secs).await?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_cache_key_format() {
        let key = format!("user:{}:profile", 42);
        assert_eq!(key, "user:42:profile");
    }
}
