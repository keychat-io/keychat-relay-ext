#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate serde;
#[macro_use]
extern crate tracing;
#[rustfmt::skip]
#[path = "../proto/nauthz.rs"] pub mod ga;
#[rustfmt::skip]
#[path = "../proto/gas.rs"] pub mod gas;
pub mod cashu;
pub mod config;
pub mod metrics;
use cashu::CashuWallet;
use config::{Config, Opts};
use futures::StreamExt;
use ga::{
    authorization_server::{Authorization, AuthorizationServer},
    EventReply, EventRequest,
};
use gas::{
    subscription_server::{Subscription, SubscriptionServer},
    EventInfo, EventInfos, EventsRequest,
};
use metrics::{LimiterState, Metric, MetricsMpmc};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;
    let opts = Opts::parse();
    let fmt = tracing_subscriber::fmt().with_line_number(true);
    if std::env::var("RUST_LOG").is_ok() {
        fmt.init();
    } else {
        fmt.with_max_level(opts.log()).init();
    }
    let config = opts.parse_config()?;
    config.validate_gas()?;
    let addr = config.listen;
    let s = State::new(config).await?;
    let h = Handler { state: Arc::new(s) };
    info!(%addr, "grpc authorization server listening");
    metrics::start_show_metrics(h.state.clone());
    tonic::transport::Server::builder()
        .concurrency_limit_per_connection(64)
        .add_service(authorization_service(h.clone()))
        .add_service(SubscriptionServer::new(h))
        .serve(addr)
        .await?;
    Ok(())
}
// Keep tonic's existing 4 MiB envelope limit. Token-specific bounds belong in
// parse_token; tightening the whole request would also reject valid event content.
fn authorization_service(handler: Handler) -> AuthorizationServer<Handler> {
    AuthorizationServer::new(handler)
}

pub struct State {
    config: Config,
    wallet: Arc<CashuWallet>,
    metrics: MetricsMpmc,
    limiter: LimiterState,
    events: (
        tokio::sync::broadcast::Sender<EventInfos>,
        tokio::sync::broadcast::Receiver<EventInfos>,
    ),
}
impl State {
    async fn new(config: Config) -> anyhow::Result<Self> {
        let wallet = CashuWallet::new(&config, true).await?;
        Ok(Self {
            config,
            wallet,
            metrics: flume::unbounded(),
            limiter: LimiterState::with_capacity(10000),
            events: tokio::sync::broadcast::channel(5000),
        })
    }
}
#[derive(Clone)]
pub struct Handler {
    state: Arc<State>,
}
fn deny(message: &str) -> EventReply {
    EventReply {
        decision: 2,
        message: Some(message.into()),
    }
}
impl Handler {
    async fn admit(&self, request: &EventRequest) -> EventReply {
        let Some(event) = request.event.as_ref() else {
            return deny("empty event");
        };
        if event.id.len() != 32 {
            return deny("event ID must contain 32 bytes");
        }
        let conf = &self.state.config;
        let id = faster_hex::hex_string(&event.id);
        // Free kinds never consume an attached ticket. Disabled gas fails closed
        // for configured paid kinds; it is not a free-publication switch.
        if conf.charged(event.kind) {
            if !conf.enabled {
                return deny("payment admission unavailable");
            }
            match self.state.wallet.status(id.clone(), conf.timeout_ms).await {
                Ok(Some(true)) => {
                    return EventReply {
                        decision: 1,
                        message: None,
                    }
                }
                Ok(Some(false)) => {
                    return deny("original payment unresolved; retry the same event")
                }
                Ok(None) => {}
                Err(_) => return deny("payment status unavailable"),
            }
            let Some(token) = event.cashu.as_ref().filter(|t| !t.is_empty()) else {
                return deny("please pay by cashu");
            };
            let parsed = match cashu::parse_token(token, &conf.input) {
                Ok(p) => p,
                Err(_) => return deny("invalid cashu token"),
            };
            let price = match conf.price_for(event.kind, &parsed.mint, &parsed.unit.to_string()) {
                Ok(p) => p,
                Err(_) => return deny("payment mint, unit or kind is not accepted"),
            };
            let mint = parsed.mint.to_string();
            let unit = parsed.unit.to_string();
            let ip = request.ip_addr();
            if self.state.limiter.cashu_failed_check(ip, &conf.limits) {
                return deny("payment rate limit exceeded");
            }
            let start = std::time::Instant::now();
            match self
                .state
                .wallet
                .pay(id.clone(), parsed, price, conf.timeout_ms)
                .await
            {
                Ok(amount) => {
                    if amount > 0 {
                        let _ = self.state.metrics.0.send(Metric {
                            costms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                            amount,
                            mint,
                            unit,
                        });
                    }
                }
                Err(_) => {
                    self.state.limiter.cashu_failed_count(ip, &conf.limits);
                    return deny("payment unconfirmed; retry the same event");
                }
            }
        }
        EventReply {
            decision: 1,
            message: None,
        }
    }
}
#[tonic::async_trait]
impl Authorization for Handler {
    async fn event_admit(
        &self,
        request: tonic::Request<EventRequest>,
    ) -> Result<tonic::Response<EventReply>, tonic::Status> {
        let eventr = request.get_ref();
        let reply = self.admit(eventr).await;
        if reply.decision == 1 {
            if let Some(event) = eventr.event.as_ref() {
                if let Some(tag) = event
                    .tags
                    .iter()
                    .find(|t| t.values.len() > 1 && t.values[0] == "p")
                {
                    let _ = self.state.events.0.send(EventInfos {
                        items: vec![EventInfo {
                            id: faster_hex::hex_string(&event.id),
                            to: tag.values[1..].to_vec(),
                        }],
                    });
                }
            }
        }
        Ok(tonic::Response::new(reply))
    }
}

#[allow(clippy::result_large_err)] // tonic requires Status as the stream error
#[tonic::async_trait]
impl Subscription for Handler {
    type EventsStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<EventInfos, tonic::Status>> + Send>>;
    async fn events(
        &self,
        request: tonic::Request<EventsRequest>,
    ) -> Result<tonic::Response<Self::EventsStream>, tonic::Status> {
        info!(
            "{} events {}",
            format!("{:?}", request.remote_addr()),
            request.get_ref().client
        );
        let mc = self.state.events.0.subscribe();
        let stream = tokio_stream::wrappers::BroadcastStream::new(mc);
        let map = stream.map(|res| {
            // info!("subscribe.map: {:?}", res);

            match res {
                Ok(v) => Ok(v),
                Err(_e) => Err(tonic::Status::aborted("eof")),
            }
        });

        Ok(tonic::Response::new(Box::pin(map)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::nuts::CurrencyUnit;
    fn request(kind: u64, token: Option<String>) -> EventRequest {
        EventRequest {
            event: Some(ga::Event {
                id: vec![7; 32],
                kind,
                cashu: token,
                tags: vec![ga::event::TagEntry {
                    values: vec!["p".into(), "recipient".into()],
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn gas_requires_confirmed_payment_preserves_free_kinds_and_subscription() {
        for unit in [CurrencyUnit::Sat, CurrencyUnit::custom("stamp")] {
            let (wallet, mint, url, _dir, server) = cashu::tests::fixture(unit.clone(), 0).await;
            let mut config: Config = toml::from_str(include_str!("../gas.toml")).unwrap();
            config.fee.mints = vec![url.clone()];
            config.stamp.as_mut().unwrap().mint = url.clone();
            let state = Arc::new(State {
                config,
                wallet,
                limiter: LimiterState::with_capacity(10),
                metrics: flume::unbounded(),
                events: tokio::sync::broadcast::channel(100),
            });
            let h = Handler { state };
            let mut sub = h.state.events.0.subscribe();
            let denied = h
                .event_admit(tonic::Request::new(request(4, None)))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(denied.decision, 2);
            assert!(sub.try_recv().is_err());
            let token = mint.token(&url, 1);
            let free = h
                .event_admit(tonic::Request::new(request(1, Some(token.clone()))))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(free.decision, 1);
            sub.try_recv().unwrap();
            let permit = h
                .event_admit(tonic::Request::new(request(4, Some(token))))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(permit.decision, 1);
            let event = sub.try_recv().unwrap();
            assert_eq!(event.items[0].id, "07".repeat(32));
            assert_eq!(event.items[0].to, vec!["recipient"]);
            // Full event receipt survives a lost permit response, with no token.
            assert_eq!(h.admit(&request(4, None)).await.decision, 1);
            server.abort();
        }
    }
    #[tokio::test]
    async fn malformed_missing_disabled_and_pending_bypass_are_denied() {
        let (wallet, _mint, _url, _dir, server) = cashu::tests::fixture(CurrencyUnit::Sat, 0).await;
        let mut config: Config = toml::from_str(include_str!("../gas.toml")).unwrap();
        config.allow_pending = true;
        config.enabled = false;
        let h = Handler {
            state: Arc::new(State {
                config,
                wallet,
                limiter: LimiterState::with_capacity(10),
                metrics: flume::unbounded(),
                events: tokio::sync::broadcast::channel(10),
            }),
        };
        assert_eq!(h.admit(&EventRequest::default()).await.decision, 2);
        let mut r = request(4, None);
        r.event.as_mut().unwrap().id = vec![0];
        assert_eq!(h.admit(&r).await.decision, 2);
        assert_eq!(
            h.admit(&request(4, Some("cashuBinvalid".into())))
                .await
                .decision,
            2
        );
        assert_eq!(h.admit(&request(1, None)).await.decision, 1);
        server.abort();
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    #[tokio::test]
    async fn grpc_keeps_existing_event_envelope_capacity() {
        let (wallet, _mint, _url, _dir, mint_server) =
            cashu::tests::fixture(cdk::nuts::CurrencyUnit::Sat, 0).await;
        let config: Config = toml::from_str(include_str!("../gas.toml")).unwrap();
        let handler = Handler {
            state: Arc::new(State {
                config,
                wallet,
                limiter: LimiterState::with_capacity(10),
                metrics: flume::unbounded(),
                events: tokio::sync::broadcast::channel(10),
            }),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = futures::stream::try_unfold(listener, |listener| async {
            let (stream, _) = listener.accept().await?;
            Ok::<_, std::io::Error>(Some((stream, listener)))
        });
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(authorization_service(handler))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = tonic::client::Grpc::new(channel);
        client.ready().await.unwrap();
        let request = EventRequest {
            event: Some(ga::Event {
                id: vec![3; 32],
                kind: 1,
                content: "x".repeat(300_000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let reply: tonic::Response<EventReply> = client
            .unary(
                tonic::Request::new(request),
                http::uri::PathAndQuery::from_static("/nauthz.Authorization/EventAdmit"),
                tonic::codec::ProstCodec::default(),
            )
            .await
            .unwrap();
        assert_eq!(reply.into_inner().decision, 1);
        server.abort();
        mint_server.abort();
    }
}
