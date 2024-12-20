use std::collections::VecDeque;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::{Arc, Weak};

use dashmap::DashMap;
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::RwLock;
use rand::Rng;
use tokio::net::TcpStream;
use tracing::{debug, error, info, info_span, Instrument};

use super::backend::HttpConnError;
use super::conn_pool_lib::{ClientInnerExt, ConnInfo};
use crate::context::RequestMonitoring;
use crate::control_plane::messages::{ColdStartInfo, MetricsAuxInfo};
use crate::metrics::{HttpEndpointPoolsGuard, Metrics};
use crate::types::EndpointCacheKey;
use crate::usage_metrics::{Ids, MetricCounter, USAGE_METRICS};

pub(crate) type Send = http2::SendRequest<hyper::body::Incoming>;
pub(crate) type Connect =
    http2::Connection<TokioIo<TcpStream>, hyper::body::Incoming, TokioExecutor>;

#[derive(Clone)]
pub(crate) struct ConnPoolEntry<C: ClientInnerExt + Clone> {
    conn: C,
    conn_id: uuid::Uuid,
    aux: MetricsAuxInfo,
}

pub(crate) struct ClientDataHttp();

// Per-endpoint connection pool
// Number of open connections is limited by the `max_conns_per_endpoint`.
pub(crate) struct EndpointConnPool<C: ClientInnerExt + Clone> {
    // TODO(conrad):
    // either we should open more connections depending on stream count
    // (not exposed by hyper, need our own counter)
    // or we can change this to an Option rather than a VecDeque.
    //
    // Opening more connections to the same db because we run out of streams
    // seems somewhat redundant though.
    //
    // Probably we should run a semaphore and just the single conn. TBD.
    conns: VecDeque<ConnPoolEntry<C>>,
    _guard: HttpEndpointPoolsGuard<'static>,
    global_connections_count: Arc<AtomicUsize>,
}

impl<C: ClientInnerExt + Clone> EndpointConnPool<C> {
    fn get_conn_entry(&mut self) -> Option<ConnPoolEntry<C>> {
        let Self { conns, .. } = self;

        loop {
            let conn = conns.pop_front()?;
            if !conn.conn.is_closed() {
                conns.push_back(conn.clone());
                return Some(conn);
            }
        }
    }

    fn remove_conn(&mut self, conn_id: uuid::Uuid) -> bool {
        let Self {
            conns,
            global_connections_count,
            ..
        } = self;

        let old_len = conns.len();
        conns.retain(|conn| conn.conn_id != conn_id);
        let new_len = conns.len();
        let removed = old_len - new_len;
        if removed > 0 {
            global_connections_count.fetch_sub(removed, atomic::Ordering::Relaxed);
            Metrics::get()
                .proxy
                .http_pool_opened_connections
                .get_metric()
                .dec_by(removed as i64);
        }
        removed > 0
    }
}

impl<C: ClientInnerExt + Clone> Drop for EndpointConnPool<C> {
    fn drop(&mut self) {
        if !self.conns.is_empty() {
            self.global_connections_count
                .fetch_sub(self.conns.len(), atomic::Ordering::Relaxed);
            Metrics::get()
                .proxy
                .http_pool_opened_connections
                .get_metric()
                .dec_by(self.conns.len() as i64);
        }
    }
}

pub(crate) struct GlobalConnPool<C: ClientInnerExt + Clone> {
    // endpoint -> per-endpoint connection pool
    //
    // That should be a fairly conteded map, so return reference to the per-endpoint
    // pool as early as possible and release the lock.
    global_pool: DashMap<EndpointCacheKey, Arc<RwLock<EndpointConnPool<C>>>>,

    /// Number of endpoint-connection pools
    ///
    /// [`DashMap::len`] iterates over all inner pools and acquires a read lock on each.
    /// That seems like far too much effort, so we're using a relaxed increment counter instead.
    /// It's only used for diagnostics.
    global_pool_size: AtomicUsize,

    /// Total number of connections in the pool
    global_connections_count: Arc<AtomicUsize>,

    config: &'static crate::config::HttpConfig,
}

impl<C: ClientInnerExt + Clone> GlobalConnPool<C> {
    pub(crate) fn new(config: &'static crate::config::HttpConfig) -> Arc<Self> {
        let shards = config.pool_options.pool_shards;
        Arc::new(Self {
            global_pool: DashMap::with_shard_amount(shards),
            global_pool_size: AtomicUsize::new(0),
            config,
            global_connections_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn shutdown(&self) {
        // drops all strong references to endpoint-pools
        self.global_pool.clear();
    }

    pub(crate) async fn gc_worker(&self, mut rng: impl Rng) {
        let epoch = self.config.pool_options.gc_epoch;
        let mut interval = tokio::time::interval(epoch / (self.global_pool.shards().len()) as u32);
        loop {
            interval.tick().await;

            let shard = rng.gen_range(0..self.global_pool.shards().len());
            self.gc(shard);
        }
    }

    fn gc(&self, shard: usize) {
        debug!(shard, "pool: performing epoch reclamation");

        // acquire a random shard lock
        let mut shard = self.global_pool.shards()[shard].write();

        let timer = Metrics::get()
            .proxy
            .http_pool_reclaimation_lag_seconds
            .start_timer();
        let current_len = shard.len();
        let mut clients_removed = 0;
        shard.retain(|endpoint, x| {
            // if the current endpoint pool is unique (no other strong or weak references)
            // then it is currently not in use by any connections.
            if let Some(pool) = Arc::get_mut(x.get_mut()) {
                let EndpointConnPool { conns, .. } = pool.get_mut();

                let old_len = conns.len();

                conns.retain(|conn| !conn.conn.is_closed());

                let new_len = conns.len();
                let removed = old_len - new_len;
                clients_removed += removed;

                // we only remove this pool if it has no active connections
                if conns.is_empty() {
                    info!("pool: discarding pool for endpoint {endpoint}");
                    return false;
                }
            }

            true
        });

        let new_len = shard.len();
        drop(shard);
        timer.observe();

        // Do logging outside of the lock.
        if clients_removed > 0 {
            let size = self
                .global_connections_count
                .fetch_sub(clients_removed, atomic::Ordering::Relaxed)
                - clients_removed;
            Metrics::get()
                .proxy
                .http_pool_opened_connections
                .get_metric()
                .dec_by(clients_removed as i64);
            info!("pool: performed global pool gc. removed {clients_removed} clients, total number of clients in pool is {size}");
        }
        let removed = current_len - new_len;

        if removed > 0 {
            let global_pool_size = self
                .global_pool_size
                .fetch_sub(removed, atomic::Ordering::Relaxed)
                - removed;
            info!("pool: performed global pool gc. size now {global_pool_size}");
        }
    }

    #[expect(unused_results)]
    pub(crate) fn get(
        self: &Arc<Self>,
        ctx: &RequestMonitoring,
        conn_info: &ConnInfo,
    ) -> Result<Option<Client<C>>, HttpConnError> {
        let result: Result<Option<Client<C>>, HttpConnError>;
        let Some(endpoint) = conn_info.endpoint_cache_key() else {
            result = Ok(None);
            return result;
        };
        let endpoint_pool = self.get_or_create_endpoint_pool(&endpoint);
        let Some(client) = endpoint_pool.write().get_conn_entry() else {
            result = Ok(None);
            return result;
        };

        tracing::Span::current().record("conn_id", tracing::field::display(client.conn_id));
        info!(
            cold_start_info = ColdStartInfo::HttpPoolHit.as_str(),
            "pool: reusing connection '{conn_info}'"
        );
        ctx.set_cold_start_info(ColdStartInfo::HttpPoolHit);
        ctx.success();
        Ok(Some(Client::new(client.conn, client.aux)))
    }

    fn get_or_create_endpoint_pool(
        self: &Arc<Self>,
        endpoint: &EndpointCacheKey,
    ) -> Arc<RwLock<EndpointConnPool<C>>> {
        // fast path
        if let Some(pool) = self.global_pool.get(endpoint) {
            return pool.clone();
        }

        // slow path
        let new_pool = Arc::new(RwLock::new(EndpointConnPool {
            conns: VecDeque::new(),
            _guard: Metrics::get().proxy.http_endpoint_pools.guard(),
            global_connections_count: self.global_connections_count.clone(),
        }));

        // find or create a pool for this endpoint
        let mut created = false;
        let pool = self
            .global_pool
            .entry(endpoint.clone())
            .or_insert_with(|| {
                created = true;
                new_pool
            })
            .clone();

        // log new global pool size
        if created {
            let global_pool_size = self
                .global_pool_size
                .fetch_add(1, atomic::Ordering::Relaxed)
                + 1;
            info!(
                "pool: created new pool for '{endpoint}', global pool size now {global_pool_size}"
            );
        }

        pool
    }
}

pub(crate) fn poll_http2_client(
    global_pool: Arc<GlobalConnPool<Send>>,
    ctx: &RequestMonitoring,
    conn_info: &ConnInfo,
    client: Send,
    connection: Connect,
    conn_id: uuid::Uuid,
    aux: MetricsAuxInfo,
) -> Client<Send> {
    let conn_gauge = Metrics::get().proxy.db_connections.guard(ctx.protocol());
    let session_id = ctx.session_id();

    let span = info_span!(parent: None, "connection", %conn_id);
    let cold_start_info = ctx.cold_start_info();
    span.in_scope(|| {
        info!(cold_start_info = cold_start_info.as_str(), %conn_info, %session_id, "new connection");
    });

    let pool = match conn_info.endpoint_cache_key() {
        Some(endpoint) => {
            let pool = global_pool.get_or_create_endpoint_pool(&endpoint);

            pool.write().conns.push_back(ConnPoolEntry {
                conn: client.clone(),
                conn_id,
                aux: aux.clone(),
            });
            Metrics::get()
                .proxy
                .http_pool_opened_connections
                .get_metric()
                .inc();

            Arc::downgrade(&pool)
        }
        None => Weak::new(),
    };

    tokio::spawn(
        async move {
            let _conn_gauge = conn_gauge;
            let res = connection.await;
            match res {
                Ok(()) => info!("connection closed"),
                Err(e) => error!(%session_id, "connection error: {e:?}"),
            }

            // remove from connection pool
            if let Some(pool) = pool.clone().upgrade() {
                if pool.write().remove_conn(conn_id) {
                    info!("closed connection removed");
                }
            }
        }
        .instrument(span),
    );

    Client::new(client, aux)
}

pub(crate) struct Client<C: ClientInnerExt + Clone> {
    pub(crate) inner: C,
    aux: MetricsAuxInfo,
}

impl<C: ClientInnerExt + Clone> Client<C> {
    pub(self) fn new(inner: C, aux: MetricsAuxInfo) -> Self {
        Self { inner, aux }
    }

    pub(crate) fn metrics(&self) -> Arc<MetricCounter> {
        USAGE_METRICS.register(Ids {
            endpoint_id: self.aux.endpoint_id,
            branch_id: self.aux.branch_id,
        })
    }
}

impl ClientInnerExt for Send {
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    fn get_process_id(&self) -> i32 {
        // ideally throw something meaningful
        -1
    }
}
