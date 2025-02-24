/*
 * Copyright 2022 Google LLC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::sync::Arc;

use cached::Cached;
use futures::Stream;
use tokio_stream::StreamExt;
use tracing_futures::Instrument;

use crate::{
    config::Config,
    xds::{
        metrics,
        service::discovery::v3::{
            aggregated_discovery_service_server::{
                AggregatedDiscoveryService, AggregatedDiscoveryServiceServer,
            },
            DeltaDiscoveryRequest, DeltaDiscoveryResponse, DiscoveryRequest, DiscoveryResponse,
        },
        ResourceType,
    },
};

#[tracing::instrument(skip_all)]
pub async fn spawn(port: u16, config: std::sync::Arc<crate::Config>) -> crate::Result<()> {
    let server = AggregatedDiscoveryServiceServer::new(ControlPlane::from_arc(config));
    let server = tonic::transport::Server::builder().add_service(server);
    tracing::info!("Serving management server at {}", port);
    Ok(server
        .serve((std::net::Ipv4Addr::UNSPECIFIED, port).into())
        .await?)
}

#[derive(Clone)]
pub struct ControlPlane {
    config: Arc<Config>,
    watchers: Arc<crate::xds::resource::ResourceMap<Watchers>>,
}

struct Watchers {
    sender: tokio::sync::watch::Sender<()>,
    receiver: tokio::sync::watch::Receiver<()>,
    version: std::sync::atomic::AtomicU64,
}

impl Default for Watchers {
    fn default() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(());
        Self {
            sender,
            receiver,
            version: <_>::default(),
        }
    }
}

impl ControlPlane {
    /// Creates a new server for managing [`Config`].
    pub fn new(config: Config) -> Self {
        Self::from_arc(Arc::new(config))
    }

    pub fn from_arc(config: Arc<Config>) -> Self {
        let this = Self {
            config,
            watchers: <_>::default(),
        };

        this.config.clusters.watch({
            let this = this.clone();
            move |_| {
                this.push_update(ResourceType::Endpoint);
                this.push_update(ResourceType::Cluster);
            }
        });

        this.config.filters.watch({
            let this = this.clone();
            move |_| {
                this.push_update(ResourceType::Listener);
            }
        });

        this
    }

    fn push_update(&self, resource_type: ResourceType) {
        let watchers = &self.watchers[resource_type];
        watchers
            .version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::trace!(%resource_type, watchers=watchers.sender.receiver_count(), "pushing update");
        if let Err(error) = watchers.sender.send(()) {
            tracing::warn!(%error, "pushing update failed");
        }
    }

    fn discovery_response(
        &self,
        id: &str,
        resource_type: ResourceType,
        names: &[String],
    ) -> Result<DiscoveryResponse, tonic::Status> {
        let mut response = self
            .config
            .discovery_request(id, resource_type, names)
            .map_err(|error| tonic::Status::internal(error.to_string()))?;
        let watchers = &self.watchers[resource_type];

        let nonce = uuid::Uuid::new_v4();
        response.version_info = watchers
            .version
            .load(std::sync::atomic::Ordering::Relaxed)
            .to_string();
        response.control_plane = Some(crate::xds::config::core::v3::ControlPlane {
            identifier: (*self.config.id.load()).clone(),
        });
        response.nonce = nonce.to_string();

        tracing::trace!(
            id = &*response.version_info,
            r#type = &*response.type_url,
            nonce = &*response.nonce,
            "discovery response"
        );

        Ok(response)
    }

    pub async fn stream_aggregated_resources<S>(
        &self,
        mut streaming: S,
    ) -> Result<impl Stream<Item = Result<DiscoveryResponse, tonic::Status>> + Send, tonic::Status>
    where
        S: Stream<Item = Result<DiscoveryRequest, tonic::Status>>
            + Send
            + std::marker::Unpin
            + 'static,
    {
        tracing::trace!("starting stream");
        let message = streaming.next().await.ok_or_else(|| {
            tracing::error!("No message found");
            tonic::Status::invalid_argument("No message found")
        })??;

        if message.node.is_none() {
            tracing::error!("Node identifier was not found");
            return Err(tonic::Status::invalid_argument("Node identifier required"));
        }

        let node = message.node.clone().unwrap();
        let resource_type: ResourceType = message.type_url.parse()?;
        tracing::trace!(id = %node.id, %resource_type, "initial request");
        metrics::DISCOVERY_REQUESTS
            .with_label_values(&[&*node.id, resource_type.type_url()])
            .inc();
        let mut rx = self.watchers[resource_type].receiver.clone();
        let mut pending_acks = cached::TimedSizedCache::with_size_and_lifespan(50, 1);
        let this = Self::clone(self);
        let response = this.discovery_response(&node.id, resource_type, &message.resource_names)?;
        pending_acks.cache_set(response.nonce.clone(), ());

        let id = node.id.clone();
        Ok(Box::pin(async_stream::try_stream! {
            yield response;

            let _span = tracing::trace_span!("stream loop");
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        tracing::trace!("sending new discovery response");
                        yield this.discovery_response(&id, resource_type, &message.resource_names).map(|response| {
                            pending_acks.cache_set(response.nonce.clone(), ());
                            response
                        })?;
                    }
                    new_message = streaming.next() => {
                        let new_message = match new_message.transpose() {
                            Ok(Some(value)) => value,
                            Ok(None) => break,
                            Err(error) => {
                                tracing::error!(%error, "unknown resource type");
                                continue;
                            }
                        };

                        let id = new_message.node.as_ref().map(|node| &*node.id).unwrap_or(&*id);
                        let resource_type = match new_message.type_url.parse::<ResourceType>() {
                            Ok(value) => value,
                            Err(error) => {
                                tracing::error!(%error, "unknown resource type");
                                continue;
                            }
                        };

                        tracing::trace!("new request");
                        metrics::DISCOVERY_REQUESTS.with_label_values(&[id, resource_type.type_url()]).inc();

                        if let Some(error) = &new_message.error_detail {
                            metrics::NACKS.with_label_values(&[id, resource_type.type_url()]).inc();
                            tracing::error!(nonce = %new_message.response_nonce, ?error, "NACK");
                            // Currently just resend previous discovery response.
                        } else if uuid::Uuid::parse_str(&new_message.response_nonce).is_ok() {
                            if pending_acks.cache_get(&new_message.response_nonce).is_some() {
                                tracing::info!(nonce = %new_message.response_nonce, "ACK");
                                continue
                            } else {
                                tracing::trace!(nonce = %new_message.response_nonce, "Unknown nonce: could not be found in cache");
                                continue
                            }
                        }

                        yield this.discovery_response(id, resource_type, &message.resource_names).map(|response| {
                            pending_acks.cache_set(response.nonce.clone(), ());
                            response
                        }).unwrap();
                    }
                }
            }

            tracing::info!("terminating stream");
        }.instrument(tracing::info_span!("xds_stream", %node.id, %resource_type))))
    }
}

#[tonic::async_trait]
impl AggregatedDiscoveryService for ControlPlane {
    type StreamAggregatedResourcesStream =
        std::pin::Pin<Box<dyn Stream<Item = Result<DiscoveryResponse, tonic::Status>> + Send>>;
    type DeltaAggregatedResourcesStream =
        tokio_stream::wrappers::ReceiverStream<Result<DeltaDiscoveryResponse, tonic::Status>>;

    #[tracing::instrument(skip_all)]
    async fn stream_aggregated_resources(
        &self,
        request: tonic::Request<tonic::Streaming<DiscoveryRequest>>,
    ) -> Result<tonic::Response<Self::StreamAggregatedResourcesStream>, tonic::Status> {
        Ok(tonic::Response::new(Box::pin(
            self.stream_aggregated_resources(request.into_inner())
                .in_current_span()
                .await?,
        )))
    }

    async fn delta_aggregated_resources(
        &self,
        _request: tonic::Request<tonic::Streaming<DeltaDiscoveryRequest>>,
    ) -> Result<tonic::Response<Self::DeltaAggregatedResourcesStream>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "Quilkin doesn't currently support Delta xDS",
        ))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tokio::time::timeout;

    use super::*;
    use crate::xds::{
        config::{
            core::v3::Node,
            listener::v3::{FilterChain, Listener},
        },
        service::discovery::v3::DiscoveryResponse,
        ResourceType,
    };

    const TIMEOUT_DURATION: std::time::Duration = std::time::Duration::from_secs(10);

    #[tokio::test]
    async fn valid_response() {
        const RESOURCE: ResourceType = ResourceType::Endpoint;
        const LISTENER_TYPE: ResourceType = ResourceType::Listener;

        let mut response = DiscoveryResponse {
            version_info: String::new(),
            resources: vec![],
            type_url: RESOURCE.type_url().into(),
            ..<_>::default()
        };

        let mut listener_response = DiscoveryResponse {
            version_info: String::new(),
            resources: vec![prost_types::Any {
                type_url: LISTENER_TYPE.type_url().into(),
                value: crate::prost::encode(&Listener {
                    filter_chains: vec![FilterChain {
                        filters: vec![],
                        ..<_>::default()
                    }],
                    ..<_>::default()
                })
                .unwrap(),
            }],
            type_url: LISTENER_TYPE.type_url().into(),
            ..<_>::default()
        };

        let config = Arc::new(Config::default());
        let client = ControlPlane::from_arc(config.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(256);

        let mut request = DiscoveryRequest {
            node: Some(Node {
                id: "quilkin".into(),
                user_agent_name: "quilkin".into(),
                ..Node::default()
            }),
            resource_names: vec![],
            type_url: RESOURCE.type_url().into(),
            ..DiscoveryRequest::default()
        };

        let mut listener_request = DiscoveryRequest {
            node: Some(Node {
                id: "quilkin".into(),
                user_agent_name: "quilkin".into(),
                ..Node::default()
            }),
            resource_names: vec![],
            type_url: LISTENER_TYPE.type_url().into(),
            ..DiscoveryRequest::default()
        };

        timeout(TIMEOUT_DURATION, tx.send(Ok(request.clone())))
            .await
            .unwrap()
            .unwrap();

        let mut stream = timeout(
            TIMEOUT_DURATION,
            client.stream_aggregated_resources(Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(rx),
            )),
        )
        .await
        .unwrap()
        .unwrap();

        let message = timeout(TIMEOUT_DURATION, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        response.version_info = message.version_info.clone();
        response.nonce = message.nonce.clone();
        response.control_plane = message.control_plane.clone();
        request.response_nonce = message.nonce.clone();

        assert_eq!(response, message);

        timeout(TIMEOUT_DURATION, tx.send(Ok(request.clone())))
            .await
            .unwrap()
            .unwrap();

        timeout(TIMEOUT_DURATION, tx.send(Ok(listener_request.clone())))
            .await
            .unwrap()
            .unwrap();

        let message = timeout(TIMEOUT_DURATION, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        listener_response.control_plane = message.control_plane.clone();
        listener_response.version_info = message.version_info.clone();
        listener_response.nonce = message.nonce.clone();
        listener_request.response_nonce = message.nonce.clone();

        assert_eq!(listener_response, message);

        timeout(TIMEOUT_DURATION, tx.send(Ok(listener_request.clone())))
            .await
            .unwrap()
            .unwrap();
    }
}
