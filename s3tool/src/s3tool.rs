#[macro_use]
extern crate serde;

use awscreds::Credentials;
use ring::digest;
use s3::Bucket;
use s3::Region;
use serde_json::json;

use std::collections::HashMap;
#[derive(Clone, Serialize, Deserialize)]
struct ObjectForm {
    method: String,
    url: String,
    headers: HashMap<String, String>,
    access_url: String,
}

#[::tokio::main]

async fn main() -> Result<(), s3::error::S3Error> {
    dotenvy::dotenv().expect(".env file not found");

    let bucket = std::env::var("AWS_BUCKET").unwrap();
    let region = Region::from_env("AWS_REGION", "AWS_ENDPOINT_URL".into())?;
    // https://s3.keychat.io/s3.keychat.io/oXLO3K5HR0thXFTVEKXYSo3qMDLpWFh0MLQTU4vj8zM?X-Amz-Algorithm=AWS4-HMAC-SHA256
    let mut bucket = Bucket::new(
        &bucket,
        region,
        // Credentials are collected from environment, config, profile or instance metadata
        Credentials::default()?,
    )?
    .with_path_style();
    bucket.add_header("x-amz-acl", "public-read");
    println!("{:?}", bucket);

    let args: Vec<_> = std::env::args().skip(1).collect();

    let mut body = vec![];
    let mut js = None;
    let mut arg = String::new();
    for a in &args {
        match std::fs::read(a) {
            Ok(c) => {
                if let Ok(p) = serde_json::from_slice::<ObjectForm>(&c) {
                    js = Some(p);
                } else {
                    body = c;
                }
            }
            Err(e) => {
                println!("{} read: {:?}", a, e);
                arg = a.to_owned();
            }
        }
    }

    if body.is_empty() {
        body = arg.clone().into();
    }

    let hash = digest::digest(&digest::SHA256, &body);
    let key = base64_simd::URL_SAFE_NO_PAD.encode_to_string(hash.as_ref());
    let sum = base64_simd::STANDARD.encode_to_string(hash.as_ref());
    println!("key: {}", key);
    println!("sum: {}", sum);

    if let Some(form) = js {
        let resp =
            send_presigned_request_with_reqwest(&form.url, &form.method, form.headers, body).await;
        println!("put_object_presigned {}: {:?}", key, resp);
    } else if arg.is_empty() {
        let json = json!({
            "sha256": sum,
            "length": body.len(),
            "cashu": ""
        });

        let js = serde_json::to_string(&json).unwrap();
        println!("{}", js);
    } else {
        // let res = bucket.put_object(&key, body).await?;
        // println!("{}", res);

        // use std::time::*;
        // let ts = SystemTime::now()
        //     .duration_since(UNIX_EPOCH)
        //     .expect("Time went backwards")
        //     .as_millis() as u64;
        // let expires = ts + 120;

        // use chrono::{Utc, TimeDelta};
        // let expires = Utc::now()
        //     .checked_add_signed(TimeDelta::seconds(60))
        //     .unwrap()
        //     .to_rfc2822();

        // https://docs.rs/aws-sdk-s3/1.37.0/aws_sdk_s3/operation/put_object/builders/struct.PutObjectFluentBuilder.html#method.set_expires
        let h = [
            ("content-length", body.len().to_string()),
            ("x-amz-checksum-sha256", sum),
            // aws x-amz-acl header not work?
            ("x-amz-acl", "public-read".to_owned()),
            // ("X-Amz-Expires", format!("expiry-date={}",expires)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect::<HashMap<_, _>>();
        let q = [
        // aws x-amz-acl header not work
        // ("x-amz-acl".to_owned(), "public-read".to_owned()),
    ]
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();

        let custom_headers: HeaderMap = h
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::try_from(k).expect("converted header name"),
                    HeaderValue::from_str(v).expect("converted header value"),
                )
            })
            .collect();

        // https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html
        let url = bucket
            .presign_put(&key, 86400, Some(custom_headers), Some(q.clone()))
            .await
            .unwrap();
        println!("Presigned url: {}", url);
        // let url = url.replace("s3-ap-southeast-1.amazonaws.com", "s3.keychat.io");

        // let body = include_bytes!("../x.rs2");
        let resp = send_presigned_request_with_reqwest(&url, "PUT", h, body).await;
        println!("put_object_presigned {}: {:?}", key, resp);
    }
    Ok(())
}

use http::header::HeaderName;
use http::header::HeaderValue;
use http::HeaderMap;

/// This function demonstrates how you can send a presigned request using [reqwest](https://crates.io/crates/reqwest)
async fn send_presigned_request_with_reqwest(
    url: &str,
    method: &str,
    headers: impl IntoIterator<Item = (String, String)>,
    body: impl Into<reqwest::Body>,
) {
    let client = reqwest::Client::new();
    let res = client
        .request(method.parse().unwrap(), url)
        .headers(
            headers
                .into_iter()
                .filter(|(k, v)| {
                    println!("H: {:?}: {:?}", k, v);
                    *k != "x-amz-ac"
                })
                .map(|(name, value)| {
                    (
                        HeaderName::try_from(name).expect("converted header name"),
                        HeaderValue::from_str(&value).expect("converted header value"),
                    )
                })
                .collect(),
        )
        .body(body)
        .send()
        .await;

    match res {
        Ok(res) => {
            println!("Response: {:?}", res);
            let json = res.text().await.unwrap();
            println!("Response.Text: {:?}", json);
        }
        Err(err) => {
            println!("send_presigned Error: {}", err)
        }
    }
}
