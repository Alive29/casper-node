//! JSON-RPC server
//!
//! The JSON-RPC server provides clients with an API for querying state and
//! sending commands to the node.
//!
//! The actual server is run in backgrounded tasks. RPCs requests are translated into reactor
//! requests to various components.
//!
//! This module currently provides both halves of what is required for an API server:
//! a component implementation that interfaces with other components via being plugged into a
//! reactor, and an external facing http server that exposes various uri routes and converts
//! JSON-RPC requests into the appropriate component events.
//!
//! For the list of supported RPC methods, see:
//! https://github.com/CasperLabs/ceps/blob/master/text/0009-client-api.md#rpcs

mod config;
mod event;
mod http_server;
pub mod rpcs;

use std::{convert::Infallible, fmt::Debug};

use datasize::DataSize;
use futures::join;

use casper_execution_engine::{
    core::engine_state::{
        self, BalanceRequest, BalanceResult, GetEraValidatorsError, QueryRequest, QueryResult,
    },
    storage::protocol_data::ProtocolData,
};
use casper_types::{auction::EraValidators, Key, ProtocolVersion, URef};

use self::rpcs::chain::BlockIdentifier;

use super::Component;
use crate::{
    components::{contract_runtime::EraValidatorsRequest, storage::Storage},
    crypto::hash::Digest,
    effect::{
        announcements::RpcServerAnnouncement,
        requests::{
            ChainspecLoaderRequest, ContractRuntimeRequest, LinearChainRequest, MetricsRequest,
            NetworkInfoRequest, RpcRequest, StorageRequest,
        },
        EffectBuilder, EffectExt, Effects, Responder,
    },
    types::{NodeId, StatusFeed},
    NodeRng,
};

pub use config::Config;
pub(crate) use event::Event;

/// A helper trait capturing all of this components Request type dependencies.
pub trait ReactorEventT:
    From<Event>
    + From<RpcRequest<NodeId>>
    + From<RpcServerAnnouncement>
    + From<ChainspecLoaderRequest>
    + From<ContractRuntimeRequest>
    + From<LinearChainRequest<NodeId>>
    + From<MetricsRequest>
    + From<NetworkInfoRequest<NodeId>>
    + From<StorageRequest<Storage>>
    + Send
{
}

impl<REv> ReactorEventT for REv where
    REv: From<Event>
        + From<RpcRequest<NodeId>>
        + From<RpcServerAnnouncement>
        + From<ChainspecLoaderRequest>
        + From<ContractRuntimeRequest>
        + From<LinearChainRequest<NodeId>>
        + From<MetricsRequest>
        + From<NetworkInfoRequest<NodeId>>
        + From<StorageRequest<Storage>>
        + Send
        + 'static
{
}

#[derive(DataSize, Debug)]
pub(crate) struct RpcServer {}

impl RpcServer {
    pub(crate) fn new<REv>(config: Config, effect_builder: EffectBuilder<REv>) -> Self
    where
        REv: ReactorEventT,
    {
        tokio::spawn(http_server::run(config, effect_builder));

        RpcServer {}
    }
}

impl RpcServer {
    fn handle_protocol_data<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        protocol_version: ProtocolVersion,
        responder: Responder<Result<Option<Box<ProtocolData>>, engine_state::Error>>,
    ) -> Effects<Event> {
        effect_builder
            .get_protocol_data(protocol_version)
            .event(move |result| Event::QueryProtocolDataResult {
                result,
                main_responder: responder,
            })
    }

    fn handle_query<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        state_root_hash: Digest,
        base_key: Key,
        path: Vec<String>,
        responder: Responder<Result<QueryResult, engine_state::Error>>,
    ) -> Effects<Event> {
        let query = QueryRequest::new(state_root_hash.into(), base_key, path);
        effect_builder
            .query_global_state(query)
            .event(move |result| Event::QueryGlobalStateResult {
                result,
                main_responder: responder,
            })
    }

    fn handle_era_validators<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        state_root_hash: Digest,
        protocol_version: ProtocolVersion,
        responder: Responder<Result<EraValidators, GetEraValidatorsError>>,
    ) -> Effects<Event> {
        let request = EraValidatorsRequest::new(state_root_hash.into(), protocol_version);
        effect_builder
            .get_era_validators(request)
            .event(move |result| Event::QueryEraValidatorsResult {
                result,
                main_responder: responder,
            })
    }

    fn handle_get_balance<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        state_root_hash: Digest,
        purse_uref: URef,
        responder: Responder<Result<BalanceResult, engine_state::Error>>,
    ) -> Effects<Event> {
        let query = BalanceRequest::new(state_root_hash.into(), purse_uref);
        effect_builder
            .get_balance(query)
            .event(move |result| Event::GetBalanceResult {
                result,
                main_responder: responder,
            })
    }
}

impl<REv> Component<REv> for RpcServer
where
    REv: ReactorEventT,
{
    type Event = Event;
    type ConstructionError = Infallible;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut NodeRng,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        match event {
            Event::RpcRequest(RpcRequest::SubmitDeploy { deploy, responder }) => {
                let mut effects = effect_builder.announce_deploy_received(deploy).ignore();
                effects.extend(responder.respond(()).ignore());
                effects
            }
            Event::RpcRequest(RpcRequest::GetBlock {
                maybe_id: Some(BlockIdentifier::Hash(hash)),
                responder,
            }) => effect_builder
                .get_block_from_storage(hash)
                .event(move |result| Event::GetBlockResult {
                    maybe_id: Some(BlockIdentifier::Hash(hash)),
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetBlock {
                maybe_id: Some(BlockIdentifier::Height(height)),
                responder,
            }) => effect_builder
                .get_block_at_height(height)
                .event(move |result| Event::GetBlockResult {
                    maybe_id: Some(BlockIdentifier::Height(height)),
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetBlock {
                maybe_id: None,
                responder,
            }) => effect_builder
                .get_highest_block()
                .event(move |result| Event::GetBlockResult {
                    maybe_id: None,
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::QueryProtocolData {
                protocol_version,
                responder,
            }) => self.handle_protocol_data(effect_builder, protocol_version, responder),
            Event::RpcRequest(RpcRequest::QueryGlobalState {
                state_root_hash,
                base_key,
                path,
                responder,
            }) => self.handle_query(effect_builder, state_root_hash, base_key, path, responder),
            Event::RpcRequest(RpcRequest::QueryEraValidators {
                state_root_hash,
                protocol_version,
                responder,
            }) => self.handle_era_validators(
                effect_builder,
                state_root_hash,
                protocol_version,
                responder,
            ),
            Event::RpcRequest(RpcRequest::GetBalance {
                state_root_hash,
                purse_uref,
                responder,
            }) => self.handle_get_balance(effect_builder, state_root_hash, purse_uref, responder),
            Event::RpcRequest(RpcRequest::GetDeploy { hash, responder }) => effect_builder
                .get_deploy_and_metadata_from_storage(hash)
                .event(move |result| Event::GetDeployResult {
                    hash,
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetPeers { responder }) => effect_builder
                .network_peers()
                .event(move |peers| Event::GetPeersResult {
                    peers,
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetStatus { responder }) => async move {
                let (last_added_block, peers, chainspec_info) = join!(
                    effect_builder.get_highest_block(),
                    effect_builder.network_peers(),
                    effect_builder.get_chainspec_info()
                );
                let status_feed = StatusFeed::new(last_added_block, peers, chainspec_info);
                responder.respond(status_feed).await;
            }
            .ignore(),
            Event::RpcRequest(RpcRequest::GetMetrics { responder }) => effect_builder
                .get_metrics()
                .event(move |text| Event::GetMetricsResult {
                    text,
                    main_responder: responder,
                }),
            Event::GetBlockResult {
                maybe_id: _,
                result,
                main_responder,
            } => main_responder.respond(*result).ignore(),
            Event::QueryProtocolDataResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::QueryGlobalStateResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::QueryEraValidatorsResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::GetBalanceResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::GetDeployResult {
                hash: _,
                result,
                main_responder,
            } => main_responder.respond(*result).ignore(),
            Event::GetPeersResult {
                peers,
                main_responder,
            } => main_responder.respond(peers).ignore(),
            Event::GetMetricsResult {
                text,
                main_responder,
            } => main_responder.respond(text).ignore(),
        }
    }
}
