//! SAS public endpoints. Reuse rust-s3's credentials, query generation and SigV4
//! primitives; the final upload host and path must be selected before signing.
use anyhow::{anyhow, ensure, Result};
use hmac::Mac;
use http::{header::HOST, HeaderMap};
use s3::{
    command::Command,
    request::{tokio_backend::HyperRequest, Request},
    signing, Bucket,
};
use std::collections::HashMap;
use url::Url;

use super::ObjectForm;

const EXPIRES_SECS: u32 = 60 * 10;

#[derive(Default)]
pub(crate) struct PublicMediaUrls {
    upload: Option<Url>,
    base: Option<Url>,
}

impl PublicMediaUrls {
    pub(crate) fn from_env() -> Result<Self> {
        fn read(name: &str) -> Result<Option<String>> {
            match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(_) => Err(anyhow!("{name} must be valid UTF-8")),
            }
        }
        Self::parse(
            read("MEDIA_PUBLIC_UPLOAD_URL")?.as_deref(),
            read("MEDIA_PUBLIC_BASE_URL")?.as_deref(),
        )
    }

    fn parse(upload: Option<&str>, base: Option<&str>) -> Result<Self> {
        fn parse_url(name: &str, value: Option<&str>) -> Result<Option<Url>> {
            let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
                return Ok(None);
            };
            let url =
                Url::parse(value).map_err(|_| anyhow!("{name} must be an absolute HTTPS URL"))?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "{name} must be an absolute HTTPS URL without credentials, query or fragment"
            );
            Ok(Some(url))
        }
        Ok(Self {
            upload: parse_url("MEDIA_PUBLIC_UPLOAD_URL", upload)?,
            base: parse_url("MEDIA_PUBLIC_BASE_URL", base)?,
        })
    }

    fn object_url(base: &Url, key: &str) -> Url {
        let mut url = base.clone();
        // HTTPS URLs are hierarchical; preserve an optional prefix and encode
        // the content-addressed key as one segment, with or without a trailing slash.
        url.path_segments_mut().unwrap().pop_if_empty().push(key);
        url
    }

    pub(crate) async fn presign_put(
        &self,
        bucket: &Bucket,
        key: &str,
        headers: HashMap<String, String>,
    ) -> Result<ObjectForm> {
        let custom_headers: HeaderMap = headers
            .iter()
            .map(|(k, v)| Ok((k.parse::<http::header::HeaderName>()?, v.parse()?)))
            .collect::<Result<_>>()?;
        let request = HyperRequest::new(
            bucket,
            key,
            Command::PresignPut {
                expiry_secs: EXPIRES_SECS,
                custom_headers: Some(custom_headers.clone()),
                custom_queries: None,
            },
        )
        .await?;

        let mut access_url = match &self.base {
            Some(base) => Self::object_url(base, key),
            None => request.url()?,
        };
        access_url.set_query(None);

        let url = if let Some(base) = &self.upload {
            let query_url = request
                .presigned_url_no_sig(EXPIRES_SECS, Some(&custom_headers), None)
                .await?;
            let mut url = Self::object_url(base, key);
            // Includes expiry, credential scope and any temporary session token.
            url.set_query(query_url.query());
            let mut signed_headers = custom_headers;
            signed_headers.insert(
                HOST,
                url[url::Position::BeforeHost..url::Position::AfterPort].parse()?,
            );
            let canonical =
                signing::canonical_request("PUT", &url, &signed_headers, "UNSIGNED-PAYLOAD")?;
            let string_to_sign = request.string_to_sign(&canonical)?;
            let mut hmac = signing::HmacSha256::new_from_slice(&request.signing_key().await?)?;
            hmac.update(string_to_sign.as_bytes());
            let signature = faster_hex::hex_string(&hmac.finalize().into_bytes());
            format!("{url}&X-Amz-Signature={signature}")
        } else {
            request.presigned().await?
        };

        Ok(ObjectForm {
            method: "PUT".into(),
            url,
            access_url: access_url.to_string(),
            headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awscreds::Credentials;
    use s3::Region;

    fn credentials() -> Credentials {
        Credentials::new(
            Some("component-test"),
            Some("component-test-only"),
            None,
            Some("component-session-token"),
            None,
        )
        .unwrap()
    }

    fn bucket() -> Box<Bucket> {
        Bucket::new(
            "keychat-media",
            Region::Custom {
                region: "ap-southeast-1".into(),
                endpoint: "https://object.invalid".into(),
            },
            credentials(),
        )
        .unwrap()
        .with_path_style()
    }

    fn headers() -> HashMap<String, String> {
        [
            ("content-length", "16"),
            ("x-amz-acl", "public-read"),
            ("x-amz-checksum-sha256", "component-checksum"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }

    #[test]
    fn public_urls_validate_and_preserve_path_prefixes() {
        for value in [
            "relative/path",
            "http://cdn.example.com",
            "https://user:password@cdn.example.com",
            "https://cdn.example.com?query=1",
            "https://cdn.example.com#fragment",
        ] {
            assert!(PublicMediaUrls::parse(Some(value), None).is_err());
            assert!(PublicMediaUrls::parse(None, Some(value)).is_err());
        }
        let empty = PublicMediaUrls::parse(Some("  "), Some("")).unwrap();
        assert!(empty.upload.is_none() && empty.base.is_none());
        for base in [
            "https://cdn.example.com/media",
            "https://cdn.example.com/media/",
        ] {
            assert_eq!(
                PublicMediaUrls::object_url(&Url::parse(base).unwrap(), "hash_-123").as_str(),
                "https://cdn.example.com/media/hash_-123"
            );
        }
    }

    #[tokio::test]
    async fn upload_and_download_overrides_are_independent() {
        let bucket = bucket();
        for upload in [
            None,
            Some("https://keychat-media.s3-accelerate.amazonaws.com"),
        ] {
            for base in [None, Some("https://cdn.keychat.io/files/")] {
                let form = PublicMediaUrls::parse(upload, base)
                    .unwrap()
                    .presign_put(&bucket, "hash_-123", headers())
                    .await
                    .unwrap();
                let url = Url::parse(&form.url).unwrap();
                assert_eq!(
                    url.host_str(),
                    Some(if upload.is_some() {
                        "keychat-media.s3-accelerate.amazonaws.com"
                    } else {
                        "object.invalid"
                    })
                );
                assert_eq!(
                    url.path(),
                    if upload.is_some() {
                        "/hash_-123"
                    } else {
                        "/keychat-media/hash_-123"
                    }
                );
                assert_eq!(
                    form.access_url,
                    if base.is_some() {
                        "https://cdn.keychat.io/files/hash_-123"
                    } else {
                        "https://object.invalid/keychat-media/hash_-123"
                    }
                );
                assert_eq!(form.method, "PUT");
                assert_eq!(form.headers, headers());
                let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
                assert_eq!(query["X-Amz-Expires"], "600");
                assert_eq!(query["X-Amz-Security-Token"], "component-session-token");
                assert!(query.contains_key("X-Amz-Signature"));
                assert!(Url::parse(&form.access_url).unwrap().query().is_none());
            }
        }
    }

    #[tokio::test]
    async fn upload_signature_matches_native_s3_accelerate_signing() {
        let form = PublicMediaUrls::parse(
            Some("https://keychat-media.s3-accelerate.amazonaws.com"),
            Some("https://cdn.keychat.io"),
        )
        .unwrap()
        .presign_put(&bucket(), "hash_-123", headers())
        .await
        .unwrap();
        let url = Url::parse(&form.url).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let native_bucket = Bucket::new(
            "keychat-media",
            Region::Custom {
                region: "ap-southeast-1".into(),
                endpoint: "https://s3-accelerate.amazonaws.com".into(),
            },
            credentials(),
        )
        .unwrap();
        let custom_headers: HeaderMap = headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.parse::<http::header::HeaderName>().unwrap(),
                    v.parse().unwrap(),
                )
            })
            .collect();
        let mut native = HyperRequest::new(
            &native_bucket,
            "hash_-123",
            Command::PresignPut {
                expiry_secs: EXPIRES_SECS,
                custom_headers: Some(custom_headers),
                custom_queries: None,
            },
        )
        .await
        .unwrap();
        // Match the emitted timestamp without relying on both requests happening
        // in the same second. No network or real AWS credentials are used.
        let date = &query["X-Amz-Date"];
        native.datetime = native
            .datetime
            .replace_year(date[0..4].parse().unwrap())
            .unwrap()
            .replace_day(1)
            .unwrap()
            .replace_month(date[4..6].parse::<u8>().unwrap().try_into().unwrap())
            .unwrap()
            .replace_day(date[6..8].parse().unwrap())
            .unwrap()
            .replace_hour(date[9..11].parse().unwrap())
            .unwrap()
            .replace_minute(date[11..13].parse().unwrap())
            .unwrap()
            .replace_second(date[13..15].parse().unwrap())
            .unwrap()
            .replace_nanosecond(0)
            .unwrap();
        let expected = Url::parse(&native.presigned().await.unwrap()).unwrap();
        let expected_query: HashMap<_, _> = expected.query_pairs().into_owned().collect();
        assert_eq!(url.origin(), expected.origin());
        assert_eq!(url.path(), expected.path());
        assert_eq!(query, expected_query);
        assert_eq!(form.access_url, "https://cdn.keychat.io/hash_-123");
    }
}
