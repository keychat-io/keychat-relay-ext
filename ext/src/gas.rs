#![feature(impl_trait_in_assoc_type)]

#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate serde;
#[macro_use]
extern crate tracing;

// pub mod ga {
// tonic::include_proto!("nauthz");
// include!("../proto/nauthz.rs");
// }
#[rustfmt::skip]
#[path = "../proto/nauthz.rs"]
pub mod ga;

use ga::authorization_server::Authorization;
use ga::authorization_server::AuthorizationServer;
use ga::{EventReply, EventRequest};

#[rustfmt::skip]
#[path = "../proto/gas.rs"]
pub mod gas;

use futures::StreamExt;
use gas::subscription_server::{Subscription, SubscriptionServer};
use gas::{EventInfo, EventInfos, EventsRequest};

pub mod config;
use config::Config;
use config::Opts;

pub mod cashu;
use cashu::UniWallet;

pub mod metrics;
use metrics::LimiterState;
use metrics::Metric;
use metrics::MetricsMpmc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use clap::Parser;
    let opts = Opts::parse();

    let fmt = tracing_subscriber::fmt().with_line_number(true);
    if std::env::var("RUST_LOG").is_ok() {
        fmt.init();
    } else {
        let max = opts.log();
        fmt.with_max_level(max).init()
    }

    // # event_admission_server = "http://[::1]:50051"
    // let addr = "0.0.0.0:50051".parse().unwrap();
    let config = match opts.parse_config() {
        Ok(c) => c,
        Err(e) => {
            error!("config parse failed: {}", e);
            return Err(e.into());
        }
    };
    warn!("cashu cost_per_event is {}", config.cost_per_event());

    let addr = config.listen;
    let s = State::new(config).await?;
    let h = Handler { state: Arc::new(s) };

    info!(
        "grpc authorization server listening on {}, {:?}",
        addr, h.state.config
    );

    metrics::start_show_metrics(h.state.clone());

    tonic::transport::Server::builder()
        .add_service(AuthorizationServer::new(h.clone()))
        .add_service(SubscriptionServer::new(h))
        .serve(addr)
        .await?;

    Ok(())
}

use std::sync::Arc;
pub struct State {
    config: Config,
    wallet: UniWallet,
    metrics: MetricsMpmc,
    limiter: LimiterState,
    events: (
        tokio::sync::broadcast::Sender<EventInfos>,
        tokio::sync::broadcast::Receiver<EventInfos>,
    ),
}

impl State {
    async fn new(config: Config) -> anyhow::Result<Self> {
        let wallet = cashu::crate_cashu_wallet(&config, true).await?;
        let mpmc = tokio::sync::broadcast::channel(5000);

        Ok(Self {
            config,
            wallet,
            limiter: LimiterState::with_capacity(10000),
            metrics: flume::unbounded(),
            events: mpmc,
        })
    }
}

#[derive(Clone)]
pub struct Handler {
    state: Arc<State>,
}

#[tonic::async_trait]
impl Authorization for Handler {
    async fn event_admit(
        &self,
        request: tonic::Request<EventRequest>,
    ) -> std::result::Result<tonic::Response<EventReply>, tonic::Status> {
        let sa = request.remote_addr().unwrap();
        let eventr = request.get_ref();
        let source_ip = eventr.ip_addr();
        let user_agent = eventr.user_agent();

        let id = eventr
            .event
            .as_ref()
            .map(|event| faster_hex::hex_string(&event.id[..]))
            .unwrap();
        let id_prefix = &id[..8];
        let kind = eventr.event.as_ref().map(|e| e.kind as i64).unwrap_or(-1);
        let event_p = eventr.event.as_ref().and_then(|e| {
            e.tags
                .iter()
                .find(|tag| tag.values.len() > 1 && tag.values[0] == "p")
        });
        let is_cashu = eventr
            .event
            .as_ref()
            .and_then(|e| e.cashu.as_ref())
            .is_some();

        info!(
            "{} {}: {} {} {:?} {} {:?}",
            sa, is_cashu as u8, source_ip, id_prefix, kind, user_agent, event_p
        );

        let mut reply = None;
        if eventr.event.is_none() {
            reply = Some(EventReply {
                decision: 2,
                message: "empty event".to_owned().into(),
            });
        }

        let config = &self.state.config;
        if reply.is_none() && config.enabled {
            let event = eventr.event.as_ref().unwrap();
            // event contains cashu
            let is_cashu_free_kinds = config
                .kinds
                .as_ref()
                .map(|kinds| !kinds.contains(&event.kind))
                .unwrap_or_default();

            if is_cashu {
                let cashu = event.cashu.as_ref().unwrap();

                debug!(
                    "{} {} event-{} cashu: {}",
                    source_ip, event.kind, id_prefix, cashu,
                );

                let price = config.cost_per_event();
                let res =
                    cashu::receive_tokens(cashu, &id_prefix, source_ip, price, self.state.clone())
                        .await;

                match res {
                    Ok(None) => {
                        info!(
                            "{} {} cashu receive tokens limited: {:?}",
                            source_ip, id_prefix, source_ip,
                        );

                        reply = Some(EventReply {
                            decision: 2,
                            message: r#""detail":"Rate limit exceeded.""#.to_string().into(),
                        });
                    }
                    Ok(Some(e)) => {
                        info!(
                            "{} {} cashu receive tokens ok: {:?}",
                            source_ip, id_prefix, e,
                        );
                    }
                    Err(e) => {
                        warn!(
                            "{} {} cashu receive tokens failed: {}",
                            source_ip, id_prefix, e
                        );

                        use cashu_wallet::wallet::ClientError;
                        use cashu_wallet::UniError;
                        if config.allow_pending
                            && match &e {
                                UniError::Client(ClientError::Mint(_c, _d))
                                    if _d.contains("proofs already pending") =>
                                {
                                    true
                                }
                                _ => false,
                            }
                        {
                        } else {
                            reply = Some(EventReply {
                                decision: 2,
                                message: e.to_string().into(),
                            });
                        }
                    }
                }
            } else if is_cashu_free_kinds {
                info!(
                    "{} {} event-{} is cashu free kinds",
                    source_ip, event.kind, id_prefix
                );
            } else if !is_cashu_free_kinds && !config.allow_free {
                reply = Some(EventReply {
                    decision: 2,
                    message: Some("please pay by cashu".into()),
                });
            }
        }

        if reply.is_none() {
            reply = Some(EventReply {
                decision: 1,
                message: None,
            });
        }

        if reply.as_ref().map(|r| r.decision == 1).unwrap() {
            if let Some(p) = &event_p {
                let to = p.values[1..].to_owned();
                self.state
                    .events
                    .0
                    .send(EventInfos {
                        items: vec![EventInfo { id, to }],
                    })
                    .map_err(|e| error!("send event info failed: {}", e))
                    .ok();
            }
        }

        Ok(tonic::Response::new(reply.unwrap()))
    }
}

use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[tonic::async_trait]
impl Subscription for Handler {
    type EventsStream = futures::stream::Map<
        tokio_stream::wrappers::BroadcastStream<EventInfos>,
        impl FnMut(
            Result<EventInfos, BroadcastStreamRecvError>,
        ) -> std::result::Result<EventInfos, tonic::Status>,
    >;
    async fn events(
        &self,
        request: tonic::Request<EventsRequest>,
    ) -> Result<tonic::Response<Self::EventsStream>, tonic::Status> {
        info!(
            "{} events {}",
            request.remote_addr().unwrap(),
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

        Ok(tonic::Response::new(map))
    }
}
