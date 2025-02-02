mod pb {
    tonic::include_proto!("apibara.application.v1alpha3");
}

use std::{pin::Pin, sync::Arc};

use anyhow::Error;
use futures::Stream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error};

use crate::{
    build_info,
    chain::{
        Address, BlockHash, BlockHeader, EthereumEvent, Event, EventFilter, StarkNetEvent,
        TopicValue,
    },
    indexer::{
        ClientToIndexerMessage as IndexerClientMessage, IndexerManager, IndexerPersistence,
        Message as IndexerMessage, Network, State as IndexerState,
    },
    network_manager::NetworkManager,
    persistence::Id,
};

use self::pb::{
    connect_indexer_request::Message as ConnectIndexerRequestMessage,
    connect_indexer_response::Message as ConnectIndexerResponseMessage,
    indexer_manager_server::{IndexerManager as IndexerManagerTrait, IndexerManagerServer},
    ConnectIndexerRequest, ConnectIndexerResponse, CreateIndexerRequest, CreateIndexerResponse,
    DeleteIndexerRequest, DeleteIndexerResponse, GetIndexerRequest, GetIndexerResponse,
    IndexerConnected, ListIndexerRequest, ListIndexerResponse, NewBlock, NewEvents, Reorg, Version,
};

pub struct IndexerManagerService<IP: IndexerPersistence> {
    indexer_manager: IndexerManager<IP>,
}

type TonicResult<T> = Result<Response<T>, Status>;

impl<IP: IndexerPersistence> IndexerManagerService<IP> {
    pub fn new(network_manager: Arc<NetworkManager>, indexer_persistence: Arc<IP>) -> Self {
        let indexer_manager = IndexerManager::new(network_manager, indexer_persistence);

        IndexerManagerService { indexer_manager }
    }

    pub fn into_service(self) -> IndexerManagerServer<IndexerManagerService<IP>> {
        IndexerManagerServer::new(self)
    }
}

#[tonic::async_trait]
impl<IP: IndexerPersistence> IndexerManagerTrait for IndexerManagerService<IP> {
    async fn create_indexer(
        &self,
        request: Request<CreateIndexerRequest>,
    ) -> TonicResult<CreateIndexerResponse> {
        let message: CreateIndexerRequest = request.into_inner();
        debug!("create indexer: {:?}", message);
        let id: Id = message.id.parse().map_err(RequestError)?;
        let network_name = message.network_name.parse().map_err(RequestError)?;

        let filters = message.filters.into_iter().map(Into::into).collect();

        let res = self
            .indexer_manager
            .create_indexer(&id, network_name, filters, message.index_from_block)
            .await
            .map_err(RequestError);

        match res {
            Err(err) => {
                error!("create indexer error: {:?}", err);
                return Err(err)?;
            }
            Ok(indexer) => {
                let response = CreateIndexerResponse {
                    indexer: Some(indexer.into()),
                };

                Ok(Response::new(response))
            }
        }
    }

    async fn get_indexer(
        &self,
        request: Request<GetIndexerRequest>,
    ) -> TonicResult<GetIndexerResponse> {
        let message: GetIndexerRequest = request.into_inner();
        debug!("get indexer: {:?}", message);
        let id: Id = message.id.parse().map_err(RequestError)?;

        let res = self
            .indexer_manager
            .get_indexer(&id)
            .await
            .map_err(RequestError)
            .map(|o| o.map(Into::into));

        match res {
            Err(err) => {
                error!("get indexer error: {:?}", err);
                return Err(err)?;
            }
            Ok(indexer) => {
                let response = GetIndexerResponse { indexer };
                Ok(Response::new(response))
            }
        }
    }

    async fn delete_indexer(
        &self,
        request: Request<DeleteIndexerRequest>,
    ) -> TonicResult<DeleteIndexerResponse> {
        let message: DeleteIndexerRequest = request.into_inner();
        debug!("delete indexer: {:?}", message);
        let id: Id = message.id.parse().map_err(RequestError)?;

        let res = self
            .indexer_manager
            .delete_indexer(&id)
            .await
            .map_err(RequestError)
            .map(Into::into);

        match res {
            Err(err) => {
                error!("delete indexer error: {:?}", err);
                return Err(err)?;
            }
            Ok(indexer) => {
                let response = DeleteIndexerResponse {
                    indexer: Some(indexer),
                };
                Ok(Response::new(response))
            }
        }
    }

    async fn list_indexer(
        &self,
        request: Request<ListIndexerRequest>,
    ) -> TonicResult<ListIndexerResponse> {
        let _message: ListIndexerRequest = request.into_inner();
        debug!("list indexer");
        let res = self
            .indexer_manager
            .list_indexer()
            .await
            .map_err(RequestError)
            .map(|indexers| indexers.into_iter().map(Into::into).collect());

        match res {
            Err(err) => {
                error!("list indexer error: {:?}", err);
                return Err(err)?;
            }
            Ok(indexers) => {
                let response = ListIndexerResponse { indexers };
                Ok(Response::new(response))
            }
        }
    }

    type ConnectIndexerStream =
        Pin<Box<dyn Stream<Item = Result<ConnectIndexerResponse, Status>> + Send + 'static>>;

    async fn connect_indexer(
        &self,
        request: Request<Streaming<ConnectIndexerRequest>>,
    ) -> TonicResult<Self::ConnectIndexerStream> {
        debug!("connect indexer");

        let request_stream = request.into_inner().map(|m| match m {
            Ok(m) => m.try_into(),
            Err(err) => Err(Error::msg(err.to_string())),
        });

        let res = self
            .indexer_manager
            .connect_indexer(Box::pin(request_stream))
            .await
            .map_err(RequestError);

        match res {
            Err(err) => {
                error!("connect indexer error: {:?}", err);
                return Err(err)?;
            }
            Ok(indexer_stream) => {
                let response_stream: Pin<
                    Box<dyn Stream<Item = Result<ConnectIndexerResponse, Status>> + Send>,
                > = Box::pin(indexer_stream.map(|m| match m {
                    Err(err) => Err(RequestError(err))?,
                    Ok(message) => {
                        let message = message.into();
                        let response = ConnectIndexerResponse {
                            message: Some(message),
                        };
                        Ok(response)
                    }
                }));

                let response = Response::new(response_stream);

                Ok(response)
            }
        }
    }
}

#[derive(Debug)]
struct RequestError(anyhow::Error);

impl From<RequestError> for Status {
    fn from(err: RequestError) -> Self {
        Status::internal(err.0.to_string())
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::Network> for Network {
    fn into(self) -> pb::Network {
        let network = match self {
            Network::StarkNet(net) => {
                let inner = pb::StarkNetNetwork {
                    name: net.name.to_string(),
                };
                pb::network::Network::Starknet(inner)
            }
            Network::Ethereum(net) => {
                let inner = pb::EthereumNetwork {
                    name: net.name.to_string(),
                };
                pb::network::Network::Ethereum(inner)
            }
        };
        pb::Network {
            network: Some(network),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::Indexer> for IndexerState {
    fn into(self) -> pb::Indexer {
        pb::Indexer {
            id: self.id.into_string(),
            network: Some(self.network.into()),
            indexed_to_block: self.indexed_to_block,
            index_from_block: self.index_from_block,
            filters: self.filters.into_iter().map(Into::into).collect(),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::EventFilter> for EventFilter {
    fn into(self) -> pb::EventFilter {
        let address = self.address.map(|a| a.to_vec()).unwrap_or_default();
        let signature = self.signature;
        pb::EventFilter { address, signature }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::BlockHeader> for BlockHeader {
    fn into(self) -> pb::BlockHeader {
        let ts_seconds = self.timestamp.timestamp();
        let timestamp = prost_types::Timestamp {
            seconds: ts_seconds,
            nanos: 0,
        };
        pb::BlockHeader {
            number: self.number,
            hash: self.hash.to_vec(),
            parent_hash: self.parent_hash.as_ref().map(BlockHash::to_vec),
            timestamp: Some(timestamp),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::Event> for Event {
    fn into(self) -> pb::Event {
        let event = match self {
            Event::StarkNet(starknet) => {
                let inner = starknet.into();
                pb::event::Event::Starknet(inner)
            }
            Event::Ethereum(ethereum) => {
                let inner = ethereum.into();
                pb::event::Event::Ethereum(inner)
            }
        };
        pb::Event { event: Some(event) }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::StarkNetEvent> for StarkNetEvent {
    fn into(self) -> pb::StarkNetEvent {
        let data = self.data.into_iter().map(Into::into).collect();
        let topics = self.topics.into_iter().map(Into::into).collect();
        pb::StarkNetEvent {
            address: self.address.to_vec(),
            log_index: self.log_index as u64,
            transaction_hash: self.transaction_hash.to_vec(),
            data,
            topics,
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::EthereumEvent> for EthereumEvent {
    fn into(self) -> pb::EthereumEvent {
        let topics = self.topics.into_iter().map(Into::into).collect();

        pb::EthereumEvent {
            address: self.address.to_vec(),
            log_index: self.log_index as u64,
            transaction_hash: self.transaction_hash.to_vec(),
            topics,
            data: self.data,
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<pb::TopicValue> for TopicValue {
    fn into(self) -> pb::TopicValue {
        pb::TopicValue {
            value: self.to_vec(),
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<ConnectIndexerResponseMessage> for IndexerMessage {
    fn into(self) -> ConnectIndexerResponseMessage {
        match self {
            IndexerMessage::Connected(state) => {
                // the version is controlled by the crate and so it cannot
                // fail parsing.
                let version = semver::Version::parse(build_info::PKG_VERSION)
                    .expect("failed to parse crate version");
                let version = Version {
                    major: version.major,
                    minor: version.minor,
                    patch: version.patch,
                };
                let connected = IndexerConnected {
                    indexer: Some(state.into()),
                    version: Some(version),
                };
                ConnectIndexerResponseMessage::Connected(connected)
            }
            IndexerMessage::NewBlock(new_head) => {
                let new_block = NewBlock {
                    new_head: Some(new_head.into()),
                };
                ConnectIndexerResponseMessage::NewBlock(new_block)
            }
            IndexerMessage::Reorg(new_head) => {
                let reorg = Reorg {
                    new_head: Some(new_head.into()),
                };
                ConnectIndexerResponseMessage::Reorg(reorg)
            }
            IndexerMessage::NewEvents(block_events) => {
                let block = block_events.block.into();
                let events = block_events.events.into_iter().map(Into::into).collect();
                let new_events = NewEvents {
                    block: Some(block),
                    events,
                };
                ConnectIndexerResponseMessage::NewEvents(new_events)
            }
        }
    }
}

impl TryFrom<ConnectIndexerRequest> for IndexerClientMessage {
    type Error = anyhow::Error;

    fn try_from(request: ConnectIndexerRequest) -> Result<Self, Self::Error> {
        match request.message {
            None => Err(Error::msg("missing required field message")),
            Some(ConnectIndexerRequestMessage::Connect(connect)) => {
                let id = connect.id.parse()?;
                Ok(IndexerClientMessage::Connect(id))
            }
            Some(ConnectIndexerRequestMessage::Ack(ack)) => {
                let hash = BlockHash::from_bytes(&ack.hash);
                Ok(IndexerClientMessage::AckBlock(hash))
            }
        }
    }
}

impl From<pb::EventFilter> for EventFilter {
    fn from(ef: pb::EventFilter) -> Self {
        let address = if ef.address.is_empty() {
            None
        } else {
            Some(Address::from_bytes(&ef.address))
        };
        let signature = ef.signature;
        EventFilter { address, signature }
    }
}

impl From<pb::TopicValue> for TopicValue {
    fn from(tv: pb::TopicValue) -> Self {
        TopicValue::from_bytes(&tv.value)
    }
}
