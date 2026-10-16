//! Guard every checkstate response used during CDK recovery, including a state
//! change between the admission preflight and CDK's own replay/check/restore.
use cdk::wallet::HttpTransport;
use cdk_common::{
    nuts::{nut22::AuthToken, CheckStateRequest, CheckStateResponse, State},
    HttpError, RawResponse,
};
use serde::{de::DeserializeOwned, Serialize};
use url::Url;

#[derive(Debug, Clone, Default)]
pub(super) struct RecoveryTransport {
    inner: cdk_http_client::Async,
}

#[tonic::async_trait]
impl HttpTransport for RecoveryTransport {
    fn with_proxy(
        &mut self,
        proxy: Url,
        matcher: Option<&str>,
        invalid: bool,
    ) -> Result<(), HttpError> {
        self.inner.with_proxy(proxy, matcher, invalid)
    }
    async fn http_get<R: DeserializeOwned>(
        &self,
        url: Url,
        auth: Option<AuthToken>,
    ) -> Result<R, HttpError> {
        self.inner.http_get(url, auth).await
    }
    async fn http_get_raw(
        &self,
        url: Url,
        auth: Option<AuthToken>,
    ) -> Result<RawResponse, HttpError> {
        self.inner.http_get_raw(url, auth).await
    }
    async fn http_post<P: Serialize + Send + Sync, R: DeserializeOwned>(
        &self,
        url: Url,
        auth: Option<AuthToken>,
        payload: &P,
    ) -> Result<R, HttpError> {
        if !url.path().ends_with("/v1/checkstate") {
            return self.inner.http_post(url, auth, payload).await;
        }
        let request: CheckStateRequest = serde_json::from_value(serde_json::to_value(payload)?)?;
        let response: CheckStateResponse = self.inner.http_post(url, auth, payload).await?;
        let mut expected: Vec<_> = request.ys.iter().map(ToString::to_string).collect();
        expected.sort();
        let mut actual: Vec<_> = response.states.iter().map(|s| s.y.to_string()).collect();
        actual.sort();
        if expected.is_empty()
            || actual != expected
            || !(response.states.iter().all(|s| s.state == State::Spent)
                || response.states.iter().all(|s| s.state == State::Unspent))
        {
            return Err(HttpError::Other(
                "recovery proof state is unresolved".into(),
            ));
        }
        Ok(serde_json::from_value(serde_json::to_value(response)?)?)
    }
    async fn http_post_form_raw<P: Serialize + Send + Sync>(
        &self,
        url: Url,
        auth: Option<AuthToken>,
        payload: &P,
    ) -> Result<RawResponse, HttpError> {
        self.inner.http_post_form_raw(url, auth, payload).await
    }
}
