//! Node API
//!
//! A node is a server that serves various protocols.
//!
//! You can monitor what is happening in the node using [`Node::subscribe`].
//!
//! To shut down the node, call [`Node::shutdown`].
use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt, Stream, StreamExt, TryFutureExt};
use iroh_base::rpc::RpcResult;
use iroh_bytes::hashseq::parse_hash_seq;
use iroh_bytes::provider::DownloadProgress;
use iroh_bytes::store::{
    ExportMode, GcMarkEvent, GcSweepEvent, ImportProgress, Map, MapEntry, ReadableStore,
    Store as BaoStore, ValidateProgress,
};
use iroh_bytes::util::progress::{FlumeProgressSender, IdGenerator, ProgressSender};
use iroh_bytes::{
    protocol::Closed, provider::AddProgress, BlobFormat, Hash, HashAndFormat, TempTag,
};
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use iroh_io::AsyncSliceReader;
use iroh_net::magic_endpoint::get_alpn;
use iroh_net::util::AbortingJoinHandle;
use iroh_net::{
    config::Endpoint,
    derp::DerpMode,
    key::{PublicKey, SecretKey},
    tls, MagicEndpoint, NodeAddr,
};
use iroh_sync::store::Store as DocStore;
use quic_rpc::server::{RpcChannel, RpcServerError};
use quic_rpc::transport::flume::FlumeConnection;
use quic_rpc::transport::misc::DummyServerEndpoint;
use quic_rpc::{RpcClient, RpcServer, ServiceEndpoint};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::LocalPoolHandle;
use tracing::{debug, error, error_span, info, trace, warn, Instrument};

use crate::downloader::Downloader;
use crate::rpc_protocol::{
    BlobAddPathRequest, BlobAddPathResponse, BlobAddStreamRequest, BlobAddStreamResponse,
    BlobAddStreamUpdate, BlobDeleteBlobRequest, BlobDownloadRequest, BlobListCollectionsRequest,
    BlobListCollectionsResponse, BlobListIncompleteRequest, BlobListIncompleteResponse,
    BlobListRequest, BlobListResponse, BlobReadRequest, BlobReadResponse, BlobValidateRequest,
    DeleteTagRequest, DocExportFileRequest, DocExportFileResponse, DocExportProgress,
    DocImportFileRequest, DocImportFileResponse, DocImportProgress, DocSetHashRequest,
    DownloadLocation, ListTagsRequest, ListTagsResponse, NodeConnectionInfoRequest,
    NodeConnectionInfoResponse, NodeConnectionsRequest, NodeConnectionsResponse,
    NodeShutdownRequest, NodeStatsRequest, NodeStatsResponse, NodeStatusRequest,
    NodeStatusResponse, NodeWatchRequest, NodeWatchResponse, ProviderRequest, ProviderResponse,
    ProviderService, SetTagOption,
};
use crate::sync_engine::{SyncEngine, SYNC_ALPN};
use crate::ticket::blob::Ticket;

const MAX_CONNECTIONS: u32 = 1024;
const MAX_STREAMS: u64 = 10;
const HEALTH_POLL_WAIT: Duration = Duration::from_secs(1);

/// Default bind address for the node.
/// 11204 is "iroh" in leetspeak <https://simple.wikipedia.org/wiki/Leet>
pub const DEFAULT_BIND_PORT: u16 = 11204;

/// How long we wait at most for some endpoints to be discovered.
const ENDPOINT_WAIT: Duration = Duration::from_secs(5);

/// Chunk size for getting blobs over RPC
const RPC_BLOB_GET_CHUNK_SIZE: usize = 1024 * 64;
/// Channel cap for getting blobs over RPC
const RPC_BLOB_GET_CHANNEL_CAP: usize = 2;
/// Default interval between GC runs.
const DEFAULT_GC_INTERVAL: Duration = Duration::from_secs(60 * 5);

/// Policy for garbage collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GcPolicy {
    /// Garbage collection is disabled.
    Disabled,
    /// Garbage collection is run at the given interval.
    Interval(Duration),
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self::Interval(DEFAULT_GC_INTERVAL)
    }
}

/// Builder for the [`Node`].
///
/// You must supply a blob store and a document store.
///
/// Blob store implementations are available in [`iroh_bytes::store`].
/// Document store implementations are available in [`iroh_sync::store`].
///
/// Everything else is optional.
///
/// Finally you can create and run the node by calling [`Builder::spawn`].
///
/// The returned [`Node`] is awaitable to know when it finishes.  It can be terminated
/// using [`Node::shutdown`].
#[derive(Debug)]
pub struct Builder<D, S = iroh_sync::store::memory::Store, E = DummyServerEndpoint>
where
    D: Map,
    S: DocStore,
    E: ServiceEndpoint<ProviderService>,
{
    bind_port: u16,
    secret_key: SecretKey,
    rpc_endpoint: E,
    db: D,
    keylog: bool,
    derp_mode: DerpMode,
    gc_policy: GcPolicy,
    rt: Option<tokio_util::task::LocalPoolHandle>,
    docs: S,
    /// Path to store peer data. If `None`, peer data will not be persisted.
    peers_data_path: Option<PathBuf>,
}

const PROTOCOLS: [&[u8]; 3] = [&iroh_bytes::protocol::ALPN, GOSSIP_ALPN, SYNC_ALPN];

impl<D: Map, S: DocStore> Builder<D, S> {
    /// Creates a new builder for [`Node`] using the given database.
    fn with_db_and_store(db: D, docs: S) -> Self {
        Self {
            bind_port: DEFAULT_BIND_PORT,
            secret_key: SecretKey::generate(),
            db,
            keylog: false,
            derp_mode: DerpMode::Default,
            rpc_endpoint: Default::default(),
            gc_policy: GcPolicy::Disabled,
            rt: None,
            docs,
            peers_data_path: None,
        }
    }
}

impl<D, S, E> Builder<D, S, E>
where
    D: BaoStore,
    S: DocStore,
    E: ServiceEndpoint<ProviderService>,
{
    /// Configure rpc endpoint, changing the type of the builder to the new endpoint type.
    pub fn rpc_endpoint<E2: ServiceEndpoint<ProviderService>>(
        self,
        value: E2,
    ) -> Builder<D, S, E2> {
        // we can't use ..self here because the return type is different
        Builder {
            bind_port: self.bind_port,
            secret_key: self.secret_key,
            db: self.db,
            keylog: self.keylog,
            rpc_endpoint: value,
            derp_mode: self.derp_mode,
            gc_policy: self.gc_policy,
            rt: self.rt,
            docs: self.docs,
            peers_data_path: self.peers_data_path,
        }
    }

    /// Sets the garbage collection policy.
    ///
    /// By default garbage collection is disabled.
    pub fn gc_policy(mut self, gc_policy: GcPolicy) -> Self {
        self.gc_policy = gc_policy;
        self
    }

    /// Sets the DERP servers to assist in establishing connectivity.
    ///
    /// DERP servers are used to discover other nodes by [`PublicKey`] and also help
    /// establish connections between peers by being an initial relay for traffic while
    /// assisting in holepunching to establish a direct connection between peers.
    ///
    /// When using [DerpMode::Custom], the provided `derp_map` must contain at least one
    /// region with a configured derp node.  If an invalid [`iroh_net::derp::DerpMap`]
    /// is provided [`Self::spawn`] will result in an error.
    pub fn derp_mode(mut self, dm: DerpMode) -> Self {
        self.derp_mode = dm;
        self
    }

    /// Binds the node service to a different socket.
    ///
    /// By default it binds to `127.0.0.1:11204`.
    pub fn bind_port(mut self, port: u16) -> Self {
        self.bind_port = port;
        self
    }

    /// Uses the given [`SecretKey`] for the [`PublicKey`] instead of a newly generated one.
    pub fn secret_key(mut self, secret_key: SecretKey) -> Self {
        self.secret_key = secret_key;
        self
    }

    /// Whether to log the SSL pre-master key.
    ///
    /// If `true` and the `SSLKEYLOGFILE` environment variable is the path to a file this
    /// file will be used to log the SSL pre-master key.  This is useful to inspect captured
    /// traffic.
    pub fn keylog(mut self, keylog: bool) -> Self {
        self.keylog = keylog;
        self
    }

    /// Set the path where known peer data is loaded on start-up and later persisted.
    pub fn peers_data_path(mut self, path: PathBuf) -> Self {
        self.peers_data_path = Some(path);
        self
    }

    /// Sets the tokio runtime to use.
    ///
    /// If not set, the current runtime will be picked up.
    pub fn local_pool(mut self, rt: &LocalPoolHandle) -> Self {
        self.rt = Some(rt.clone());
        self
    }

    /// Spawns the [`Node`] in a tokio task.
    ///
    /// This will create the underlying network server and spawn a tokio task accepting
    /// connections.  The returned [`Node`] can be used to control the task as well as
    /// get information about it.
    pub async fn spawn(self) -> Result<Node<D>> {
        trace!("spawning node");
        let lp = self
            .rt
            .unwrap_or_else(|| LocalPoolHandle::new(num_cpus::get()));
        // Initialize the metrics collection.
        //
        // The metrics are global per process. Subsequent calls do not change the metrics
        // collection and will return an error. We ignore this error. This means that if you'd
        // spawn multiple Iroh nodes in the same process, the metrics would be shared between the
        // nodes.
        #[cfg(feature = "metrics")]
        crate::metrics::try_init_metrics_collection().ok();

        let mut transport_config = quinn::TransportConfig::default();
        transport_config
            .max_concurrent_bidi_streams(MAX_STREAMS.try_into()?)
            .max_concurrent_uni_streams(0u32.into());

        let endpoint = MagicEndpoint::builder()
            .secret_key(self.secret_key.clone())
            .alpns(PROTOCOLS.iter().map(|p| p.to_vec()).collect())
            .keylog(self.keylog)
            .transport_config(transport_config)
            .concurrent_connections(MAX_CONNECTIONS)
            .derp_mode(self.derp_mode);
        let endpoint = match self.peers_data_path {
            Some(path) => endpoint.peers_data_path(path),
            None => endpoint,
        };
        let endpoint = endpoint.bind(self.bind_port).await?;
        trace!("created quinn endpoint");

        let (cb_sender, cb_receiver) = mpsc::channel(8);
        let cancel_token = CancellationToken::new();

        debug!("rpc listening on: {:?}", self.rpc_endpoint.local_addr());

        let addr = endpoint.my_addr().await?;

        // initialize the gossip protocol
        let gossip = Gossip::from_endpoint(endpoint.clone(), Default::default(), &addr.info);

        // spawn the sync engine
        let downloader = Downloader::new(self.db.clone(), endpoint.clone(), lp.clone());
        let ds = self.docs.clone();
        let sync = SyncEngine::spawn(
            endpoint.clone(),
            gossip.clone(),
            self.docs,
            self.db.clone(),
            downloader,
        );

        let callbacks = Callbacks::default();
        let gc_task = if let GcPolicy::Interval(gc_period) = self.gc_policy {
            tracing::info!("Starting GC task with interval {:?}", gc_period);
            let db = self.db.clone();
            let callbacks = callbacks.clone();
            let task = lp.spawn_pinned(move || Self::gc_loop(db, ds, gc_period, callbacks));
            Some(AbortingJoinHandle(task))
        } else {
            None
        };
        let (internal_rpc, controller) = quic_rpc::transport::flume::connection(1);
        let inner = Arc::new(NodeInner {
            db: self.db,
            endpoint: endpoint.clone(),
            secret_key: self.secret_key,
            controller,
            cancel_token,
            callbacks: callbacks.clone(),
            cb_sender,
            gc_task,
            rt: lp.clone(),
            sync,
        });
        let task = {
            let gossip = gossip.clone();
            let handler = RpcHandler {
                inner: inner.clone(),
            };
            let me = endpoint.node_id().fmt_short();
            let ep = endpoint.clone();
            tokio::task::spawn(
                async move {
                    Self::run(
                        ep,
                        callbacks,
                        cb_receiver,
                        handler,
                        self.rpc_endpoint,
                        internal_rpc,
                        gossip,
                    )
                    .await
                }
                .instrument(error_span!("node", %me)),
            )
        };
        let node = Node {
            inner,
            task: task.map_err(Arc::new).boxed().shared(),
        };

        // spawn a task that updates the gossip endpoints.
        // TODO: track task
        let ep = endpoint.clone();
        tokio::task::spawn(async move {
            loop {
                match ep.local_endpoints_change().await {
                    Ok(eps) => {
                        if let Err(err) = gossip.update_endpoints(&eps) {
                            warn!("Failed to update gossip endpoints: {err:?}");
                        }
                    }
                    Err(err) => {
                        warn!("failed to retrieve local endpoints: {err:?}");
                        break;
                    }
                }
            }
        });

        // Wait for a single endpoint update, to make sure
        // we found some endpoints
        tokio::time::timeout(ENDPOINT_WAIT, endpoint.local_endpoints())
            .await
            .context("waiting for endpoint")??;

        Ok(node)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        server: MagicEndpoint,
        callbacks: Callbacks,
        mut cb_receiver: mpsc::Receiver<EventCallback>,
        handler: RpcHandler<D>,
        rpc: E,
        internal_rpc: impl ServiceEndpoint<ProviderService>,
        gossip: Gossip,
    ) {
        let rpc = RpcServer::new(rpc);
        let internal_rpc = RpcServer::new(internal_rpc);
        if let Ok((ipv4, ipv6)) = server.local_addr() {
            debug!(
                "listening at: {}{}",
                ipv4,
                ipv6.map(|addr| format!(" and {addr}")).unwrap_or_default()
            );
        }
        let cancel_token = handler.inner.cancel_token.clone();

        // forward our initial endpoints to the gossip protocol
        // it may happen the the first endpoint update callback is missed because the gossip cell
        // is only initialized once the endpoint is fully bound
        if let Ok(local_endpoints) = server.local_endpoints().await {
            debug!(me = ?server.node_id(), "gossip initial update: {local_endpoints:?}");
            gossip.update_endpoints(&local_endpoints).ok();
        }

        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => break,
                // handle rpc requests. This will do nothing if rpc is not configured, since
                // accept is just a pending future.
                request = rpc.accept() => {
                    match request {
                        Ok((msg, chan)) => {
                            handle_rpc_request(msg, chan, &handler);
                        }
                        Err(e) => {
                            info!("rpc request error: {:?}", e);
                        }
                    }
                },
                // handle internal rpc requests.
                request = internal_rpc.accept() => {
                    match request {
                        Ok((msg, chan)) => {
                            handle_rpc_request(msg, chan, &handler);
                        }
                        Err(_) => {
                            info!("last controller dropped, shutting down");
                            break;
                        }
                    }
                },
                // handle incoming p2p connections
                Some(mut connecting) = server.accept() => {
                    let alpn = match get_alpn(&mut connecting).await {
                        Ok(alpn) => alpn,
                        Err(err) => {
                            error!("invalid handshake: {:?}", err);
                            continue;
                        }
                    };
                    let gossip = gossip.clone();
                    let inner = handler.inner.clone();
                    let sync = handler.inner.sync.clone();
                    tokio::task::spawn(async move {
                        if let Err(err) = handle_connection(connecting, alpn, inner, gossip, sync).await {
                            warn!("Handling incoming connection ended with error: {err}");
                        }
                    });
                },
                // Handle new callbacks
                Some(cb) = cb_receiver.recv() => {
                    callbacks.push(cb).await;
                }
                else => break,
            }
        }

        // Closing the Endpoint is the equivalent of calling Connection::close on all
        // connections: Operations will immediately fail with
        // ConnectionError::LocallyClosed.  All streams are interrupted, this is not
        // graceful.
        let error_code = Closed::ProviderTerminating;
        server
            .close(error_code.into(), error_code.reason())
            .await
            .ok();
    }

    async fn gc_loop(db: D, ds: S, gc_period: Duration, callbacks: Callbacks) {
        tracing::debug!("GC loop starting {:?}", gc_period);
        'outer: loop {
            // do delay before the two phases of GC
            tokio::time::sleep(gc_period).await;
            tracing::debug!("Starting GC");
            callbacks
                .send(Event::Db(iroh_bytes::store::Event::GcStarted))
                .await;
            db.clear_live();
            let doc_hashes = match ds.content_hashes() {
                Ok(hashes) => hashes,
                Err(err) => {
                    tracing::error!("Error getting doc hashes: {}", err);
                    continue 'outer;
                }
            };
            let mut doc_db_error = false;
            let doc_hashes = doc_hashes.filter_map(|e| match e {
                Ok(hash) => Some(hash),
                Err(err) => {
                    tracing::error!("Error getting doc hash: {}", err);
                    doc_db_error = true;
                    None
                }
            });
            db.add_live(doc_hashes);
            if doc_db_error {
                tracing::error!("Error getting doc hashes, skipping GC to be safe");
                continue 'outer;
            }

            tracing::debug!("Starting GC mark phase");
            let mut stream = db.gc_mark(None);
            while let Some(item) = stream.next().await {
                match item {
                    GcMarkEvent::CustomDebug(text) => {
                        tracing::debug!("{}", text);
                    }
                    GcMarkEvent::CustomWarning(text, _) => {
                        tracing::warn!("{}", text);
                    }
                    GcMarkEvent::Error(err) => {
                        tracing::error!("Fatal error during GC mark {}", err);
                        continue 'outer;
                    }
                }
            }

            tracing::debug!("Starting GC sweep phase");
            let mut stream = db.gc_sweep();
            while let Some(item) = stream.next().await {
                match item {
                    GcSweepEvent::CustomDebug(text) => {
                        tracing::debug!("{}", text);
                    }
                    GcSweepEvent::CustomWarning(text, _) => {
                        tracing::warn!("{}", text);
                    }
                    GcSweepEvent::Error(err) => {
                        tracing::error!("Fatal error during GC mark {}", err);
                        continue 'outer;
                    }
                }
            }
            callbacks
                .send(Event::Db(iroh_bytes::store::Event::GcCompleted))
                .await;
        }
    }
}

// TODO: Restructure this code to not take all these arguments.
#[allow(clippy::too_many_arguments)]
async fn handle_connection<D: BaoStore>(
    connecting: quinn::Connecting,
    alpn: String,
    node: Arc<NodeInner<D>>,
    gossip: Gossip,
    sync: SyncEngine,
) -> Result<()> {
    match alpn.as_bytes() {
        GOSSIP_ALPN => gossip.handle_connection(connecting.await?).await?,
        SYNC_ALPN => sync.handle_connection(connecting).await?,
        alpn if alpn == iroh_bytes::protocol::ALPN => {
            iroh_bytes::provider::handle_connection(
                connecting,
                node.db.clone(),
                node.callbacks.clone(),
                node.rt.clone(),
            )
            .await
        }
        _ => bail!("ignoring connection: unsupported ALPN protocol"),
    }
    Ok(())
}

type EventCallback = Box<dyn Fn(Event) -> BoxFuture<'static, ()> + 'static + Sync + Send>;

#[derive(Default, derive_more::Debug, Clone)]
struct Callbacks(#[debug("..")] Arc<RwLock<Vec<EventCallback>>>);

impl Callbacks {
    async fn push(&self, cb: EventCallback) {
        self.0.write().await.push(cb);
    }

    #[allow(dead_code)]
    async fn send(&self, event: Event) {
        let cbs = self.0.read().await;
        for cb in &*cbs {
            cb(event.clone()).await;
        }
    }
}

impl iroh_bytes::provider::EventSender for Callbacks {
    fn send(&self, event: iroh_bytes::provider::Event) -> BoxFuture<()> {
        async move {
            let cbs = self.0.read().await;
            for cb in &*cbs {
                cb(Event::ByteProvide(event.clone())).await;
            }
        }
        .boxed()
    }
}

/// A server which implements the iroh node.
///
/// Clients can connect to this server and requests hashes from it.
///
/// The only way to create this is by using the [`Builder::spawn`].  [`Node::builder`]
/// is a shorthand to create a suitable [`Builder`].
///
/// This runs a tokio task which can be aborted and joined if desired.  To join the task
/// await the [`Node`] struct directly, it will complete when the task completes.  If
/// this is dropped the node task is not stopped but keeps running.
#[derive(Debug, Clone)]
pub struct Node<D> {
    inner: Arc<NodeInner<D>>,
    task: Shared<BoxFuture<'static, Result<(), Arc<JoinError>>>>,
}

#[derive(derive_more::Debug)]
struct NodeInner<D> {
    db: D,
    endpoint: MagicEndpoint,
    secret_key: SecretKey,
    cancel_token: CancellationToken,
    controller: FlumeConnection<ProviderResponse, ProviderRequest>,
    #[debug("callbacks: Sender<Box<dyn Fn(Event)>>")]
    cb_sender: mpsc::Sender<Box<dyn Fn(Event) -> BoxFuture<'static, ()> + Send + Sync + 'static>>,
    callbacks: Callbacks,
    #[allow(dead_code)]
    gc_task: Option<AbortingJoinHandle<()>>,
    #[debug("rt")]
    rt: LocalPoolHandle,
    pub(crate) sync: SyncEngine,
}

/// Events emitted by the [`Node`] informing about the current status.
#[derive(Debug, Clone)]
pub enum Event {
    /// Events from the iroh-bytes transfer protocol.
    ByteProvide(iroh_bytes::provider::Event),
    /// Events from database
    Db(iroh_bytes::store::Event),
}

impl<D: ReadableStore> Node<D> {
    /// Returns a new builder for the [`Node`].
    ///
    /// Once the done with the builder call [`Builder::spawn`] to create the node.
    pub fn builder<S: DocStore>(bao_store: D, doc_store: S) -> Builder<D, S> {
        Builder::with_db_and_store(bao_store, doc_store)
    }

    /// Returns the [`MagicEndpoint`] of the node.
    ///
    /// This can be used to establish connections to other nodes under any
    /// ALPNs other than the iroh internal ones. This is useful for some advanced
    /// use cases.
    pub fn magic_endpoint(&self) -> &MagicEndpoint {
        &self.inner.endpoint
    }

    /// The address on which the node socket is bound.
    ///
    /// Note that this could be an unspecified address, if you need an address on which you
    /// can contact the node consider using [`Node::local_endpoint_addresses`].  However the
    /// port will always be the concrete port.
    pub fn local_address(&self) -> Result<Vec<SocketAddr>> {
        self.inner.local_address()
    }

    /// Lists the local endpoint of this node.
    pub async fn local_endpoints(&self) -> Result<Vec<Endpoint>> {
        self.inner.local_endpoints().await
    }

    /// Convenience method to get just the addr part of [`Node::local_endpoints`].
    pub async fn local_endpoint_addresses(&self) -> Result<Vec<SocketAddr>> {
        self.inner.local_endpoint_addresses().await
    }

    /// Returns the [`PublicKey`] of the node.
    pub fn node_id(&self) -> PublicKey {
        self.inner.secret_key.public()
    }

    /// Subscribe to [`Event`]s emitted from the node, informing about connections and
    /// progress.
    ///
    /// Warning: The callback must complete quickly, as otherwise it will block ongoing work.
    pub async fn subscribe<F: Fn(Event) -> BoxFuture<'static, ()> + Send + Sync + 'static>(
        &self,
        cb: F,
    ) -> Result<()> {
        self.inner.cb_sender.send(Box::new(cb)).await?;
        Ok(())
    }

    /// Returns a handle that can be used to do RPC calls to the node internally.
    pub fn controller(&self) -> crate::client::mem::RpcClient {
        RpcClient::new(self.inner.controller.clone())
    }

    /// Return a client to control this node over an in-memory channel.
    pub fn client(&self) -> crate::client::mem::Iroh {
        crate::client::Iroh::new(self.controller())
    }

    /// Return a single token containing everything needed to get a hash.
    ///
    /// See [`Ticket`] for more details of how it can be used.
    pub async fn ticket(&self, hash: Hash, format: BlobFormat) -> Result<Ticket> {
        // TODO: Verify that the hash exists in the db?
        let me = self.my_addr().await?;
        Ticket::new(me, hash, format)
    }

    /// Return the [`NodeAddr`] for this node.
    pub async fn my_addr(&self) -> Result<NodeAddr> {
        self.inner.endpoint.my_addr().await
    }

    /// Get the DERP region we are connected to.
    pub fn my_derp(&self) -> Option<u16> {
        self.inner.endpoint.my_derp()
    }

    /// Aborts the node.
    ///
    /// This does not gracefully terminate currently: all connections are closed and
    /// anything in-transit is lost.  The task will stop running and awaiting this
    /// [`Node`] will complete.
    ///
    /// The shutdown behaviour will become more graceful in the future.
    pub fn shutdown(&self) {
        self.inner.cancel_token.cancel();
    }

    /// Returns a token that can be used to cancel the node.
    pub fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel_token.clone()
    }
}

impl<D> NodeInner<D> {
    async fn local_endpoints(&self) -> Result<Vec<Endpoint>> {
        self.endpoint.local_endpoints().await
    }

    async fn local_endpoint_addresses(&self) -> Result<Vec<SocketAddr>> {
        let endpoints = self.local_endpoints().await?;
        Ok(endpoints.into_iter().map(|x| x.addr).collect())
    }

    fn local_address(&self) -> Result<Vec<SocketAddr>> {
        let (v4, v6) = self.endpoint.local_addr()?;
        let mut addrs = vec![v4];
        if let Some(v6) = v6 {
            addrs.push(v6);
        }
        Ok(addrs)
    }
}

/// The future completes when the spawned tokio task finishes.
impl<D> Future for Node<D> {
    type Output = Result<(), Arc<JoinError>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

#[derive(Debug, Clone)]
struct RpcHandler<D> {
    inner: Arc<NodeInner<D>>,
}

impl<D: BaoStore> RpcHandler<D> {
    fn rt(&self) -> LocalPoolHandle {
        self.inner.rt.clone()
    }

    fn blob_list(
        self,
        _msg: BlobListRequest,
    ) -> impl Stream<Item = BlobListResponse> + Send + 'static {
        use bao_tree::io::fsm::Outboard;

        let db = self.inner.db.clone();
        futures::stream::iter(db.blobs()).filter_map(move |hash| {
            let db = db.clone();
            async move {
                let entry = db.get(&hash)?;
                let hash = entry.hash().into();
                let size = entry.outboard().await.ok()?.tree().size().0;
                let path = "".to_owned();
                Some(BlobListResponse { hash, size, path })
            }
        })
    }

    fn blob_list_incomplete(
        self,
        _msg: BlobListIncompleteRequest,
    ) -> impl Stream<Item = BlobListIncompleteResponse> + Send + 'static {
        let db = self.inner.db.clone();
        let local = self.inner.rt.clone();
        futures::stream::iter(db.partial_blobs()).filter_map(move |hash| {
            let db = db.clone();
            let t = local.spawn_pinned(move || async move {
                let Some(entry) = db.get_partial(&hash) else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "no partial entry found",
                    ));
                };
                io::Result::Ok(BlobListIncompleteResponse {
                    hash,
                    size: 0,
                    expected_size: entry.size(),
                })
            });
            async move { t.await.ok()?.ok() }
        })
    }

    fn blob_list_collections(
        self,
        _msg: BlobListCollectionsRequest,
    ) -> impl Stream<Item = BlobListCollectionsResponse> + Send + 'static {
        let db = self.inner.db.clone();
        let local = self.inner.rt.clone();
        let tags = db.tags();
        futures::stream::iter(tags).filter_map(move |(name, HashAndFormat { hash, format })| {
            let db = db.clone();
            let local = local.clone();
            async move {
                if !format.is_hash_seq() {
                    return None;
                }
                let entry = db.get(&hash)?;
                let count = local
                    .spawn_pinned(|| async move {
                        let reader = entry.data_reader().await.ok()?;
                        let (_collection, count) = parse_hash_seq(reader).await.ok()?;
                        Some(count)
                    })
                    .await
                    .ok()??;
                Some(BlobListCollectionsResponse {
                    tag: name,
                    hash,
                    total_blobs_count: Some(count),
                    total_blobs_size: None,
                })
            }
        })
    }

    async fn blob_delete_tag(self, msg: DeleteTagRequest) -> RpcResult<()> {
        self.inner.db.set_tag(msg.name, None).await?;
        Ok(())
    }

    async fn blob_delete_blob(self, msg: BlobDeleteBlobRequest) -> RpcResult<()> {
        self.inner.db.delete(&msg.hash).await?;
        Ok(())
    }

    fn blob_list_tags(
        self,
        _msg: ListTagsRequest,
    ) -> impl Stream<Item = ListTagsResponse> + Send + 'static {
        tracing::info!("blob_list_tags");
        futures::stream::iter(
            self.inner
                .db
                .tags()
                .map(|(name, HashAndFormat { hash, format })| {
                    tracing::info!("{:?} {} {:?}", name, hash, format);
                    ListTagsResponse { name, hash, format }
                }),
        )
    }

    /// Invoke validate on the database and stream out the result
    fn blob_validate(
        self,
        _msg: BlobValidateRequest,
    ) -> impl Stream<Item = ValidateProgress> + Send + 'static {
        let (tx, rx) = mpsc::channel(1);
        let tx2 = tx.clone();
        let db = self.inner.db.clone();
        tokio::task::spawn(async move {
            if let Err(e) = db.validate(tx).await {
                tx2.send(ValidateProgress::Abort(e.into())).await.unwrap();
            }
        });
        tokio_stream::wrappers::ReceiverStream::new(rx)
    }

    fn blob_add_from_path(
        self,
        msg: BlobAddPathRequest,
    ) -> impl Stream<Item = BlobAddPathResponse> {
        // provide a little buffer so that we don't slow down the sender
        let (tx, rx) = flume::bounded(32);
        let tx2 = tx.clone();
        self.rt().spawn_pinned(|| async move {
            if let Err(e) = self.blob_add_from_path0(msg, tx).await {
                tx2.send_async(AddProgress::Abort(e.into())).await.ok();
            }
        });
        rx.into_stream().map(BlobAddPathResponse)
    }

    fn doc_import_file(
        self,
        msg: DocImportFileRequest,
    ) -> impl Stream<Item = DocImportFileResponse> {
        // provide a little buffer so that we don't slow down the sender
        let (tx, rx) = flume::bounded(32);
        let tx2 = tx.clone();
        self.rt().spawn_pinned(|| async move {
            if let Err(e) = self.doc_import_file0(msg, tx).await {
                tx2.send_async(DocImportProgress::Abort(e.into()))
                    .await
                    .ok();
            }
        });
        rx.into_stream().map(DocImportFileResponse)
    }

    async fn doc_import_file0(
        self,
        msg: DocImportFileRequest,
        progress: flume::Sender<DocImportProgress>,
    ) -> anyhow::Result<()> {
        use iroh_bytes::store::ImportMode;
        use std::collections::BTreeMap;

        let progress = FlumeProgressSender::new(progress);
        let names = Arc::new(Mutex::new(BTreeMap::new()));
        // convert import progress to provide progress
        let import_progress = progress.clone().with_filter_map(move |x| match x {
            ImportProgress::Found { id, name } => {
                names.lock().unwrap().insert(id, name);
                None
            }
            ImportProgress::Size { id, size } => {
                let name = names.lock().unwrap().remove(&id)?;
                Some(DocImportProgress::Found { id, name, size })
            }
            ImportProgress::OutboardProgress { id, offset } => {
                Some(DocImportProgress::Progress { id, offset })
            }
            ImportProgress::OutboardDone { hash, id } => {
                Some(DocImportProgress::IngestDone { hash, id })
            }
            _ => None,
        });
        let DocImportFileRequest {
            doc_id,
            author_id,
            key,
            path: root,
            in_place,
        } = msg;
        // Check that the path is absolute and exists.
        anyhow::ensure!(root.is_absolute(), "path must be absolute");
        anyhow::ensure!(
            root.exists(),
            "trying to add missing path: {}",
            root.display()
        );

        let import_mode = match in_place {
            true => ImportMode::TryReference,
            false => ImportMode::Copy,
        };

        let (temp_tag, size) = self
            .inner
            .db
            .import_file(root, import_mode, BlobFormat::Raw, import_progress)
            .await?;

        let hash_and_format = temp_tag.inner();
        let HashAndFormat { hash, .. } = *hash_and_format;
        self.inner
            .sync
            .doc_set_hash(DocSetHashRequest {
                doc_id,
                author_id,
                key: key.clone(),
                hash,
                size,
            })
            .await?;
        drop(temp_tag);
        progress.send(DocImportProgress::AllDone { key }).await?;
        Ok(())
    }

    fn doc_export_file(
        self,
        msg: DocExportFileRequest,
    ) -> impl Stream<Item = DocExportFileResponse> {
        let (tx, rx) = flume::bounded(1024);
        let tx2 = tx.clone();
        self.rt().spawn_pinned(|| async move {
            if let Err(e) = self.doc_export_file0(msg, tx).await {
                tx2.send_async(DocExportProgress::Abort(e.into()))
                    .await
                    .ok();
            }
        });
        rx.into_stream().map(DocExportFileResponse)
    }

    async fn doc_export_file0(
        self,
        msg: DocExportFileRequest,
        progress: flume::Sender<DocExportProgress>,
    ) -> anyhow::Result<()> {
        let progress = FlumeProgressSender::new(progress);
        let DocExportFileRequest { entry, path } = msg;
        let key = bytes::Bytes::from(entry.key().to_vec());
        let export_progress = progress.clone().with_filter_map(move |x| match x {
            DownloadProgress::Export {
                id,
                hash,
                size,
                target,
            } => Some(DocExportProgress::Found {
                id,
                key: key.clone(),
                size,
                outpath: target,
                hash,
            }),
            DownloadProgress::ExportProgress { id, offset } => {
                Some(DocExportProgress::Progress { id, offset })
            }
            _ => None,
        });
        self.blob_export(path, entry.content_hash(), false, false, export_progress)
            .await?;
        progress.send(DocExportProgress::AllDone).await?;
        Ok(())
    }

    async fn blob_export(
        self,
        out: PathBuf,
        hash: Hash,
        recursive: bool,
        stable: bool,
        progress: impl ProgressSender<Msg = DownloadProgress> + IdGenerator,
    ) -> anyhow::Result<()> {
        let db = &self.inner.db;
        let path = PathBuf::from(&out);
        let mode = if stable {
            ExportMode::TryReference
        } else {
            ExportMode::Copy
        };
        if recursive {
            use crate::collection::{Blob, Collection};
            use crate::util::io::pathbuf_from_name;
            tokio::fs::create_dir_all(&path).await?;
            let collection = Collection::load(db, &hash).await?;
            for Blob { hash, name } in collection.blobs() {
                #[allow(clippy::needless_borrow)]
                let path = path.join(pathbuf_from_name(&name));
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                trace!("exporting blob {} to {}", hash, path.display());
                let id = progress.new_id();
                let progress1 = progress.clone();
                db.export(*hash, path, mode, move |offset| {
                    Ok(progress1.try_send(DownloadProgress::ExportProgress { id, offset })?)
                })
                .await?;
            }
        } else if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            let id = progress.new_id();
            let entry = db.get(&hash).context("entry not there")?;
            progress
                .send(DownloadProgress::Export {
                    id,
                    hash,
                    target: out,
                    size: entry.size(),
                })
                .await?;
            let progress1 = progress.clone();
            db.export(hash, path, mode, move |offset| {
                Ok(progress1.try_send(DownloadProgress::ExportProgress { id, offset })?)
            })
            .await?;
        }
        anyhow::Ok(())
    }

    async fn blob_download0(
        self,
        msg: BlobDownloadRequest,
        progress: impl ProgressSender<Msg = DownloadProgress> + IdGenerator,
    ) -> anyhow::Result<()> {
        let local = self.inner.rt.clone();
        let hash = msg.hash;
        let format = msg.format;
        let db = self.inner.db.clone();
        let haf = HashAndFormat { hash, format };
        let temp_pin = db.temp_tag(haf);
        let conn = self
            .inner
            .endpoint
            .connect(msg.peer, &iroh_bytes::protocol::ALPN)
            .await?;
        progress.send(DownloadProgress::Connected).await?;
        let progress2 = progress.clone();
        let progress3 = progress.clone();
        let db = self.inner.db.clone();
        let db2 = db.clone();
        let download = local.spawn_pinned(move || async move {
            crate::get::get(
                &db2,
                conn,
                &HashAndFormat {
                    hash: msg.hash,
                    format: msg.format,
                },
                progress2,
            )
            .await
        });

        let this = self.clone();
        let _export = local.spawn_pinned(move || async move {
            let stats = download.await.unwrap()?;
            progress
                .send(DownloadProgress::NetworkDone {
                    bytes_written: stats.bytes_written,
                    bytes_read: stats.bytes_read,
                    elapsed: stats.elapsed,
                })
                .await?;
            match msg.out {
                DownloadLocation::External { path, in_place } => {
                    if let Err(cause) = this
                        .blob_export(path, hash, msg.format.is_hash_seq(), in_place, progress3)
                        .await
                    {
                        progress.send(DownloadProgress::Abort(cause.into())).await?;
                    }
                }
                DownloadLocation::Internal => {
                    // nothing to do
                }
            }
            match msg.tag {
                SetTagOption::Named(tag) => {
                    db.set_tag(tag, Some(haf)).await?;
                }
                SetTagOption::Auto => {
                    db.create_tag(haf).await?;
                }
            }
            drop(temp_pin);
            progress.send(DownloadProgress::AllDone).await?;
            anyhow::Ok(())
        });
        Ok(())
    }

    fn blob_download(self, msg: BlobDownloadRequest) -> impl Stream<Item = DownloadProgress> {
        async move {
            let (sender, receiver) = flume::bounded(1024);
            let sender = FlumeProgressSender::new(sender);
            if let Err(cause) = self.blob_download0(msg, sender.clone()).await {
                sender
                    .send(DownloadProgress::Abort(cause.into()))
                    .await
                    .unwrap();
            };
            receiver.into_stream()
        }
        .flatten_stream()
    }

    async fn blob_add_from_path0(
        self,
        msg: BlobAddPathRequest,
        progress: flume::Sender<AddProgress>,
    ) -> anyhow::Result<()> {
        use crate::{
            collection::{Blob, Collection},
            rpc_protocol::WrapOption,
        };
        use futures::TryStreamExt;
        use iroh_bytes::store::ImportMode;
        use std::collections::BTreeMap;

        let progress = FlumeProgressSender::new(progress);
        let names = Arc::new(Mutex::new(BTreeMap::new()));
        // convert import progress to provide progress
        let import_progress = progress.clone().with_filter_map(move |x| match x {
            ImportProgress::Found { id, name } => {
                names.lock().unwrap().insert(id, name);
                None
            }
            ImportProgress::Size { id, size } => {
                let name = names.lock().unwrap().remove(&id)?;
                Some(AddProgress::Found { id, name, size })
            }
            ImportProgress::OutboardProgress { id, offset } => {
                Some(AddProgress::Progress { id, offset })
            }
            ImportProgress::OutboardDone { hash, id } => Some(AddProgress::Done { hash, id }),
            _ => None,
        });
        let BlobAddPathRequest {
            wrap,
            path: root,
            in_place,
            tag,
        } = msg;
        // Check that the path is absolute and exists.
        anyhow::ensure!(root.is_absolute(), "path must be absolute");
        anyhow::ensure!(
            root.exists(),
            "trying to add missing path: {}",
            root.display()
        );

        let import_mode = match in_place {
            true => ImportMode::TryReference,
            false => ImportMode::Copy,
        };

        let create_collection = match wrap {
            WrapOption::Wrap { .. } => true,
            WrapOption::NoWrap => root.is_dir(),
        };

        let temp_tag = if create_collection {
            // import all files below root recursively
            let data_sources = crate::util::fs::scan_path(root, wrap)?;
            const IO_PARALLELISM: usize = 4;
            let result: Vec<(Blob, u64, TempTag)> = futures::stream::iter(data_sources)
                .map(|source| {
                    let import_progress = import_progress.clone();
                    let db = self.inner.db.clone();
                    async move {
                        let name = source.name().to_string();
                        let (tag, size) = db
                            .import_file(
                                source.path().to_owned(),
                                import_mode,
                                BlobFormat::Raw,
                                import_progress,
                            )
                            .await?;
                        let hash = *tag.hash();
                        let blob = Blob { hash, name };
                        io::Result::Ok((blob, size, tag))
                    }
                })
                .buffered(IO_PARALLELISM)
                .try_collect::<Vec<_>>()
                .await?;
            let total_blobs_size = result.iter().map(|(_, size, _)| *size).sum();

            // create a collection
            let (blobs, _child_tags): (Vec<_>, Vec<_>) =
                result.into_iter().map(|(blob, _, tag)| (blob, tag)).unzip();
            let collection = Collection::new(blobs, total_blobs_size)?;

            collection.store(&self.inner.db).await?
        } else {
            // import a single file
            let (tag, _size) = self
                .inner
                .db
                .import_file(root, import_mode, BlobFormat::Raw, import_progress)
                .await?;
            tag
        };

        let hash_and_format = temp_tag.inner();
        let HashAndFormat { hash, format } = *hash_and_format;
        let tag = match tag {
            SetTagOption::Named(tag) => {
                self.inner
                    .db
                    .set_tag(tag.clone(), Some(*hash_and_format))
                    .await?;
                tag
            }
            SetTagOption::Auto => self.inner.db.create_tag(*hash_and_format).await?,
        };
        progress
            .send(AddProgress::AllDone {
                hash,
                format,
                tag: tag.clone(),
            })
            .await?;
        self.inner
            .callbacks
            .send(Event::ByteProvide(
                iroh_bytes::provider::Event::TaggedBlobAdded { hash, format, tag },
            ))
            .await;

        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn node_stats(self, _req: NodeStatsRequest) -> RpcResult<NodeStatsResponse> {
        #[cfg(feature = "metrics")]
        let res = Ok(NodeStatsResponse {
            stats: crate::metrics::get_metrics()?,
        });

        #[cfg(not(feature = "metrics"))]
        let res = Err(anyhow::anyhow!("metrics are disabled").into());

        res
    }

    async fn node_status(self, _: NodeStatusRequest) -> RpcResult<NodeStatusResponse> {
        Ok(NodeStatusResponse {
            addr: self.inner.endpoint.my_addr().await?,
            listen_addrs: self
                .inner
                .local_endpoint_addresses()
                .await
                .unwrap_or_default(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    #[allow(clippy::unused_async)]
    async fn node_shutdown(self, request: NodeShutdownRequest) {
        if request.force {
            info!("hard shutdown requested");
            std::process::exit(0);
        } else {
            // trigger a graceful shutdown
            info!("graceful shutdown requested");
            self.inner.cancel_token.cancel();
        }
    }

    fn node_watch(self, _: NodeWatchRequest) -> impl Stream<Item = NodeWatchResponse> {
        futures::stream::unfold((), |()| async move {
            tokio::time::sleep(HEALTH_POLL_WAIT).await;
            Some((
                NodeWatchResponse {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                (),
            ))
        })
    }

    fn blob_add_stream(
        self,
        msg: BlobAddStreamRequest,
        stream: impl Stream<Item = BlobAddStreamUpdate> + Send + Unpin + 'static,
    ) -> impl Stream<Item = BlobAddStreamResponse> {
        let (tx, rx) = flume::bounded(32);
        let this = self.clone();

        self.rt().spawn_pinned(|| async move {
            if let Err(err) = this.blob_add_stream0(msg, stream, tx.clone()).await {
                tx.send_async(AddProgress::Abort(err.into())).await.ok();
            }
        });

        rx.into_stream().map(BlobAddStreamResponse)
    }

    async fn blob_add_stream0(
        self,
        msg: BlobAddStreamRequest,
        stream: impl Stream<Item = BlobAddStreamUpdate> + Send + Unpin + 'static,
        progress: flume::Sender<AddProgress>,
    ) -> anyhow::Result<()> {
        let progress = FlumeProgressSender::new(progress);

        let stream = stream.map(|item| match item {
            BlobAddStreamUpdate::Chunk(chunk) => Ok(chunk),
            BlobAddStreamUpdate::Abort => {
                Err(io::Error::new(io::ErrorKind::Interrupted, "Remote abort"))
            }
        });

        let name_cache = Arc::new(Mutex::new(None));
        let import_progress = progress.clone().with_filter_map(move |x| match x {
            ImportProgress::Found { id: _, name } => {
                let _ = name_cache.lock().unwrap().insert(name);
                None
            }
            ImportProgress::Size { id, size } => {
                let name = name_cache.lock().unwrap().take()?;
                Some(AddProgress::Found { id, name, size })
            }
            ImportProgress::OutboardProgress { id, offset } => {
                Some(AddProgress::Progress { id, offset })
            }
            ImportProgress::OutboardDone { hash, id } => Some(AddProgress::Done { hash, id }),
            _ => None,
        });
        let (temp_tag, _len) = self
            .inner
            .db
            .import_stream(stream, BlobFormat::Raw, import_progress)
            .await?;
        let hash_and_format = *temp_tag.inner();
        let HashAndFormat { hash, format } = hash_and_format;
        let tag = match msg.tag {
            SetTagOption::Named(tag) => {
                self.inner
                    .db
                    .set_tag(tag.clone(), Some(hash_and_format))
                    .await?;
                tag
            }
            SetTagOption::Auto => self.inner.db.create_tag(hash_and_format).await?,
        };
        progress
            .send(AddProgress::AllDone { hash, tag, format })
            .await?;
        Ok(())
    }

    fn blob_read(
        self,
        req: BlobReadRequest,
    ) -> impl Stream<Item = RpcResult<BlobReadResponse>> + Send + 'static {
        let (tx, rx) = flume::bounded(RPC_BLOB_GET_CHANNEL_CAP);
        let entry = self.inner.db.get(&req.hash);
        self.inner.rt.spawn_pinned(move || async move {
            if let Err(err) = read_loop(entry, tx.clone(), RPC_BLOB_GET_CHUNK_SIZE).await {
                tx.send_async(RpcResult::Err(err.into())).await.ok();
            }
        });

        async fn read_loop<M: Map>(
            entry: Option<impl MapEntry<M>>,
            tx: flume::Sender<RpcResult<BlobReadResponse>>,
            chunk_size: usize,
        ) -> anyhow::Result<()> {
            let entry = entry.ok_or_else(|| anyhow!("Blob not found"))?;
            let size = entry.size();
            tx.send_async(Ok(BlobReadResponse::Entry {
                size,
                is_complete: entry.is_complete(),
            }))
            .await?;
            let mut reader = entry.data_reader().await?;
            let mut offset = 0u64;
            loop {
                let chunk = reader.read_at(offset, chunk_size).await?;
                let len = chunk.len();
                if !chunk.is_empty() {
                    tx.send_async(Ok(BlobReadResponse::Data { chunk })).await?;
                }
                if len < chunk_size {
                    break;
                } else {
                    offset += len as u64;
                }
            }
            Ok(())
        }

        rx.into_stream()
    }

    fn node_connections(
        self,
        _: NodeConnectionsRequest,
    ) -> impl Stream<Item = RpcResult<NodeConnectionsResponse>> + Send + 'static {
        // provide a little buffer so that we don't slow down the sender
        let (tx, rx) = flume::bounded(32);
        self.rt().spawn_pinned(|| async move {
            match self.inner.endpoint.connection_infos().await {
                Ok(mut conn_infos) => {
                    conn_infos.sort_by_key(|n| n.public_key.to_string());
                    for conn_info in conn_infos {
                        tx.send_async(Ok(NodeConnectionsResponse { conn_info }))
                            .await
                            .ok();
                    }
                }
                Err(e) => {
                    tx.send_async(Err(e.into())).await.ok();
                }
            }
        });
        rx.into_stream()
    }

    async fn node_connection_info(
        self,
        req: NodeConnectionInfoRequest,
    ) -> RpcResult<NodeConnectionInfoResponse> {
        let NodeConnectionInfoRequest { node_id } = req;
        let conn_info = self.inner.endpoint.connection_info(node_id).await?;
        Ok(NodeConnectionInfoResponse { conn_info })
    }
}

fn handle_rpc_request<D: BaoStore, E: ServiceEndpoint<ProviderService>>(
    msg: ProviderRequest,
    chan: RpcChannel<ProviderService, E>,
    handler: &RpcHandler<D>,
) {
    let handler = handler.clone();
    tokio::task::spawn(async move {
        use ProviderRequest::*;
        debug!("handling rpc request: {msg}");
        match msg {
            NodeWatch(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::node_watch)
                    .await
            }
            NodeStatus(msg) => chan.rpc(msg, handler, RpcHandler::node_status).await,
            NodeShutdown(msg) => chan.rpc(msg, handler, RpcHandler::node_shutdown).await,
            NodeStats(msg) => chan.rpc(msg, handler, RpcHandler::node_stats).await,
            NodeConnections(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::node_connections)
                    .await
            }
            NodeConnectionInfo(msg) => {
                chan.rpc(msg, handler, RpcHandler::node_connection_info)
                    .await
            }
            BlobList(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_list)
                    .await
            }
            BlobListIncomplete(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_list_incomplete)
                    .await
            }
            BlobListCollections(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_list_collections)
                    .await
            }
            ListTags(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_list_tags)
                    .await
            }
            DeleteTag(msg) => chan.rpc(msg, handler, RpcHandler::blob_delete_tag).await,
            BlobDeleteBlob(msg) => chan.rpc(msg, handler, RpcHandler::blob_delete_blob).await,
            BlobAddPath(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_add_from_path)
                    .await
            }
            BlobDownload(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_download)
                    .await
            }
            BlobValidate(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_validate)
                    .await
            }
            BlobRead(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::blob_read)
                    .await
            }
            BlobAddStream(msg) => {
                chan.bidi_streaming(msg, handler, RpcHandler::blob_add_stream)
                    .await
            }
            BlobAddStreamUpdate(_msg) => Err(RpcServerError::UnexpectedUpdateMessage),
            AuthorList(msg) => {
                chan.server_streaming(msg, handler, |handler, req| {
                    handler.inner.sync.author_list(req)
                })
                .await
            }
            AuthorCreate(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.author_create(req).await
                })
                .await
            }
            AuthorImport(_msg) => {
                todo!()
            }
            DocOpen(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_open(req).await
                })
                .await
            }
            DocClose(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_close(req).await
                })
                .await
            }
            DocStatus(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_status(req).await
                })
                .await
            }
            DocList(msg) => {
                chan.server_streaming(msg, handler, |handler, req| {
                    handler.inner.sync.doc_list(req)
                })
                .await
            }
            DocCreate(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_create(req).await
                })
                .await
            }
            DocDrop(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_drop(req).await
                })
                .await
            }
            DocImport(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_import(req).await
                })
                .await
            }
            DocSet(msg) => {
                let bao_store = handler.inner.db.clone();
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_set(&bao_store, req).await
                })
                .await
            }
            DocImportFile(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::doc_import_file)
                    .await
            }
            DocExportFile(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::doc_export_file)
                    .await
            }
            DocDel(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_del(req).await
                })
                .await
            }
            DocSetHash(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_set_hash(req).await
                })
                .await
            }
            DocGet(msg) => {
                chan.server_streaming(msg, handler, |handler, req| {
                    handler.inner.sync.doc_get_many(req)
                })
                .await
            }
            DocGetExact(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_get_exact(req).await
                })
                .await
            }
            DocStartSync(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_start_sync(req).await
                })
                .await
            }
            DocLeave(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_leave(req).await
                })
                .await
            }
            DocShare(msg) => {
                chan.rpc(msg, handler, |handler, req| async move {
                    handler.inner.sync.doc_share(req).await
                })
                .await
            }
            DocSubscribe(msg) => {
                chan.server_streaming(msg, handler, |handler, req| {
                    async move { handler.inner.sync.doc_subscribe(req) }.flatten_stream()
                })
                .await
            }
        }
    });
}

/// Create a [`quinn::ServerConfig`] with the given secret key and limits.
pub fn make_server_config(
    secret_key: &SecretKey,
    max_streams: u64,
    max_connections: u32,
    alpn_protocols: Vec<Vec<u8>>,
) -> anyhow::Result<quinn::ServerConfig> {
    let tls_server_config = tls::make_server_config(secret_key, alpn_protocols, false)?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
    let mut transport_config = quinn::TransportConfig::default();
    transport_config
        .max_concurrent_bidi_streams(max_streams.try_into()?)
        .max_concurrent_uni_streams(0u32.into());

    server_config
        .transport_config(Arc::new(transport_config))
        .concurrent_connections(max_connections);
    Ok(server_config)
}

#[cfg(all(test, feature = "flat-db"))]
mod tests {
    use anyhow::bail;
    use futures::StreamExt;
    use std::path::Path;

    use crate::rpc_protocol::WrapOption;

    use super::*;

    #[tokio::test]
    async fn test_ticket_multiple_addrs() {
        let _guard = iroh_test::logging::setup();

        let lp = LocalPoolHandle::new(1);
        let (db, hashes) = iroh_bytes::store::readonly_mem::Store::new([("test", b"hello")]);
        let doc_store = iroh_sync::store::memory::Store::default();
        let hash = hashes["test"].into();
        let node = Node::builder(db, doc_store)
            .bind_port(0)
            .local_pool(&lp)
            .spawn()
            .await
            .unwrap();
        let _drop_guard = node.cancel_token().drop_guard();
        let ticket = node.ticket(hash, BlobFormat::Raw).await.unwrap();
        println!("addrs: {:?}", ticket.node_addr().info);
        assert!(!ticket.node_addr().info.direct_addresses.is_empty());
    }

    #[tokio::test]
    async fn test_node_add_blob_stream() -> Result<()> {
        let _guard = iroh_test::logging::setup();

        use std::io::Cursor;
        let db = iroh_bytes::store::mem::Store::new();
        let doc_store = iroh_sync::store::memory::Store::default();
        let node = Node::builder(db, doc_store)
            .bind_port(0)
            .local_pool(&LocalPoolHandle::new(1))
            .spawn()
            .await?;

        let _drop_guard = node.cancel_token().drop_guard();
        let client = node.client();
        let input = vec![2u8; 1024 * 256]; // 265kb so actually streaming, chunk size is 64kb
        let reader = Cursor::new(input.clone());
        let progress = client.blobs.add_reader(reader, SetTagOption::Auto).await?;
        let outcome = progress.finish().await?;
        let hash = outcome.hash;
        let output = client.blobs.read_to_bytes(hash).await?;
        assert_eq!(input, output.to_vec());
        Ok(())
    }

    #[tokio::test]
    async fn test_node_add_tagged_blob_event() -> Result<()> {
        let _guard = iroh_test::logging::setup();

        let db = iroh_bytes::store::mem::Store::new();
        let doc_store = iroh_sync::store::memory::Store::default();
        let node = Node::builder(db, doc_store).bind_port(0).spawn().await?;

        let _drop_guard = node.cancel_token().drop_guard();

        let (r, mut s) = mpsc::channel(1);
        node.subscribe(move |event| {
            let r = r.clone();
            async move {
                if let Event::ByteProvide(iroh_bytes::provider::Event::TaggedBlobAdded {
                    hash,
                    ..
                }) = event
                {
                    r.send(hash).await.ok();
                }
            }
            .boxed()
        })
        .await?;

        let got_hash = tokio::time::timeout(Duration::from_secs(1), async move {
            let mut stream = node
                .controller()
                .server_streaming(BlobAddPathRequest {
                    path: Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"),
                    in_place: false,
                    tag: SetTagOption::Auto,
                    wrap: WrapOption::NoWrap,
                })
                .await?;

            while let Some(item) = stream.next().await {
                let BlobAddPathResponse(progress) = item?;
                match progress {
                    AddProgress::AllDone { hash, .. } => {
                        return Ok(hash);
                    }
                    AddProgress::Abort(e) => {
                        bail!("Error while adding data: {e}");
                    }
                    _ => {}
                }
            }
            bail!("stream ended without providing data");
        })
        .await
        .context("timeout")?
        .context("get failed")?;

        let event_hash = s.recv().await.expect("missing add tagged blob event");
        assert_eq!(got_hash, event_hash);

        Ok(())
    }
}
