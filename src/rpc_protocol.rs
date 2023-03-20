#![allow(missing_docs)]
use std::{net::SocketAddr, path::PathBuf};

use crate::{protocol::AuthToken, util::RpcResult, Hash, PeerId};
use derive_more::{From, TryInto};
use quic_rpc::{
    message::{Msg, RpcMsg, ServerStreaming, ServerStreamingMsg},
    Service,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvideRequest {
    pub path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvideResponse {
    pub hash: Hash,
    pub entries: Vec<ProvideResponseEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvideResponseEntry {
    pub name: String,
    pub hash: Hash,
    pub size: u64,
}

impl RpcMsg<ProviderService> for ProvideRequest {
    type Response = RpcResult<ProvideResponse>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateRequest;

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateResponse {
    pub hash: Hash,
    pub path: Option<PathBuf>,
    pub size: u64,
    pub error: Option<String>,
}

impl Msg<ProviderService> for ValidateRequest {
    type Pattern = ServerStreaming;
}

impl ServerStreamingMsg<ProviderService> for ValidateRequest {
    type Response = ValidateResponse;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListRequest;

#[derive(Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub path: PathBuf,
    pub hash: Hash,
    pub size: u64,
}

impl Msg<ProviderService> for ListRequest {
    type Pattern = ServerStreaming;
}

impl ServerStreamingMsg<ProviderService> for ListRequest {
    type Response = ListResponse;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WatchRequest;

#[derive(Serialize, Deserialize, Debug)]
pub struct VersionRequest;

impl RpcMsg<ProviderService> for VersionRequest {
    type Response = VersionResponse;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ShutdownRequest {
    pub force: bool,
}

impl RpcMsg<ProviderService> for ShutdownRequest {
    type Response = ();
}

#[derive(Serialize, Deserialize, Debug)]
pub struct IdRequest;

impl RpcMsg<ProviderService> for IdRequest {
    type Response = IdResponse;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WatchResponse {
    pub version: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct IdResponse {
    pub peer_id: Box<PeerId>,
    pub auth_token: Box<AuthToken>,
    pub listen_addr: Box<SocketAddr>,
    pub version: String,
}

impl Msg<ProviderService> for WatchRequest {
    type Pattern = ServerStreaming;
}

impl ServerStreamingMsg<ProviderService> for WatchRequest {
    type Response = WatchResponse;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VersionResponse {
    pub version: String,
}

/// The RPC service for the iroh provider process.
#[derive(Debug, Clone)]
pub struct ProviderService;

/// Request enum
#[derive(Debug, Serialize, Deserialize, From, TryInto)]
pub enum ProviderRequest {
    Watch(WatchRequest),
    Version(VersionRequest),
    List(ListRequest),
    Provide(ProvideRequest),
    Id(IdRequest),
    Shutdown(ShutdownRequest),
    Validate(ValidateRequest),
}

/// Response enum
#[derive(Debug, Serialize, Deserialize, From, TryInto)]
pub enum ProviderResponse {
    Watch(WatchResponse),
    Version(VersionResponse),
    List(ListResponse),
    Provide(RpcResult<ProvideResponse>),
    Id(IdResponse),
    Validate(ValidateResponse),
    Shutdown(()),
}

impl Service for ProviderService {
    type Req = ProviderRequest;
    type Res = ProviderResponse;
}
