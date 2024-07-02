#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate serde;
#[macro_use]
extern crate tracing;

use axum::{
    extract::{ConnectInfo, Path, State as AxumState},
    http::HeaderMap,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

pub mod config;
use config::Config;
use config::Opts;
use url::Url;

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

    let config = match opts.parse_config() {
        Ok(c) => c,
        Err(e) => {
            error!("config parse failed: {}", e);
            return Err(e.into());
        }
    };

    let lis = config.listen;
    let state = State::new(config).await?;
    info!(
        "storage authorization server listening on {}, {:?}",
        lis, state.config
    );
    let h = Handler::from(state);

    let v1 = Router::new()
        .route("/info", get(get_info))
        .route("/object", post(create_object))
        .route("/:key", axum::routing::put(put_object));

    let app = Router::new()
        // .route("/", get(root))
        .route("/v1/", get(root))
        .nest("/v1", v1)
        .with_state(h.clone())
        // https://docs.rs/tower-http/latest/tower_http/trace/index.html
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                // tower_http::trace::TraceLayer::new(
                //     tower_http::classify::StatusInRangeAsFailures::new(400..=599)
                //         .into_make_classifier(),
                // )
                .make_span_with(
                    tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO),
                )
                .on_response(
                    tower_http::trace::DefaultOnResponse::new().level(tracing::Level::INFO),
                ),
        )
        .into_make_service_with_connect_info::<SocketAddr>();

    let listener = tokio::net::TcpListener::bind(lis).await?;

    metrics::start_show_metrics(h.state.clone());

    axum::serve(listener, app).await?;
    Ok(())
}

// basic handler that responds with a static string
async fn root() -> Json<()> {
    ().into()
}

use serde_json::{json, Value};
async fn get_info(
    ConnectInfo(_sa): ConnectInfo<SocketAddr>,
    AxumState(Handler { state }): AxumState<Handler>,
) -> Json<Value> {
    let js = json!(state.config.fee);
    // js["mints"] = state.config.mints().iter().map(|s| s.as_str()).collect();
    Json(js)
}

// for test
async fn put_object(
    ConnectInfo(sa): ConnectInfo<SocketAddr>,
    // AxumState(_state): AxumState<Handler>,
    Path(key): Path<String>,
    header: HeaderMap,
    // or bytes
    body: String,
) -> impl IntoResponse {
    info!(
        "{} put_object {} {}:\n{:?}\n{:?}",
        sa,
        key,
        body.len(),
        header,
        body
    );
    "Ok"
}

// curl -X POST -H 'Content-type: application/json' --data '{"sha256": "oXLO3K5HR0thXFTVEKXYSo3qMDLpWFh0MLQTU4vj8zM=", "length": 3, "cashu": ""}' 0.0.0.0:9001/v1/object -v
async fn create_object(
    ConnectInfo(sa): ConnectInfo<SocketAddr>,
    AxumState(Handler { state }): AxumState<Handler>,
    header: HeaderMap,
    Json(js): Json<CreateObject>,
) -> Result<impl IntoResponse, (StatusCode, Cow<'static, str>)> {
    let mut status = StatusCode::from_u16(400).unwrap();

    let conf = &state.config;
    let mut ip = sa.ip().to_string();
    if conf.remote_ip_header.len() >= 1 {
        let h = header
            .get(&conf.remote_ip_header)
            .ok_or_else(|| "header.get ip none".into())
            .and_then(|v| v.to_str().map_err(|e| e.to_string()));

        match h {
            Ok(s) => {
                ip = s.to_string();
            }
            Err(e) => {
                error!("header.get ip {} failed: {}", conf.remote_ip_header, e);
            }
        }
    }

    if js.length == 0 {
        return Err((status, "object length invalid".into()));
    }

    let sha256 = match base64_simd::STANDARD.decode_to_vec(&js.sha256) {
        Ok(b) if b.len() == 256 / 8 => b,
        x => {
            #[rustfmt::skip]
            error!("{} base64_simd::STANDARD.decode: {:?}", sa,  x.map(|s| s.len()));
            return Err((status, "object sha256 invalid".into()));
        }
    };
    let key = base64_simd::URL_SAFE_NO_PAD.encode_to_string(&sha256);

    if conf.enabled {
        let price = match conf.compute_price(js.length) {
            Ok(p) => p,
            Err(e) => {
                status = StatusCode::BAD_REQUEST;
                return Err((status, e.to_string().into()));
            }
        };

        if price > 0 && js.cashu.is_empty() && !conf.allow_free {
            status = StatusCode::PAYMENT_REQUIRED;
            return Err((status, "object cashu empty".into()));
        }

        if price > 0 && !js.cashu.is_empty() {
            let res = cashu::receive_tokens(&js.cashu, &ip, &js.sha256, price, &state).await;
            match res {
                Ok(None) => {
                    info!("{} {} cashu receive tokens limited: {:?}", sa, key, ip,);
                    status = StatusCode::TOO_MANY_REQUESTS;
                    return Err((status, r#""detail":"Rate limit exceeded.""#.into()));
                }
                Ok(Some(_)) => {}
                Err(e) => {
                    status = StatusCode::PAYMENT_REQUIRED;
                    return Err((status, e.to_string().into()));
                }
            }
        }
    }

    let h = [
        ("content-length", js.length.to_string()),
        ("x-amz-checksum-sha256", js.sha256),
        // aws x-amz-acl header not work?
        ("x-amz-acl", "public-read".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect::<HashMap<_, _>>();

    let q = [
        // ("x-amz-acl".to_owned(), "public-read".to_owned()),
    ]
    .into_iter()
    .collect::<HashMap<_, _>>();

    use http::header::HeaderName;
    use http::header::HeaderValue;
    let custom_headers: http::HeaderMap = h
        .iter()
        .map(|(k, v)| {
            (
                HeaderName::try_from(k).expect("converted header name"),
                HeaderValue::from_str(v).expect("converted header value"),
            )
        })
        .collect();

    // https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html
    let url = state
        .bucket
        .presign_put(&key, 60 * 10, Some(custom_headers), Some(q))
        .await
        .unwrap();

    info!("{} {} {} PresignedUrl: {}", key, js.length, sa, url);

    let mut uri = Url::parse(&url).unwrap();
    uri.set_query(None);

    let object = ObjectForm {
        url,
        // url: format!("http://0.0.0.0:3001/v1/{key}"),
        // url: format!("https://backup.keychat.io/api/v1/{key}"),
        method: "PUT".into(),
        access_url: uri.to_string(),
        headers: h,
    };

    Ok(Json(object))
}

#[derive(Clone, Serialize, Deserialize)]
struct CreateObject {
    sha256: String,
    length: u64,
    #[serde(default)]
    cashu: String,
    // acl: Option<String>,
}

use std::{borrow::Cow, collections::HashMap, net::SocketAddr};
#[derive(Clone, Serialize, Deserialize)]
struct ObjectForm {
    method: String,
    url: String,
    headers: HashMap<String, String>,
    access_url: String,
}

use awscreds::Credentials;
use s3::Bucket;
use s3::Region;

use std::sync::Arc;
pub struct State {
    config: Config,
    bucket: Bucket,
    wallet: UniWallet,
    metrics: MetricsMpmc,
    limiter: LimiterState,
}

impl State {
    async fn new(config: Config) -> anyhow::Result<Self> {
        let wallet = cashu::crate_cashu_wallet(&config, true).await?;

        dotenvy::dotenv().expect(".env file not found");
        let bucket = std::env::var("AWS_BUCKET").unwrap();
        let region = Region::from_env("AWS_REGION", "AWS_ENDPOINT_URL".into())?;

        // https://s3.keychat.io/s3.keychat.io/oXLO3K5HR0thXFTVEKXYSo3qMDLpWFh0MLQTU4vj8zM
        let mut bucket = Bucket::new(
            &bucket,
            region,
            // Credentials are collected from environment, config, profile or instance metadata
            Credentials::default()?,
        )?
        .with_path_style();
        bucket.add_header("x-amz-acl", "public-read");

        let this = Self {
            config,
            bucket,
            wallet,
            limiter: LimiterState::with_capacity(10000),
            metrics: flume::unbounded(),
        };

        Ok(this)
    }
}

#[derive(Clone)]
pub struct Handler {
    state: Arc<State>,
}

impl From<State> for Handler {
    fn from(value: State) -> Self {
        Self {
            state: Arc::new(value),
        }
    }
}
