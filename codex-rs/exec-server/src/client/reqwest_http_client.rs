//! `reqwest`-backed `HttpClient` implementation.
//!
//! This code runs wherever the real network request should originate:
//! - in a local environment, that means the orchestrator process
//! - in a remote environment, that means the remote runtime after the
//!   orchestrator has forwarded `http/request` over JSON-RPC

use std::error::Error as StdError;
use std::time::Duration;

use codex_client::build_reqwest_client_with_custom_ca;
use codex_client::is_allowed_chatgpt_host;
use codex_client::merge_chatgpt_cloudflare_cookie_header;
use codex_client::with_chatgpt_cloudflare_cookie_store;
use codex_exec_server_protocol::JSONRPCErrorError;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use reqwest::Method;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::AUTHORIZATION;
use reqwest::header::CONTENT_ENCODING;
use reqwest::header::CONTENT_LENGTH;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::COOKIE;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use reqwest::header::LOCATION;
use reqwest::header::PROXY_AUTHORIZATION;
use reqwest::header::REFERER;
use reqwest::header::TRANSFER_ENCODING;
use reqwest::header::WWW_AUTHENTICATE;

use super::HttpResponseBodyStream;
use super::response_body_stream::send_body_delta;
use crate::HttpClient;
use crate::client::ExecServerError;
use crate::protocol::HttpHeader;
use crate::protocol::HttpRequestBodyDeltaNotification;
use crate::protocol::HttpRequestParams;
use crate::protocol::HttpRequestResponse;
use crate::rpc::RpcNotificationSender;
use crate::rpc::internal_error;
use crate::rpc::invalid_params;

/// `HttpClient` implementation that performs the actual HTTP request with
/// `reqwest`.
#[derive(Clone, Default)]
pub struct ReqwestHttpClient;

/// Streaming response state held between the initial HTTP response and
/// downstream body-delta forwarding.
pub(crate) struct PendingReqwestHttpBodyStream {
    pub(crate) request_id: String,
    pub(crate) response: reqwest::Response,
}

/// Validates `http/request` parameters and runs the actual `reqwest` call used
/// by the exec-server route and the local [`HttpClient`] backend.
pub(crate) struct ReqwestHttpRequestRunner {
    client: reqwest::Client,
    timeout_ms: Option<u64>,
}

impl ReqwestHttpClient {
    fn build_client(timeout_ms: Option<u64>) -> Result<reqwest::Client, ExecServerError> {
        let builder = match timeout_ms {
            None => reqwest::Client::builder(),
            Some(timeout_ms) => {
                reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms))
            }
        };
        build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(builder))
            .map_err(|error| ExecServerError::HttpRequest(error.to_string()))
    }

    fn build_client_without_redirects(
        timeout_ms: Option<u64>,
    ) -> Result<reqwest::Client, ExecServerError> {
        let builder = match timeout_ms {
            None => reqwest::Client::builder(),
            Some(timeout_ms) => {
                reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms))
            }
        }
        .redirect(reqwest::redirect::Policy::none());
        build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(builder))
            .map_err(|error| ExecServerError::HttpRequest(error.to_string()))
    }
}

impl HttpClient for ReqwestHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        async move {
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms)
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let (response, _) = runner
                .run(HttpRequestParams {
                    stream_response: false,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            Ok(response)
        }
        .boxed()
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async move {
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms)
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let (response, pending_stream) = runner
                .run(HttpRequestParams {
                    stream_response: true,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let pending_stream = pending_stream.ok_or_else(|| {
                ExecServerError::Protocol(
                    "http request stream did not return a response body stream".to_string(),
                )
            })?;
            Ok((
                response,
                HttpResponseBodyStream::local(pending_stream.response),
            ))
        }
        .boxed()
    }
}

impl ReqwestHttpRequestRunner {
    pub(crate) fn new(timeout_ms: Option<u64>) -> Result<Self, JSONRPCErrorError> {
        let client = ReqwestHttpClient::build_client(timeout_ms)
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(Self { client, timeout_ms })
    }

    pub(crate) async fn run(
        &self,
        params: HttpRequestParams,
    ) -> Result<(HttpRequestResponse, Option<PendingReqwestHttpBodyStream>), JSONRPCErrorError>
    {
        let method = Method::from_bytes(params.method.as_bytes())
            .map_err(|error| invalid_params(format!("http/request method is invalid: {error}")))?;
        let url = Url::parse(&params.url)
            .map_err(|error| invalid_params(format!("http/request url is invalid: {error}")))?;
        match url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(invalid_params(format!(
                    "http/request only supports http and https URLs, got {scheme}"
                )));
            }
        }

        let headers = Self::build_headers(params.headers)?;
        let body = params.body.map(crate::protocol::ByteChunk::into_inner);
        let response = if should_refresh_chatgpt_cookies_on_redirects(&headers, &url) {
            self.send_with_chatgpt_cookie_redirects(method.clone(), url, headers, body)
                .await?
        } else {
            Self::send_once(&self.client, &method, url, headers, body.as_deref()).await?
        };
        let status = response.status().as_u16();
        let headers = Self::response_headers(response.headers());

        if params.stream_response {
            return Ok((
                HttpRequestResponse {
                    status,
                    headers,
                    body: Vec::new().into(),
                },
                Some(PendingReqwestHttpBodyStream {
                    request_id: params.request_id,
                    response,
                }),
            ));
        }

        let body = response.bytes().await.map_err(|error| {
            internal_error(format!(
                "failed to read http/request response body: {error}"
            ))
        })?;

        Ok((
            HttpRequestResponse {
                status,
                headers,
                body: body.to_vec().into(),
            },
            None,
        ))
    }

    async fn send_with_chatgpt_cookie_redirects(
        &self,
        mut method: Method,
        mut url: Url,
        mut headers: HeaderMap,
        mut body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, JSONRPCErrorError> {
        const MAX_REDIRECTS: usize = 10;

        // Reqwest's public redirect policy can decide whether to follow but cannot mutate the next
        // attempt's headers. Mirror its default redirect behavior here so explicit Cookie headers
        // can be combined with the latest allowlisted Cloudflare cookies on every attempt.
        let client = ReqwestHttpClient::build_client_without_redirects(self.timeout_ms)
            .map_err(|error| internal_error(error.to_string()))?;
        let mut redirect_count = 0;

        loop {
            let mut attempt_headers = headers.clone();
            merge_chatgpt_cloudflare_cookie_header(&mut attempt_headers, &url);
            let response = Self::send_once(
                &client,
                &method,
                url.clone(),
                attempt_headers,
                body.as_deref(),
            )
            .await?;
            let status = response.status();
            if !matches!(
                status,
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                return Ok(response);
            }

            let Some(next_url) = redirect_url(&response)? else {
                return Ok(response);
            };
            if redirect_count == MAX_REDIRECTS {
                return Err(internal_error(
                    "http/request failed: too many redirects".to_string(),
                ));
            }
            redirect_count += 1;

            match status {
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND if method == Method::POST => {
                    method = Method::GET;
                    body = None;
                    drop_payload_headers(&mut headers);
                }
                StatusCode::SEE_OTHER => {
                    if method != Method::HEAD {
                        method = Method::GET;
                    }
                    body = None;
                    drop_payload_headers(&mut headers);
                }
                StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT => {}
                _ => unreachable!("redirect statuses were checked above"),
            }

            remove_sensitive_headers_on_cross_host_redirect(&mut headers, &url, &next_url);
            set_referer_header(&mut headers, &url, &next_url);
            url = next_url;
        }
    }

    async fn send_once(
        client: &reqwest::Client,
        method: &Method,
        url: Url,
        headers: HeaderMap,
        body: Option<&[u8]>,
    ) -> Result<reqwest::Response, JSONRPCErrorError> {
        let mut request = client.request(method.clone(), url).headers(headers);
        if let Some(body) = body {
            request = request.body(body.to_vec());
        }

        match request.send().await {
            Ok(response) => Ok(response),
            Err(error) => {
                let error_message = error.to_string();
                log_send_error(method, error);
                Err(internal_error(format!(
                    "http/request failed: {error_message}"
                )))
            }
        }
    }

    pub(crate) async fn stream_body(
        pending_stream: PendingReqwestHttpBodyStream,
        notifications: RpcNotificationSender,
    ) {
        let PendingReqwestHttpBodyStream {
            request_id,
            response,
        } = pending_stream;
        let mut seq = 1;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    if !send_body_delta(
                        &notifications,
                        HttpRequestBodyDeltaNotification {
                            request_id: request_id.clone(),
                            seq,
                            delta: bytes.to_vec().into(),
                            done: false,
                            error: None,
                        },
                    )
                    .await
                    {
                        return;
                    }
                    seq += 1;
                }
                Err(error) => {
                    let _ = send_body_delta(
                        &notifications,
                        HttpRequestBodyDeltaNotification {
                            request_id,
                            seq,
                            delta: Vec::new().into(),
                            done: true,
                            error: Some(error.to_string()),
                        },
                    )
                    .await;
                    return;
                }
            }
        }

        let _ = send_body_delta(
            &notifications,
            HttpRequestBodyDeltaNotification {
                request_id,
                seq,
                delta: Vec::new().into(),
                done: true,
                error: None,
            },
        )
        .await;
    }

    fn build_headers(headers: Vec<HttpHeader>) -> Result<HeaderMap, JSONRPCErrorError> {
        let mut header_map = HeaderMap::new();
        for header in headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
                invalid_params(format!("http/request header name is invalid: {error}"))
            })?;
            let value = HeaderValue::from_str(&header.value).map_err(|error| {
                invalid_params(format!(
                    "http/request header value is invalid for {}: {error}",
                    header.name
                ))
            })?;
            header_map.append(name, value);
        }
        Ok(header_map)
    }

    fn response_headers(headers: &HeaderMap) -> Vec<HttpHeader> {
        headers
            .iter()
            .filter_map(|(name, value)| {
                Some(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_str().ok()?.to_string(),
                })
            })
            .collect()
    }
}

fn should_refresh_chatgpt_cookies_on_redirects(headers: &HeaderMap, url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(is_allowed_chatgpt_host)
        && headers.contains_key(COOKIE)
}

fn redirect_url(response: &reqwest::Response) -> Result<Option<Url>, JSONRPCErrorError> {
    let Some(location) = response.headers().get(LOCATION) else {
        return Ok(None);
    };
    let Ok(location) = location.to_str() else {
        return Ok(None);
    };
    let Ok(next_url) = response.url().join(location) else {
        return Ok(None);
    };
    if !matches!(next_url.scheme(), "http" | "https") {
        return Err(internal_error(format!(
            "http/request redirect has unsupported URL scheme: {}",
            next_url.scheme()
        )));
    }
    Ok(Some(next_url))
}

fn drop_payload_headers(headers: &mut HeaderMap) {
    for header in [
        CONTENT_TYPE,
        CONTENT_LENGTH,
        CONTENT_ENCODING,
        TRANSFER_ENCODING,
    ] {
        headers.remove(header);
    }
}

fn remove_sensitive_headers_on_cross_host_redirect(
    headers: &mut HeaderMap,
    previous_url: &Url,
    next_url: &Url,
) {
    let cross_host = next_url.host_str() != previous_url.host_str()
        || next_url.port_or_known_default() != previous_url.port_or_known_default();
    if cross_host {
        for header in [
            AUTHORIZATION,
            COOKIE,
            HeaderName::from_static("cookie2"),
            PROXY_AUTHORIZATION,
            WWW_AUTHENTICATE,
        ] {
            headers.remove(header);
        }
    }
}

fn set_referer_header(headers: &mut HeaderMap, previous_url: &Url, next_url: &Url) {
    if next_url.scheme() == "http" && previous_url.scheme() == "https" {
        return;
    }

    let mut referer = previous_url.clone();
    let _ = referer.set_username("");
    let _ = referer.set_password(None);
    referer.set_fragment(None);
    if let Ok(referer) = HeaderValue::from_str(referer.as_str()) {
        headers.insert(REFERER, referer);
    }
}

fn log_send_error(method: &Method, error: reqwest::Error) {
    let error = error.without_url();
    let source_chain = error_source_chain(&error);
    tracing::warn!(
        http_method = method.as_str(),
        error_is_timeout = error.is_timeout(),
        error_is_connect = error.is_connect(),
        error = %error,
        error_sources = ?source_chain,
        "http/request send failed"
    );
}

fn error_source_chain(error: &reqwest::Error) -> Option<String> {
    let mut sources = Vec::new();
    let mut source = error.source();
    while let Some(error) = source {
        sources.push(error.to_string());
        source = error.source();
    }
    (!sources.is_empty()).then(|| sources.join(": "))
}
