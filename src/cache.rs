use crate::errors::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{de::DeserializeOwned, Serialize};
use sqlx::PgPool;

/// Cache repository using PostgreSQL UNLOGGED table with per-query TTL check
#[derive(Clone)]
pub struct CacheRepository {
    pool: PgPool,
}

impl CacheRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get a cached value, returning None if not found or expired
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

    /// Set a cached value with TTL in seconds
    pub async fn set<T: Serialize>(&self, key: &str, value: &T, ttl_secs: u64) -> Result<()> {
        let json = serde_json::to_value(value)?;
        let expires_at = Utc::now() + Duration::seconds(ttl_secs as i64);

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

    /// Set a cached value with explicit expiration time
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

    /// Delete a cached entry
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM cache_entries WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Delete all expired entries (cleanup)
    pub async fn cleanup_expired(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM cache_entries WHERE expires_at <= NOW()")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    /// Get or set a cached value using a fallback function
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
