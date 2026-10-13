use crate::errors::{PgQueueError, Result};
use crate::listen::ListenerService;
use crate::queue::{CompleteOutcome, FailOutcome, Job, QueueName, QueueRepository};
use serde::de::DeserializeOwned;
use std::future::Future;
use std::time::Duration;

/// `lease` must exceed the longest legitimate handler run, or a still-running job
/// is handed to a second worker.
pub async fn run_worker<T, F, Fut>(
    queue: &QueueRepository,
    name: &QueueName,
    listener: &mut ListenerService,
    lease: Duration,
    poll_interval: Duration,
    max_per_cycle: usize,
    mut handle: F,
) -> Result<()>
where
    T: DeserializeOwned,
    F: FnMut(Job<T>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if lease.is_zero() {
        return Err(PgQueueError::InvalidWorkerConfig("lease"));
    }
    if poll_interval.is_zero() {
        return Err(PgQueueError::InvalidWorkerConfig("poll_interval"));
    }
    if max_per_cycle == 0 {
        return Err(PgQueueError::InvalidWorkerConfig("max_per_cycle"));
    }

    listener.listen(&name.channel_name()).await?;

    let mut listener_healthy = true;
    loop {
        let reclaimed = queue.reap_stale(name, lease).await?;
        if !reclaimed.is_empty() {
            eprintln!(
                "pg-queue: reclaimed {} stale job(s) in {name}",
                reclaimed.len()
            );
        }
        let buried = queue.reap_exhausted(name, lease).await?;
        if !buried.is_empty() {
            eprintln!(
                "pg-queue: buried {} job(s) in {name} after repeated crashes",
                buried.len()
            );
        }

        for _ in 0..max_per_cycle {
            let job = match queue.pop::<T>(name).await {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(PgQueueError::Serialization(err)) => {
                    eprintln!("pg-queue: a job in {name} could not be decoded: {err}");
                    continue;
                }
                Err(err) => return Err(err),
            };

            let job_id = job.id;
            let attempts = job.attempts;

            match handle(job).await {
                Ok(()) => {
                    if let CompleteOutcome::StaleClaim =
                        queue.complete(queue.pool(), name, job_id).await?
                    {
                        eprintln!(
                            "pg-queue: job {job_id} in {name} finished after its \
                             claim was reclaimed"
                        );
                    }
                }
                Err(err) => {
                    eprintln!("pg-queue: job {job_id} from {name} failed: {err}");
                    match queue.fail(queue.pool(), name, job_id, attempts).await? {
                        FailOutcome::Dead { attempts } => eprintln!(
                            "pg-queue: job {job_id} from {name} is dead after {attempts} attempts"
                        ),
                        FailOutcome::StaleClaim => eprintln!(
                            "pg-queue: job {job_id} in {name} failed after its claim was reclaimed"
                        ),
                        FailOutcome::Retried { .. } => {}
                    }
                }
            }
        }

        match listener.recv_timeout(poll_interval).await {
            Ok(_) => listener_healthy = true,
            Err(err) => {
                if listener_healthy {
                    eprintln!(
                        "pg-queue: LISTEN on {} failed, polling until it recovers: {err}",
                        name.channel_name()
                    );
                    listener_healthy = false;
                }
            }
        }
    }
}
