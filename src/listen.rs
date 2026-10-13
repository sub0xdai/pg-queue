use crate::errors::{PgQueueError, Result};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use std::time::Duration;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct Notification {
    pub channel: String,
    pub payload: String,
}

pub struct ListenerService {
    listener: PgListener,
}

impl ListenerService {
    pub async fn new(pool: &PgPool) -> Result<Self> {
        let listener = PgListener::connect_with(pool).await?;
        Ok(Self { listener })
    }

    pub async fn listen(&mut self, channel: &str) -> Result<()> {
        self.listener.listen(channel).await?;
        Ok(())
    }

    pub async fn listen_all(&mut self, channels: &[&str]) -> Result<()> {
        self.listener.listen_all(channels.iter().copied()).await?;
        Ok(())
    }

    pub async fn unlisten(&mut self, channel: &str) -> Result<()> {
        self.listener.unlisten(channel).await?;
        Ok(())
    }

    pub async fn unlisten_all(&mut self) -> Result<()> {
        self.listener.unlisten_all().await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Notification> {
        let notification = self.listener.recv().await?;
        Ok(Notification {
            channel: notification.channel().to_string(),
            payload: notification.payload().to_string(),
        })
    }

    pub async fn recv_timeout(&mut self, duration: Duration) -> Result<Option<Notification>> {
        match timeout(duration, self.listener.recv()).await {
            Ok(Ok(notification)) => Ok(Some(Notification {
                channel: notification.channel().to_string(),
                payload: notification.payload().to_string(),
            })),
            Ok(Err(e)) => Err(PgQueueError::Database(e)),
            Err(_) => Ok(None),
        }
    }

    pub async fn try_recv(&mut self) -> Result<Option<Notification>> {
        self.recv_timeout(Duration::from_millis(0)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notification_struct() {
        let notification = Notification {
            channel: "my_channel".to_string(),
            payload: r#"{"key": "value"}"#.to_string(),
        };

        assert_eq!(notification.channel, "my_channel");
        assert_eq!(notification.payload, r#"{"key": "value"}"#);
    }
}
