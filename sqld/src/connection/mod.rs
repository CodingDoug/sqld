use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::Future;
use tokio::{sync::Semaphore, time::timeout};

use crate::auth::Authenticated;
use crate::error::Error;
use crate::query::{Params, Query};
use crate::query_analysis::{State, Statement};
use crate::query_result_builder::{IgnoreResult, QueryResultBuilder};
use crate::Result;

use self::program::{Cond, DescribeResult, Program, Step};

pub mod config;
pub mod dump;
pub mod libsql;
pub mod program;
pub mod write_proxy;

const TXN_TIMEOUT: Duration = Duration::from_secs(5);

#[async_trait::async_trait]
pub trait Connection: Send + Sync + 'static {
    /// Executes a query program
    async fn execute_program<B: QueryResultBuilder>(
        &self,
        pgm: Program,
        auth: Authenticated,
        reponse_builder: B,
    ) -> Result<(B, State)>;

    /// Execute all the queries in the batch sequentially.
    /// If an query in the batch fails, the remaining queries are ignores, and the batch current
    /// transaction (if any) is rolledback.
    async fn execute_batch_or_rollback<B: QueryResultBuilder>(
        &self,
        batch: Vec<Query>,
        auth: Authenticated,
        result_builder: B,
    ) -> Result<(B, State)> {
        let batch_len = batch.len();
        let mut steps = make_batch_program(batch);

        if !steps.is_empty() {
            // We add a conditional rollback step if the last step was not sucessful.
            steps.push(Step {
                query: Query {
                    stmt: Statement::parse("ROLLBACK").next().unwrap().unwrap(),
                    params: Params::empty(),
                    want_rows: false,
                },
                cond: Some(Cond::Not {
                    cond: Box::new(Cond::Ok {
                        step: steps.len() - 1,
                    }),
                }),
            })
        }

        let pgm = Program::new(steps);

        // ignore the rollback result
        let builder = result_builder.take(batch_len);
        let (builder, state) = self.execute_program(pgm, auth, builder).await?;

        Ok((builder.into_inner(), state))
    }

    /// Execute all the queries in the batch sequentially.
    /// If an query in the batch fails, the remaining queries are ignored
    async fn execute_batch<B: QueryResultBuilder>(
        &self,
        batch: Vec<Query>,
        auth: Authenticated,
        result_builder: B,
    ) -> Result<(B, State)> {
        let steps = make_batch_program(batch);
        let pgm = Program::new(steps);
        self.execute_program(pgm, auth, result_builder).await
    }

    async fn rollback(&self, auth: Authenticated) -> Result<()> {
        self.execute_batch(
            vec![Query {
                stmt: Statement::parse("ROLLBACK").next().unwrap().unwrap(),
                params: Params::empty(),
                want_rows: false,
            }],
            auth,
            IgnoreResult,
        )
        .await?;

        Ok(())
    }

    /// Parse the SQL statement and return information about it.
    async fn describe(&self, sql: String, auth: Authenticated) -> Result<DescribeResult>;

    /// Check whether the connection is in autocommit mode.
    async fn is_autocommit(&self) -> Result<bool>;

    /// Calls for database checkpoint (if supported).
    async fn checkpoint(&self) -> Result<()>;
}

fn make_batch_program(batch: Vec<Query>) -> Vec<Step> {
    let mut steps = Vec::with_capacity(batch.len());
    for (i, query) in batch.into_iter().enumerate() {
        let cond = if i > 0 {
            // only execute if the previous step was a success
            Some(Cond::Ok { step: i - 1 })
        } else {
            None
        };

        let step = Step { cond, query };
        steps.push(step);
    }
    steps
}

#[async_trait::async_trait]
pub trait MakeConnection: Send + Sync + 'static {
    type Connection: Connection;

    /// Create a new connection of type Self::Connection
    async fn create(&self) -> Result<Self::Connection, Error>;

    fn throttled(
        self,
        conccurency: usize,
        timeout: Option<Duration>,
        max_total_response_size: u64,
    ) -> MakeThrottledConnection<Self>
    where
        Self: Sized,
    {
        MakeThrottledConnection::new(conccurency, self, timeout, max_total_response_size)
    }
}

#[async_trait::async_trait]
impl<F, C, Fut> MakeConnection for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<C, Error>> + Send,
    C: Connection + Sync + Send + 'static,
{
    type Connection = C;

    async fn create(&self) -> Result<Self::Connection, Error> {
        let db = (self)().await?;
        Ok(db)
    }
}

pub struct MakeThrottledConnection<F> {
    semaphore: Arc<Semaphore>,
    connection_maker: F,
    timeout: Option<Duration>,
    // Max memory available for responses. High memory pressure
    // will result in reducing concurrency to prevent out-of-memory errors.
    max_total_response_size: u64,
    waiters: AtomicUsize,
}

impl<F> MakeThrottledConnection<F> {
    fn new(
        conccurency: usize,
        connection_maker: F,
        timeout: Option<Duration>,
        max_total_response_size: u64,
    ) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(conccurency)),
            connection_maker,
            timeout,
            max_total_response_size,
            waiters: AtomicUsize::new(0),
        }
    }

    // How many units should be acquired from the semaphore,
    // depending on current memory pressure.
    fn units_to_take(&self) -> u32 {
        let total_response_size = crate::query_result_builder::TOTAL_RESPONSE_SIZE
            .load(std::sync::atomic::Ordering::Relaxed) as u64;
        if total_response_size * 2 > self.max_total_response_size {
            tracing::trace!("High memory pressure, reducing concurrency");
            16
        } else if total_response_size * 4 > self.max_total_response_size {
            tracing::trace!("Medium memory pressure, reducing concurrency");
            4
        } else {
            1
        }
    }
}

struct WaitersGuard<'a> {
    pub waiters: &'a AtomicUsize,
}

impl<'a> WaitersGuard<'a> {
    fn new(waiters: &'a AtomicUsize) -> Self {
        waiters.fetch_add(1, Ordering::Relaxed);
        Self { waiters }
    }
}

impl Drop for WaitersGuard<'_> {
    fn drop(&mut self) {
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl<F: MakeConnection> MakeConnection for MakeThrottledConnection<F> {
    type Connection = TrackedConnection<F::Connection>;

    async fn create(&self) -> Result<Self::Connection, Error> {
        // If the memory pressure is high, request more units to reduce concurrency.
        tracing::trace!(
            "Available semaphore units: {}",
            self.semaphore.available_permits()
        );
        let units = self.units_to_take();
        let waiters_guard = WaitersGuard::new(&self.waiters);
        if waiters_guard.waiters.load(Ordering::Relaxed) >= 128 {
            return Err(Error::TooManyRequests);
        }
        let fut = self.semaphore.clone().acquire_many_owned(units);
        let mut permit = match self.timeout {
            Some(t) => timeout(t, fut).await.map_err(|_| Error::DbCreateTimeout)?,
            None => fut.await,
        }
        .expect("semaphore closed");

        let units = self.units_to_take();
        if units > 1 {
            tracing::debug!("Reacquiring {units} units due to high memory pressure");
            let fut = self.semaphore.clone().acquire_many_owned(units);
            let mem_permit = match self.timeout {
                Some(t) => timeout(t, fut).await.map_err(|_| Error::DbCreateTimeout)?,
                None => fut.await,
            }
            .expect("semaphore closed");
            permit.merge(mem_permit);
        }

        let inner = self.connection_maker.create().await?;
        Ok(TrackedConnection { permit, inner })
    }
}

pub struct TrackedConnection<DB> {
    inner: DB,
    #[allow(dead_code)] // just hold on to it
    permit: tokio::sync::OwnedSemaphorePermit,
}

#[async_trait::async_trait]
impl<DB: Connection> Connection for TrackedConnection<DB> {
    #[inline]
    async fn execute_program<B: QueryResultBuilder>(
        &self,
        pgm: Program,
        auth: Authenticated,
        builder: B,
    ) -> crate::Result<(B, State)> {
        self.inner.execute_program(pgm, auth, builder).await
    }

    #[inline]
    async fn describe(&self, sql: String, auth: Authenticated) -> crate::Result<DescribeResult> {
        self.inner.describe(sql, auth).await
    }

    #[inline]
    async fn is_autocommit(&self) -> crate::Result<bool> {
        self.inner.is_autocommit().await
    }

    #[inline]
    async fn checkpoint(&self) -> Result<()> {
        self.inner.checkpoint().await
    }
}

#[cfg(test)]
mod test {
    use super::*;

    struct DummyDb;

    #[async_trait::async_trait]
    impl Connection for DummyDb {
        async fn execute_program<B: QueryResultBuilder>(
            &self,
            _pgm: Program,
            _auth: Authenticated,
            _builder: B,
        ) -> crate::Result<(B, State)> {
            unreachable!()
        }

        async fn describe(
            &self,
            _sql: String,
            _auth: Authenticated,
        ) -> crate::Result<DescribeResult> {
            unreachable!()
        }

        async fn is_autocommit(&self) -> crate::Result<bool> {
            unreachable!()
        }

        async fn checkpoint(&self) -> Result<()> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn throttle_db_creation() {
        let factory =
            (|| async { Ok(DummyDb) }).throttled(10, Some(Duration::from_millis(100)), u64::MAX);

        let mut conns = Vec::with_capacity(10);
        for _ in 0..10 {
            conns.push(factory.create().await.unwrap())
        }

        assert!(factory.create().await.is_err());

        drop(conns);

        assert!(factory.create().await.is_ok());
    }
}
