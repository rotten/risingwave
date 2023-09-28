// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Wrapper gRPC clients, which help constructing the request and destructing the
//! response gRPC message structs.

#![feature(trait_alias)]
#![feature(binary_heap_drain_sorted)]
#![feature(result_option_inspect)]
#![feature(type_alias_impl_trait)]
#![feature(associated_type_defaults)]
#![feature(generators)]
#![feature(iterator_try_collect)]
#![feature(hash_extract_if)]
#![feature(try_blocks)]
#![feature(let_chains)]
#![feature(impl_trait_in_assoc_type)]

use std::any::type_name;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::iter::repeat;
use std::pin::pin;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use futures::future::{select, try_join_all, Either};
use futures::stream::{BoxStream, Peekable};
use futures::{Stream, StreamExt};
use moka::future::Cache;
use rand::prelude::SliceRandom;
use risingwave_common::util::addr::HostAddr;
use risingwave_pb::common::WorkerNode;
use risingwave_pb::meta::heartbeat_request::extra_info;
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

pub mod error;
use error::{Result, RpcError};
mod compactor_client;
mod compute_client;
mod connector_client;
mod hummock_meta_client;
mod meta_client;
mod sink_coordinate_client;
mod stream_client;
mod tracing;

use std::pin::Pin;

pub use compactor_client::{CompactorClient, GrpcCompactorProxyClient};
pub use compute_client::{ComputeClient, ComputeClientPool, ComputeClientPoolRef};
pub use connector_client::{ConnectorClient, SinkCoordinatorStreamHandle, SinkWriterStreamHandle};
pub use hummock_meta_client::{CompactionEventItem, HummockMetaClient};
pub use meta_client::{MetaClient, SinkCoordinationRpcClient};
pub use sink_coordinate_client::CoordinatorStreamHandle;
pub use stream_client::{StreamClient, StreamClientPool, StreamClientPoolRef};

#[async_trait]
pub trait RpcClient: Send + Sync + 'static + Clone {
    async fn new_client(host_addr: HostAddr) -> Result<Self>;

    async fn new_clients(host_addr: HostAddr, size: usize) -> Result<Vec<Self>> {
        try_join_all(repeat(host_addr).take(size).map(Self::new_client)).await
    }
}

#[derive(Clone)]
pub struct RpcClientPool<S> {
    connection_pool_size: u16,

    clients: Cache<HostAddr, Vec<S>>,
}

impl<S> Default for RpcClientPool<S>
where
    S: RpcClient,
{
    fn default() -> Self {
        Self::new(1)
    }
}

impl<S> RpcClientPool<S>
where
    S: RpcClient,
{
    pub fn new(connection_pool_size: u16) -> Self {
        Self {
            connection_pool_size,
            clients: Cache::new(u64::MAX),
        }
    }

    /// Gets the RPC client for the given node. If the connection is not established, a
    /// new client will be created and returned.
    pub async fn get(&self, node: &WorkerNode) -> Result<S> {
        let addr: HostAddr = node.get_host().unwrap().into();
        self.get_by_addr(addr).await
    }

    /// Gets the RPC client for the given addr. If the connection is not established, a
    /// new client will be created and returned.
    pub async fn get_by_addr(&self, addr: HostAddr) -> Result<S> {
        Ok(self
            .clients
            .try_get_with(
                addr.clone(),
                S::new_clients(addr.clone(), self.connection_pool_size as usize),
            )
            .await
            .map_err(|e| -> RpcError {
                anyhow!("failed to create RPC client to {addr}: {:?}", e).into()
            })?
            .choose(&mut rand::thread_rng())
            .unwrap()
            .clone())
    }
}

/// `ExtraInfoSource` is used by heartbeat worker to pull extra info that needs to be piggybacked.
#[async_trait::async_trait]
pub trait ExtraInfoSource: Send + Sync {
    /// None means the info is not available at the moment.
    async fn get_extra_info(&self) -> Option<extra_info::Info>;
}

pub type ExtraInfoSourceRef = Arc<dyn ExtraInfoSource>;

#[macro_export]
macro_rules! rpc_client_method_impl {
    ($( { $client:tt, $fn_name:ident, $req:ty, $resp:ty }),*) => {
        $(
            pub async fn $fn_name(&self, request: $req) -> $crate::Result<$resp> {
                Ok(self
                    .$client
                    .to_owned()
                    .$fn_name(request)
                    .await?
                    .into_inner())
            }
        )*
    }
}

#[macro_export]
macro_rules! meta_rpc_client_method_impl {
    ($( { $client:tt, $fn_name:ident, $req:ty, $resp:ty }),*) => {
        $(
            pub async fn $fn_name(&self, request: $req) -> $crate::Result<$resp> {
                let mut client = self.core.read().await.$client.to_owned();
                match client.$fn_name(request).await {
                    Ok(resp) => Ok(resp.into_inner()),
                    Err(e) => {
                        self.refresh_client_if_needed(e.code()).await;
                        Err(RpcError::from(e))
                    }
                }
            }
        )*
    }
}

pub struct BidiStreamHandle<REQ: 'static, RSP: 'static> {
    request_sender: Sender<REQ>,
    response_stream: Peekable<BoxStream<'static, std::result::Result<RSP, Status>>>,
}

impl<REQ: 'static, RSP: 'static> Debug for BidiStreamHandle<REQ, RSP> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(type_name::<Self>())
    }
}

impl<REQ: 'static, RSP: 'static> BidiStreamHandle<REQ, RSP> {
    pub fn for_test(
        request_sender: Sender<REQ>,
        response_stream: BoxStream<'static, std::result::Result<RSP, Status>>,
    ) -> Self {
        Self {
            request_sender,
            response_stream: response_stream.peekable(),
        }
    }

    pub async fn initialize<
        F: FnOnce(Request<ReceiverStream<REQ>>) -> Fut,
        St: Stream<Item = std::result::Result<RSP, Status>> + Send + Unpin + 'static,
        Fut: Future<Output = std::result::Result<Response<St>, Status>> + Send,
    >(
        first_request: REQ,
        init_stream_fn: F,
    ) -> Result<(Self, RSP)> {
        const SINK_WRITER_REQUEST_BUFFER_SIZE: usize = 16;
        let (request_sender, request_receiver) = channel(SINK_WRITER_REQUEST_BUFFER_SIZE);

        // Send initial request in case of the blocking receive call from creating streaming request
        request_sender
            .send(first_request)
            .await
            .map_err(|err| anyhow!(err.to_string()))?;

        let mut response_stream =
            init_stream_fn(Request::new(ReceiverStream::new(request_receiver)))
                .await?
                .into_inner();

        let first_response = response_stream
            .next()
            .await
            .ok_or_else(|| anyhow!("get empty response from start sink request"))??;

        Ok((
            Self {
                request_sender,
                response_stream: response_stream.boxed().peekable(),
            },
            first_response,
        ))
    }

    pub async fn next_response(&mut self) -> Result<RSP> {
        Ok(self
            .response_stream
            .next()
            .await
            .ok_or_else(|| anyhow!("end of response stream"))??)
    }

    pub async fn send_request(&mut self, request: REQ) -> Result<()> {
        // Poll the response stream to early see the error
        let send_request_result = match select(
            pin!(self.request_sender.send(request)),
            pin!(Pin::new(&mut self.response_stream).peek()),
        )
        .await
        {
            Either::Left((result, _)) => result,
            Either::Right((response_result, send_future)) => match response_result {
                None => {
                    return Err(anyhow!("end of response stream").into());
                }
                Some(Err(e)) => {
                    return Err(e.clone().into());
                }
                Some(Ok(_)) => send_future.await,
            },
        };
        send_request_result
            .map_err(|_| anyhow!("unable to send request {}", type_name::<REQ>()).into())
    }
}
