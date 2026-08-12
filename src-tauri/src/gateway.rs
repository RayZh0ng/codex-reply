use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use std::process::Command;

use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Extension, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{future::join_all, StreamExt, TryStreamExt};
use ipnet::IpNet;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use reqwest::{redirect::Policy, Client, NoProxy, Proxy};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, Notify};
use url::Url;

use crate::{
    codex_environment::default_codex_home,
    database::Repository,
    database::StoredProfile,
    domain::{
        ApiServiceTestReport, CodexAuthMode, GatewayHealthProviderSummary, GatewayHealthSummary,
        GatewayModelMapping, GatewayNetworkAddress, GatewayProvider, GatewayRequestMetricSummary,
        GatewayStatus, GatewayWireApi, MaskedProfile, ProfileKind,
        GATEWAY_CODEX_CLIENT_KEY_REF_SETTING, GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING,
    },
    error::{AppError, AppResult},
    oauth_credentials::{CredentialAccess, OAuthCredentialStore},
    profiles::{
        candidates_for_model, cool_down_profile, mark_profile_validation_invalid,
        mark_profile_validation_unknown, timestamp_ms,
    },
    secrets::SecretStore,
};

#[derive(Clone)]
struct GatewayApiState {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    cidrs: Vec<IpNet>,
    oauth_responses_url: Url,
    http_client: Client,
    auth_cache: Arc<GatewayAuthCache>,
    concurrency: Arc<ProfileConcurrencyManager>,
    telemetry: Arc<GatewayTelemetry>,
    certificate_ready: bool,
    codex_home: PathBuf,
    scheduler: Arc<Mutex<WeightedScheduler>>,
    affinities: Arc<Mutex<HashMap<String, ResponseAffinity>>>,
}

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const GATEWAY_FIRST_RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);
const GATEWAY_PROXY_MODE_SETTING: &str = "gateway_upstream_proxy_mode";
const GATEWAY_PROXY_DISPLAY_SETTING: &str = "gateway_upstream_proxy_display";
const GATEWAY_PROXY_SECRET_REF: &str = "gateway:upstream-proxy-url";
const GATEWAY_LAST_ERROR_SETTING: &str = "gateway_upstream_last_error";
const GATEWAY_ERROR_PROXY_CONFIG: &str = "proxy_config_error";
const GATEWAY_ERROR_FIRST_RESPONSE: &str = "upstream_first_response_failed";
const GATEWAY_ERROR_STREAM_INTERRUPTED: &str = "upstream_stream_interrupted";
const AFFINITY_TTL_MS: i64 = 60 * 60 * 1000;
const MAX_AFFINITIES: usize = 2_048;
/// Axum's JSON extractor defaults to 2 MiB, which is too small for Codex
/// requests containing a long conversation, tool output, or base64 images.
/// Keep the override bounded because JSON bodies are buffered before routing.
const GATEWAY_JSON_BODY_LIMIT_BYTES: usize = 64 * 1024 * 1024;

#[cfg(test)]
static ARGON2_VERIFY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static AUTH_COUNTER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, PartialEq, Eq)]
enum GatewayUpstreamProxy {
    System,
    Manual { url: String },
    Disabled,
}

#[derive(Clone)]
struct ResponseAffinity {
    profile_id: String,
    expires_at_ms: i64,
}

#[derive(Clone, Debug)]
struct AuthorizedClient {
    codex_managed: bool,
    auth_mode: &'static str,
    auth_latency_ms: i64,
    request_started: Instant,
}

type TokenDigest = [u8; 32];

#[derive(Clone)]
struct DirectOAuthAuthSnapshot {
    access_token_digest: TokenDigest,
}

#[derive(Clone)]
struct CachedClientAuth {
    client: AuthorizedClient,
    expires_at: Instant,
}

struct GatewayAuthCacheState {
    oauth: Option<DirectOAuthAuthSnapshot>,
    client_keys: HashMap<TokenDigest, CachedClientAuth>,
    rejected: VecDeque<(TokenDigest, Instant)>,
    last_oauth_refresh: Instant,
}

struct GatewayAuthCache {
    state: Mutex<GatewayAuthCacheState>,
}

impl GatewayAuthCache {
    fn new() -> Self {
        Self {
            state: Mutex::new(GatewayAuthCacheState {
                oauth: None,
                client_keys: HashMap::new(),
                rejected: VecDeque::new(),
                last_oauth_refresh: Instant::now() - Duration::from_secs(60),
            }),
        }
    }

    fn refresh_oauth(&self, repository: &Repository, codex_home: &std::path::Path) {
        let snapshot = direct_oauth_auth_snapshot(repository, codex_home);
        if let Ok(mut state) = self.state.lock() {
            state.oauth = snapshot;
            state.last_oauth_refresh = Instant::now();
        }
    }

    fn refresh_oauth_if_due(&self, repository: &Repository, codex_home: &std::path::Path) {
        let due = self
            .state
            .lock()
            .map(|state| state.last_oauth_refresh.elapsed() >= Duration::from_secs(5))
            .unwrap_or(false);
        if due {
            self.refresh_oauth(repository, codex_home);
        }
    }

    fn oauth_matches(&self, digest: &TokenDigest) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.oauth.clone())
            .is_some_and(|snapshot| bool::from(snapshot.access_token_digest.ct_eq(digest)))
    }

    fn oauth_ready(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.oauth.is_some())
            .unwrap_or(false)
    }

    fn cached_client(&self, digest: &TokenDigest) -> Option<AuthorizedClient> {
        let mut state = self.state.lock().ok()?;
        let now = Instant::now();
        state
            .client_keys
            .retain(|_, cached| cached.expires_at > now);
        state
            .client_keys
            .get(digest)
            .filter(|cached| cached.expires_at > now)
            .map(|cached| cached.client.clone())
    }

    fn cache_client(&self, digest: TokenDigest, client: AuthorizedClient) {
        if let Ok(mut state) = self.state.lock() {
            if state.client_keys.len() >= 256 {
                state.client_keys.clear();
            }
            state.client_keys.insert(
                digest,
                CachedClientAuth {
                    client,
                    expires_at: Instant::now() + Duration::from_secs(10 * 60),
                },
            );
        }
    }

    fn recently_rejected(&self, digest: &TokenDigest) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let now = Instant::now();
        while state
            .rejected
            .front()
            .is_some_and(|(_, expires)| *expires <= now)
        {
            state.rejected.pop_front();
        }
        state
            .rejected
            .iter()
            .any(|(candidate, _)| bool::from(candidate.ct_eq(digest)))
    }

    fn cache_rejected(&self, digest: TokenDigest) {
        if let Ok(mut state) = self.state.lock() {
            while state.rejected.len() >= 512 {
                state.rejected.pop_front();
            }
            state
                .rejected
                .push_back((digest, Instant::now() + Duration::from_secs(5)));
        }
    }

    fn invalidate_client_keys(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.client_keys.clear();
            state.rejected.clear();
        }
    }
}

struct ProfileGateState {
    active: usize,
    queued: usize,
}

struct ProfileGate {
    state: Mutex<ProfileGateState>,
    notify: Notify,
}

struct ProfileConcurrencyManager {
    gates: Mutex<HashMap<String, Arc<ProfileGate>>>,
    active: AtomicUsize,
    queued: AtomicUsize,
}

#[derive(Debug)]
enum ProfileAcquireError {
    QueueFull,
    QueueTimeout,
}

struct ProfilePermit {
    gate: Arc<ProfileGate>,
    manager: Arc<ProfileConcurrencyManager>,
}

struct ProfileQueueWaiter {
    gate: Arc<ProfileGate>,
    manager: Arc<ProfileConcurrencyManager>,
    active: bool,
}

impl ProfileQueueWaiter {
    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ProfileQueueWaiter {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut state) = self.gate.state.lock() {
            state.queued = state.queued.saturating_sub(1);
        }
        self.manager.queued.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for ProfilePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.active = state.active.saturating_sub(1);
        }
        self.manager.active.fetch_sub(1, Ordering::Relaxed);
        self.gate.notify.notify_one();
    }
}

impl ProfileConcurrencyManager {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gates: Mutex::new(HashMap::new()),
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
        })
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.active.load(Ordering::Relaxed),
            self.queued.load(Ordering::Relaxed),
        )
    }

    async fn acquire(
        self: &Arc<Self>,
        profile: &MaskedProfile,
    ) -> Result<(ProfilePermit, i64), ProfileAcquireError> {
        let max_active = profile.max_concurrency.max(1) as usize;
        let max_queue = profile.max_queue_depth.max(0) as usize;
        let timeout = Duration::from_millis(profile.queue_timeout_ms.max(1) as u64);
        let gate = {
            let mut gates = self
                .gates
                .lock()
                .map_err(|_| ProfileAcquireError::QueueFull)?;
            gates
                .entry(profile.id.clone())
                .or_insert_with(|| {
                    Arc::new(ProfileGate {
                        state: Mutex::new(ProfileGateState {
                            active: 0,
                            queued: 0,
                        }),
                        notify: Notify::new(),
                    })
                })
                .clone()
        };
        let started = Instant::now();
        let mut waiter: Option<ProfileQueueWaiter> = None;
        loop {
            {
                let mut state = gate
                    .state
                    .lock()
                    .map_err(|_| ProfileAcquireError::QueueFull)?;
                if state.active < max_active {
                    state.active += 1;
                    if let Some(waiter) = waiter.as_mut() {
                        state.queued = state.queued.saturating_sub(1);
                        self.queued.fetch_sub(1, Ordering::Relaxed);
                        waiter.disarm();
                    }
                    self.active.fetch_add(1, Ordering::Relaxed);
                    return Ok((
                        ProfilePermit {
                            gate: gate.clone(),
                            manager: self.clone(),
                        },
                        started.elapsed().as_millis() as i64,
                    ));
                }
                if waiter.is_none() {
                    if state.queued >= max_queue {
                        return Err(ProfileAcquireError::QueueFull);
                    }
                    state.queued += 1;
                    self.queued.fetch_add(1, Ordering::Relaxed);
                    waiter = Some(ProfileQueueWaiter {
                        gate: gate.clone(),
                        manager: self.clone(),
                        active: true,
                    });
                }
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(ProfileAcquireError::QueueTimeout);
            }
            if tokio::time::timeout(timeout - elapsed, gate.notify.notified())
                .await
                .is_err()
            {
                return Err(ProfileAcquireError::QueueTimeout);
            }
        }
    }
}

struct GatewayTelemetry {
    sender: Mutex<Option<mpsc::Sender<GatewayRequestMetricSummary>>>,
    flush_complete: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    dropped: AtomicI64,
}

impl GatewayTelemetry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sender: Mutex::new(None),
            flush_complete: Mutex::new(None),
            dropped: AtomicI64::new(0),
        })
    }

    fn start(&self, repository: Arc<Repository>) {
        let (sender, mut receiver) = mpsc::channel::<GatewayRequestMetricSummary>(2_048);
        let (complete_sender, complete_receiver) = std::sync::mpsc::channel();
        if let Ok(mut slot) = self.sender.lock() {
            *slot = Some(sender);
        }
        if let Ok(mut slot) = self.flush_complete.lock() {
            *slot = Some(complete_receiver);
        }
        tokio::spawn(async move {
            let _ = repository.prune_gateway_request_metrics();
            let mut batch = Vec::with_capacity(100);
            let mut inserted_since_prune = 0_usize;
            let mut flush_interval = tokio::time::interval(Duration::from_secs(1));
            flush_interval.tick().await;
            loop {
                tokio::select! {
                    next = receiver.recv() => match next {
                        Some(metric) => {
                        batch.push(metric);
                            if batch.len() >= 100 {
                                inserted_since_prune +=
                                    flush_gateway_metric_batch(&repository, &mut batch);
                            }
                        }
                        None => {
                            inserted_since_prune +=
                                flush_gateway_metric_batch(&repository, &mut batch);
                            if inserted_since_prune > 0 {
                                let _ = repository.prune_gateway_request_metrics();
                            }
                            let _ = complete_sender.send(());
                            break;
                        }
                    },
                    _ = flush_interval.tick() => {
                        inserted_since_prune +=
                            flush_gateway_metric_batch(&repository, &mut batch);
                    }
                }
                if inserted_since_prune >= 500 {
                    let _ = repository.prune_gateway_request_metrics();
                    inserted_since_prune = 0;
                }
            }
        });
    }

    fn stop(&self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        if let Ok(mut receiver) = self.flush_complete.lock() {
            if let Some(receiver) = receiver.take() {
                let _ = receiver.recv_timeout(Duration::from_secs(2));
            }
        }
    }

    fn record(&self, metric: GatewayRequestMetricSummary) {
        let sender = self.sender.lock().ok().and_then(|sender| sender.clone());
        if sender.is_none_or(|sender| sender.try_send(metric).is_err()) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn flush_gateway_metric_batch(
    repository: &Repository,
    batch: &mut Vec<GatewayRequestMetricSummary>,
) -> usize {
    let count = batch.len();
    if count > 0 && repository.record_gateway_request_metrics(batch).is_ok() {
        batch.clear();
        count
    } else {
        0
    }
}

#[derive(Default)]
struct WeightedScheduler {
    scores: HashMap<String, HashMap<String, i64>>,
}

/// Fetches the model catalogue using only the provider's documented discovery
/// endpoint. Providers that do not expose one return an explicit unavailable
/// result rather than guessed model names.
pub async fn discover_profile_models(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
) -> AppResult<MaskedProfile> {
    let mut stored = repository.profile(id)?;
    if stored.profile.kind != ProfileKind::ApiKey {
        return Err(AppError::ValidationFailed);
    }
    let base_url = stored
        .profile
        .base_url
        .clone()
        .ok_or(AppError::ValidationFailed)?;
    let secret_ref = stored
        .secret_ref
        .clone()
        .ok_or(AppError::ValidationFailed)?;
    let key = secrets.get(&secret_ref).await?;
    let report = test_api_service(&stored.profile.provider, &base_url, &key).await?;
    if report.status != "verified" {
        if report.category == "authentication" {
            mark_profile_validation_invalid(repository, id, report.message)?;
        } else {
            mark_profile_validation_unknown(repository, id, report.message)?;
        }
        return Err(AppError::UpstreamUnavailable);
    }
    let mappings = if stored.profile.model_mappings.is_empty() {
        report
            .models
            .iter()
            .map(|model| GatewayModelMapping {
                model: model.clone(),
                upstream_model: model.clone(),
                display_name: None,
                context_window: None,
            })
            .collect::<Vec<_>>()
    } else {
        stored.profile.model_mappings.clone()
    };
    stored.profile.models = mappings
        .iter()
        .map(|mapping| mapping.model.clone())
        .collect();
    stored.profile.model_mappings = mappings;
    stored.profile.health = "healthy".to_owned();
    stored.profile.validation_status = "valid".to_owned();
    stored.profile.validated_at_ms = Some(timestamp_ms());
    stored.profile.validation_message = Some(report.message);
    repository.update_profile(&stored)?;
    Ok(stored.profile)
}

pub async fn test_api_service(
    provider: &GatewayProvider,
    base_url: &str,
    key: &str,
) -> AppResult<ApiServiceTestReport> {
    let base_url = normalize_base_url(provider, base_url)?;
    let route = model_discovery_route(provider);
    let endpoint = build_upstream_url(&base_url, route)?;
    let endpoint_text = endpoint.to_string();
    let started = Instant::now();
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| AppError::UpstreamUnavailable)?;
    let response = match discovery_request(client.get(endpoint), provider, key)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let category = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connection"
            } else if error.is_request() {
                "tls_or_url"
            } else {
                "network"
            };
            return Ok(ApiServiceTestReport {
                status: "failed".to_owned(),
                category: category.to_owned(),
                endpoint: endpoint_text,
                message: format!("无法连接上游：{category}。请检查 Base URL、网络与 TLS 证书。"),
                http_status: None,
                latency_ms: started.elapsed().as_millis() as i64,
                model_count: 0,
                models: Vec::new(),
            });
        }
    };
    let status = response.status();
    let latency_ms = started.elapsed().as_millis() as i64;
    if !status.is_success() {
        let category = match status.as_u16() {
            401 | 403 => "authentication",
            404 => "endpoint_not_found",
            429 => "rate_limited",
            value if value >= 500 => "upstream_server",
            _ => "protocol",
        };
        return Ok(ApiServiceTestReport {
            status: "failed".to_owned(),
            category: category.to_owned(),
            endpoint: endpoint_text,
            message: format!("上游返回 HTTP {}（{category}）。", status.as_u16()),
            http_status: Some(status.as_u16()),
            latency_ms,
            model_count: 0,
            models: Vec::new(),
        });
    }
    let body_text = match response.text().await {
        Ok(text) => text,
        Err(error) => {
            let category = if error.is_timeout() {
                "timeout"
            } else if error.is_decode() {
                "json"
            } else {
                "network"
            };
            return Ok(ApiServiceTestReport {
                status: "failed".to_owned(),
                category: category.to_owned(),
                endpoint: endpoint_text,
                message: format!("上游模型目录响应读取失败（{category}）。"),
                http_status: Some(status.as_u16()),
                latency_ms,
                model_count: 0,
                models: Vec::new(),
            });
        }
    };
    let body = match serde_json::from_str::<Value>(&body_text) {
        Ok(body) => body,
        Err(_) => {
            return Ok(ApiServiceTestReport {
                status: "failed".to_owned(),
                category: "json".to_owned(),
                endpoint: endpoint_text,
                message: "上游返回的模型目录不是有效 JSON。".to_owned(),
                http_status: Some(status.as_u16()),
                latency_ms,
                model_count: 0,
                models: Vec::new(),
            });
        }
    };
    let models = model_ids_from_provider_response(provider, &body);
    if models.is_empty() {
        return Ok(ApiServiceTestReport {
            status: "failed".to_owned(),
            category: "protocol".to_owned(),
            endpoint: endpoint_text,
            message: "上游响应成功，但未返回可识别的模型目录。".to_owned(),
            http_status: Some(status.as_u16()),
            latency_ms,
            model_count: 0,
            models,
        });
    }
    Ok(ApiServiceTestReport {
        status: "verified".to_owned(),
        category: "ok".to_owned(),
        endpoint: endpoint_text,
        message: "上游连接与认证已验证。".to_owned(),
        http_status: Some(status.as_u16()),
        latency_ms,
        model_count: models.len(),
        models,
    })
}

pub fn normalize_base_url(provider: &GatewayProvider, base_url: &str) -> AppResult<String> {
    let mut parsed = Url::parse(base_url.trim()).map_err(|_| AppError::ValidationFailed)?;
    if parsed.host_str().is_none() || !matches!(parsed.scheme(), "https" | "http") {
        return Err(AppError::ValidationFailed);
    }
    let path = parsed.path().trim_end_matches('/');
    let target = match provider {
        GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible => {
            if path.ends_with("/v1") {
                path.to_owned()
            } else {
                format!("{path}/v1")
            }
        }
        GatewayProvider::Anthropic => path.trim_end_matches("/v1").to_owned(),
        GatewayProvider::Gemini => path.trim_end_matches("/v1beta").to_owned(),
        GatewayProvider::Ollama => path.trim_end_matches("/api").to_owned(),
    };
    parsed.set_path(if target.is_empty() { "/" } else { &target });
    Ok(parsed.to_string().trim_end_matches('/').to_owned())
}

fn model_discovery_route(provider: &GatewayProvider) -> &'static str {
    match provider {
        GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible => "models",
        GatewayProvider::Anthropic => "v1/models",
        GatewayProvider::Gemini => "v1beta/models",
        GatewayProvider::Ollama => "api/tags",
    }
}

fn discovery_request(
    request: reqwest::RequestBuilder,
    provider: &GatewayProvider,
    key: &str,
) -> reqwest::RequestBuilder {
    match provider {
        GatewayProvider::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        GatewayProvider::Gemini => request.header("x-goog-api-key", key),
        _ => request.bearer_auth(key),
    }
}

fn model_ids_from_provider_response(provider: &GatewayProvider, body: &Value) -> Vec<String> {
    let mut arrays = Vec::new();
    match provider {
        GatewayProvider::Ollama | GatewayProvider::Gemini => {
            if let Some(values) = body.get("models").and_then(Value::as_array) {
                arrays.push(values);
            }
            if let Some(values) = body.get("data").and_then(Value::as_array) {
                arrays.push(values);
            }
        }
        _ => {
            if let Some(values) = body.get("data").and_then(Value::as_array) {
                arrays.push(values);
            }
            if let Some(values) = body.get("models").and_then(Value::as_array) {
                arrays.push(values);
            }
        }
    }
    if let Some(values) = body.as_array() {
        arrays.push(values);
    }
    let mut models = arrays
        .into_iter()
        .flat_map(|values| values.iter())
        .filter_map(model_id_from_value)
        .filter(|name| !name.trim().is_empty())
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models
}

fn model_id_from_value(value: &Value) -> Option<String> {
    let name = value.as_str().or_else(|| {
        value
            .get("id")
            .or_else(|| value.get("name"))
            .or_else(|| value.get("model"))
            .or_else(|| value.get("slug"))
            .and_then(Value::as_str)
    })?;
    let name = name.trim().trim_start_matches("models/").trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

struct GatewayRuntime {
    handle: axum_server::Handle,
}

pub struct GatewayManager {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    certificate_dir: PathBuf,
    runtime: Mutex<Option<GatewayRuntime>>,
    scheduler: Arc<Mutex<WeightedScheduler>>,
    affinities: Arc<Mutex<HashMap<String, ResponseAffinity>>>,
    auth_cache: Arc<GatewayAuthCache>,
    concurrency: Arc<ProfileConcurrencyManager>,
    telemetry: Arc<GatewayTelemetry>,
}

impl GatewayManager {
    pub fn new(
        repository: Arc<Repository>,
        secrets: Arc<dyn SecretStore>,
        oauth_credentials: Arc<OAuthCredentialStore>,
        certificate_dir: PathBuf,
    ) -> Self {
        Self {
            repository,
            secrets,
            oauth_credentials,
            certificate_dir,
            runtime: Mutex::new(None),
            scheduler: Arc::new(Mutex::new(WeightedScheduler::default())),
            affinities: Arc::new(Mutex::new(HashMap::new())),
            auth_cache: Arc::new(GatewayAuthCache::new()),
            concurrency: ProfileConcurrencyManager::new(),
            telemetry: GatewayTelemetry::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.runtime
            .lock()
            .map(|runtime| runtime.is_some())
            .unwrap_or(false)
    }

    pub fn certificate_ready(&self) -> bool {
        self.certificate_dir.join("gateway-ca.pem").exists()
    }

    pub fn status(&self) -> AppResult<GatewayStatus> {
        if let Ok(home) = default_codex_home() {
            self.auth_cache
                .refresh_oauth_if_due(&self.repository, &home);
        }
        let mut status = self.repository.gateway_settings(
            self.is_running(),
            self.certificate_ready(),
            available_lan_addresses(),
        )?;
        let (active, queued) = self.concurrency.counts();
        if let Some(route) = status.direct_route.as_mut() {
            if route.route_mode == "relay_bridge" && !self.auth_cache.oauth_ready() {
                route.oauth_ready = false;
                route.status = if route.credential_ready {
                    "degraded".to_owned()
                } else {
                    "unavailable".to_owned()
                };
            }
        }
        status.active_requests = active;
        status.queued_requests = queued;
        Ok(status)
    }

    pub fn refresh_runtime_auth(&self) -> AppResult<()> {
        let home = default_codex_home()?;
        self.auth_cache.refresh_oauth(&self.repository, &home);
        Ok(())
    }

    pub fn invalidate_client_key_cache(&self) {
        self.auth_cache.invalidate_client_keys();
    }

    pub fn metrics_snapshot(
        &self,
        window_minutes: i64,
    ) -> AppResult<crate::domain::MetricsSnapshot> {
        let mut metrics = self
            .repository
            .gateway_performance(crate::domain::GatewayPerformanceInput { window_minutes })?;
        let (active, queued) = self.concurrency.counts();
        metrics.active_requests = active as i64;
        metrics.queued_requests = queued as i64;
        metrics.telemetry_dropped = self.telemetry.dropped.load(Ordering::Relaxed);
        Ok(metrics)
    }

    pub async fn start(&self) -> AppResult<GatewayStatus> {
        if self.is_running() {
            return Err(AppError::Conflict);
        }
        for mut profile in self
            .repository
            .list_profiles()?
            .into_iter()
            .filter(|profile| {
                profile.profile.enabled
                    && profile.profile.in_pool
                    && profile.profile.kind == ProfileKind::CodexOauth
                    && profile.profile.auth_mode == crate::domain::CodexAuthMode::OAuth
                    && profile.profile.validation_status != "invalid"
            })
        {
            match self
                .oauth_credentials
                .current_or_refresh(&profile, CredentialAccess::UserInitiated)
                .await
            {
                Ok(_) => {
                    if profile.profile.health == "reauthorization_required" {
                        profile.profile.health = "healthy".to_owned();
                        let _ = self.repository.update_profile(&profile);
                    }
                }
                Err(_) => {
                    profile.profile.health = "reauthorization_required".to_owned();
                    let _ = self.repository.update_profile(&profile);
                }
            }
        }
        let mut settings = self.repository.gateway_settings(
            false,
            self.certificate_ready(),
            available_lan_addresses(),
        )?;
        if settings.bind_mode == "loopback" {
            settings.bind_address = "127.0.0.1".to_owned();
            settings.cidrs.clear();
        } else if settings.bind_mode != "lan"
            || settings
                .bind_address
                .parse::<IpAddr>()
                .ok()
                .is_none_or(|address| !is_private_address(address))
        {
            let address = default_lan_address().ok_or(AppError::ForbiddenNetworkTarget)?;
            self.repository.update_gateway_settings(
                "lan",
                &address.address,
                settings.port,
                &settings.cidrs,
            )?;
            settings.bind_mode = "lan".to_owned();
            settings.bind_address = address.address;
        }
        let address: IpAddr = settings
            .bind_address
            .parse()
            .map_err(|_| AppError::ValidationFailed)?;
        validate_binding(&settings.bind_mode, address, &settings.cidrs)?;
        let cidrs = if settings.bind_mode == "loopback" {
            Vec::new()
        } else {
            settings
                .cidrs
                .iter()
                .map(|value| {
                    value
                        .parse::<IpNet>()
                        .map_err(|_| AppError::ValidationFailed)
                })
                .collect::<AppResult<Vec<_>>>()?
        };
        let (certificate_pem, key_pem) = self.ensure_local_certificates(address).await?;
        let upstream_proxy =
            match load_gateway_upstream_proxy(&self.repository, self.secrets.clone()).await {
                Ok(proxy) => proxy,
                Err(error) => {
                    record_gateway_upstream_error(&self.repository, GATEWAY_ERROR_PROXY_CONFIG);
                    return Err(error);
                }
            };
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(
            certificate_pem.into_bytes(),
            key_pem.into_bytes(),
        )
        .await
        .map_err(|_| AppError::Internal)?;
        let http_client = gateway_http_client(&upstream_proxy).map_err(|_| {
            record_gateway_upstream_error(&self.repository, GATEWAY_ERROR_PROXY_CONFIG);
            AppError::UpstreamUnavailable
        })?;
        let codex_home = default_codex_home()?;
        self.auth_cache.refresh_oauth(&self.repository, &codex_home);
        self.telemetry.start(self.repository.clone());
        let api_state = GatewayApiState {
            repository: self.repository.clone(),
            secrets: self.secrets.clone(),
            oauth_credentials: self.oauth_credentials.clone(),
            cidrs,
            oauth_responses_url: Url::parse(CODEX_RESPONSES_URL).map_err(|_| AppError::Internal)?,
            http_client,
            auth_cache: self.auth_cache.clone(),
            concurrency: self.concurrency.clone(),
            telemetry: self.telemetry.clone(),
            certificate_ready: self.certificate_ready(),
            codex_home,
            scheduler: self.scheduler.clone(),
            affinities: self.affinities.clone(),
        };
        let app = gateway_router(api_state);
        let handle = axum_server::Handle::new();
        let run_handle = handle.clone();
        let socket = SocketAddr::new(address, settings.port);
        let listener = TcpListener::bind(socket).map_err(|_| AppError::Conflict)?;
        tokio::spawn(async move {
            let _ = axum_server::from_tcp_rustls(listener, config)
                .handle(run_handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await;
        });
        self.runtime
            .lock()
            .map_err(|_| AppError::Internal)?
            .replace(GatewayRuntime { handle });
        self.status()
    }

    pub fn stop(&self) -> AppResult<GatewayStatus> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| AppError::Internal)?
            .take()
            .ok_or(AppError::GatewayNotRunning)?;
        runtime
            .handle
            .graceful_shutdown(Some(Duration::from_secs(2)));
        self.telemetry.stop();
        self.status()
    }

    pub fn export_ca(&self, destination: &std::path::Path) -> AppResult<()> {
        let source = self.certificate_dir.join("gateway-ca.pem");
        if !source.exists() {
            return Err(AppError::NotFound);
        }
        std::fs::copy(source, destination).map_err(|_| AppError::RuntimeUnavailable)?;
        Ok(())
    }

    pub fn trust_ca_in_system_store(&self) -> AppResult<()> {
        let source = self.certificate_dir.join("gateway-ca.pem");
        if !source.exists() {
            return Err(AppError::NotFound);
        }
        #[cfg(target_os = "macos")]
        {
            let home = std::env::var_os("HOME").ok_or(AppError::RuntimeUnavailable)?;
            let keychain = PathBuf::from(home).join("Library/Keychains/login.keychain-db");
            let output = Command::new("/usr/bin/security")
                .args(["add-trusted-cert", "-d", "-r", "trustRoot", "-k"])
                .arg(keychain)
                .arg(source)
                .output()
                .map_err(|_| AppError::RuntimeUnavailable)?;
            if output.status.success() {
                return Ok(());
            }
            Err(AppError::KeychainInteractionRequired)
        }
        #[cfg(target_os = "windows")]
        {
            let output = Command::new("certutil.exe")
                .args(["-user", "-addstore", "Root"])
                .arg(source)
                .output()
                .map_err(|_| AppError::RuntimeUnavailable)?;
            if output.status.success() {
                return Ok(());
            }
            Err(AppError::CaTrustFailed)
        }
        #[cfg(target_os = "linux")]
        {
            let has_update_ca = Command::new("sh")
                .args(["-lc", "command -v update-ca-certificates"])
                .output()
                .is_ok_and(|output| output.status.success());
            let has_update_ca_trust = Command::new("sh")
                .args(["-lc", "command -v update-ca-trust"])
                .output()
                .is_ok_and(|output| output.status.success());
            let has_pkexec = Command::new("sh")
                .args(["-lc", "command -v pkexec"])
                .output()
                .is_ok_and(|output| output.status.success());
            if !has_pkexec {
                return Err(AppError::EnvironmentPrivilegeRequired);
            }
            let source_text = source.display().to_string();
            let script = if has_update_ca {
                format!(
                    "cp '{}' /usr/local/share/ca-certificates/codex-relay-gateway-ca.crt && update-ca-certificates",
                    source_text.replace('\'', "'\\''")
                )
            } else if has_update_ca_trust {
                format!(
                    "cp '{}' /etc/pki/ca-trust/source/anchors/codex-relay-gateway-ca.crt && update-ca-trust extract",
                    source_text.replace('\'', "'\\''")
                )
            } else {
                return Err(AppError::CaTrustFailed);
            };
            let output = Command::new("pkexec")
                .args(["sh", "-lc", &script])
                .output()
                .map_err(|_| AppError::RuntimeUnavailable)?;
            if output.status.success() {
                return Ok(());
            }
            Err(AppError::CaTrustFailed)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Err(AppError::RuntimeUnavailable)
        }
    }

    async fn ensure_local_certificates(&self, address: IpAddr) -> AppResult<(String, String)> {
        std::fs::create_dir_all(&self.certificate_dir).map_err(|_| AppError::Internal)?;
        let ca_path = self.certificate_dir.join("gateway-ca.pem");
        let leaf_path = self.certificate_dir.join("gateway-leaf.pem");
        if self
            .repository
            .setting("gateway_certificate_address")?
            .as_deref()
            == Some(&address.to_string())
            && ca_path.exists()
            && leaf_path.exists()
        {
            if let (Ok(ca), Ok(leaf), Ok(key)) = (
                std::fs::read_to_string(&ca_path),
                std::fs::read_to_string(&leaf_path),
                self.secrets.get("gateway:leaf-private-key").await,
            ) {
                return Ok((format!("{leaf}\n{ca}"), key));
            }
        }
        let (ca, ca_key, ca_pem) = if ca_path.exists() {
            match (
                std::fs::read_to_string(&ca_path),
                self.secrets.get("gateway:ca-private-key").await,
            ) {
                (Ok(ca_pem), Ok(ca_key_pem)) => {
                    let params = CertificateParams::from_ca_cert_pem(&ca_pem)
                        .map_err(|_| AppError::Internal)?;
                    let key = KeyPair::from_pem(&ca_key_pem).map_err(|_| AppError::Internal)?;
                    let certificate = params.self_signed(&key).map_err(|_| AppError::Internal)?;
                    (certificate, key, ca_pem)
                }
                _ => new_certificate_authority()?,
            }
        } else {
            new_certificate_authority()?
        };
        let leaf_key = KeyPair::generate().map_err(|_| AppError::Internal)?;
        let leaf_params = CertificateParams::new(vec!["localhost".to_owned(), address.to_string()])
            .map_err(|_| AppError::Internal)?;
        let leaf = leaf_params
            .signed_by(&leaf_key, &ca, &ca_key)
            .map_err(|_| AppError::Internal)?;
        self.secrets
            .set("gateway:ca-private-key", &ca_key.serialize_pem())
            .await?;
        self.secrets
            .set("gateway:leaf-private-key", &leaf_key.serialize_pem())
            .await?;
        let leaf_pem = leaf.pem();
        std::fs::write(&ca_path, &ca_pem).map_err(|_| AppError::Internal)?;
        std::fs::write(&leaf_path, &leaf_pem).map_err(|_| AppError::Internal)?;
        self.repository
            .set_setting("gateway_certificate_address", &address.to_string())?;
        Ok((format!("{leaf_pem}\n{ca_pem}"), leaf_key.serialize_pem()))
    }
}

fn new_certificate_authority() -> AppResult<(rcgen::Certificate, KeyPair, String)> {
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().map_err(|_| AppError::Internal)?;
    let ca = ca_params
        .self_signed(&ca_key)
        .map_err(|_| AppError::Internal)?;
    let pem = ca.pem();
    Ok((ca, ca_key, pem))
}

pub fn validate_binding(mode: &str, address: IpAddr, cidrs: &[String]) -> AppResult<()> {
    match mode {
        "loopback" if address.is_loopback() && cidrs.is_empty() => Ok(()),
        "lan" if is_private_address(address) => {
            for cidr in cidrs {
                let _: IpNet = cidr.parse().map_err(|_| AppError::ValidationFailed)?;
            }
            Ok(())
        }
        _ => Err(AppError::ForbiddenNetworkTarget),
    }
}

pub fn available_lan_addresses() -> Vec<GatewayNetworkAddress> {
    let default = default_route_address();
    let mut addresses = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(address) if address.is_private() => Some(GatewayNetworkAddress {
                name: interface.name,
                address: address.to_string(),
                is_default: default == Some(IpAddr::V4(address)),
            }),
            IpAddr::V6(address) if address.is_unique_local() => Some(GatewayNetworkAddress {
                name: interface.name,
                address: address.to_string(),
                is_default: default == Some(IpAddr::V6(address)),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    addresses.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then(left.name.cmp(&right.name))
    });
    addresses.dedup_by(|left, right| left.address == right.address);
    addresses
}

fn default_route_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:443").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn default_lan_address() -> Option<GatewayNetworkAddress> {
    available_lan_addresses().into_iter().next()
}

fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => value.is_private(),
        IpAddr::V6(value) => value.is_unique_local(),
    }
}

fn gateway_router(api_state: GatewayApiState) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/responses", post(responses))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1beta/*path", post(gemini))
        .route("/api/chat", post(ollama_chat))
        .route("/api/generate", post(ollama_generate))
        .route("/api/tags", get(ollama_tags))
        .layer(DefaultBodyLimit::max(GATEWAY_JSON_BODY_LIMIT_BYTES))
        .route_layer(middleware::from_fn_with_state(
            api_state.clone(),
            authenticate_gateway_request,
        ));
    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .with_state(api_state)
}

async fn authenticate_gateway_request(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let auth_started = Instant::now();
    let Some(client) = authorize(&state, peer.ip(), request.headers()) else {
        let request_id = uuid::Uuid::new_v4().to_string();
        state.telemetry.record(GatewayRequestMetricSummary {
            sequence: 0,
            request_id: request_id.clone(),
            started_at_ms: timestamp_ms(),
            route: request.uri().path().trim_start_matches('/').to_owned(),
            provider: "unknown".to_owned(),
            profile_id: None,
            auth_mode: "rejected".to_owned(),
            stream: false,
            auth_latency_ms: auth_started.elapsed().as_millis() as i64,
            queue_latency_ms: 0,
            ttfb_ms: None,
            total_latency_ms: auth_started.elapsed().as_millis() as i64,
            request_bytes: request
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            response_bytes: 0,
            http_status: StatusCode::UNAUTHORIZED.as_u16(),
            outcome: "failed".to_owned(),
            error_category: Some("authentication".to_owned()),
            upstream_attempts: 0,
            retry_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            upstream_response_id: None,
        });
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        if let Ok(value) = request_id.parse() {
            response
                .headers_mut()
                .insert("x-codex-relay-request-id", value);
        }
        return response;
    };
    let metric_client = client.clone();
    let route = request.uri().path().trim_start_matches('/').to_owned();
    let request_bytes = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    request.extensions_mut().insert(client);
    let mut response = next.run(request).await;
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        let request_id = uuid::Uuid::new_v4().to_string();
        state.telemetry.record(GatewayRequestMetricSummary {
            sequence: 0,
            request_id: request_id.clone(),
            started_at_ms: timestamp_ms()
                .saturating_sub(metric_client.request_started.elapsed().as_millis() as i64),
            route,
            provider: "unknown".to_owned(),
            profile_id: None,
            auth_mode: metric_client.auth_mode.to_owned(),
            stream: false,
            auth_latency_ms: metric_client.auth_latency_ms,
            queue_latency_ms: 0,
            ttfb_ms: None,
            total_latency_ms: metric_client.request_started.elapsed().as_millis() as i64,
            request_bytes,
            response_bytes: 0,
            http_status: StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
            outcome: "failed".to_owned(),
            error_category: Some("body_limit".to_owned()),
            upstream_attempts: 0,
            retry_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            upstream_response_id: None,
        });
        if let Ok(value) = request_id.parse() {
            response
                .headers_mut()
                .insert("x-codex-relay-request-id", value);
        }
    }
    response
}

async fn healthz(State(state): State<GatewayApiState>) -> impl IntoResponse {
    state
        .auth_cache
        .refresh_oauth_if_due(&state.repository, &state.codex_home);
    let mut gateway =
        state
            .repository
            .gateway_settings(true, state.certificate_ready, available_lan_addresses());
    let candidates = candidates_for_model(&state.repository, None).unwrap_or_default();
    let mut summaries = HashMap::<(GatewayProvider, String), (HashSet<String>, usize)>::new();
    for candidate in candidates {
        let surface = match candidate.profile.kind {
            ProfileKind::CodexOauth => "openai".to_owned(),
            ProfileKind::ApiKey => provider_surface(&candidate.profile),
        };
        let entry = summaries
            .entry((candidate.profile.provider.clone(), surface))
            .or_insert_with(|| (HashSet::new(), 0));
        entry.1 += 1;
        for model in candidate.profile.models {
            entry.0.insert(model);
        }
    }
    let providers = summaries
        .into_iter()
        .map(
            |((provider, surface), (models, profiles))| GatewayHealthProviderSummary {
                provider,
                surface,
                profiles,
                models: models.len(),
            },
        )
        .collect::<Vec<_>>();
    let (active_requests, queued_requests) = state.concurrency.counts();
    let summary = match gateway.as_mut() {
        Ok(gateway) => {
            if let Some(route) = gateway.direct_route.as_mut() {
                if route.route_mode == "relay_bridge" && !state.auth_cache.oauth_ready() {
                    route.oauth_ready = false;
                    route.status = if route.credential_ready {
                        "degraded".to_owned()
                    } else {
                        "unavailable".to_owned()
                    };
                }
            }
            gateway.active_requests = active_requests;
            gateway.queued_requests = queued_requests;
            let status = if gateway.pool_status == "ok"
                || gateway
                    .direct_route
                    .as_ref()
                    .is_some_and(|route| route.status == "ok")
            {
                "ok"
            } else if gateway
                .direct_route
                .as_ref()
                .is_some_and(|route| route.status == "degraded")
            {
                "degraded"
            } else {
                "unavailable"
            };
            GatewayHealthSummary {
                status: status.to_owned(),
                running: gateway.running,
                bind_mode: gateway.bind_mode.clone(),
                service_url: gateway.service_url.clone(),
                available_profiles: gateway.available_profiles,
                cooling_profiles: gateway.cooling_profiles,
                pool_status: gateway.pool_status.clone(),
                direct_route: gateway.direct_route.clone(),
                active_requests,
                queued_requests,
                certificate_ready: gateway.certificate_ready,
                client_key_count: gateway.client_key_count,
                upstream_last_error: gateway.upstream_last_error.clone(),
                providers,
            }
        }
        Err(_) => GatewayHealthSummary {
            status: "unavailable".to_owned(),
            running: true,
            bind_mode: "unknown".to_owned(),
            service_url: String::new(),
            available_profiles: 0,
            cooling_profiles: 0,
            pool_status: "unavailable".to_owned(),
            direct_route: None,
            active_requests,
            queued_requests,
            certificate_ready: state.certificate_ready,
            client_key_count: 0,
            upstream_last_error: None,
            providers,
        },
    };
    (StatusCode::OK, Json(summary))
}

fn provider_surface(profile: &MaskedProfile) -> String {
    match profile.provider {
        GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible => match profile.wire_api {
            GatewayWireApi::Responses => "responses",
            GatewayWireApi::ChatCompletions => "chat_completions",
        },
        GatewayProvider::Anthropic => "messages",
        GatewayProvider::Gemini => "generate_content",
        GatewayProvider::Ollama => "chat",
    }
    .to_owned()
}

async fn list_models(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
) -> Response {
    let candidates = match direct_profile_for_codex_client(&state, &client, None) {
        Ok(Some(profile)) => vec![profile],
        Ok(None) => match candidates_for_model(&state.repository, None) {
            Ok(value) => value,
            Err(_) => {
                return instrument_immediate_gateway_response(
                    StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    &state,
                    &client,
                    "models",
                    &GatewayProvider::OpenAiCompatible,
                    0,
                    None,
                    "profile_lookup_failed",
                )
            }
        },
        Err(_) => {
            return instrument_immediate_gateway_response(
                StatusCode::SERVICE_UNAVAILABLE.into_response(),
                &state,
                &client,
                "models",
                &GatewayProvider::OpenAiCompatible,
                0,
                None,
                "direct_profile_unavailable",
            )
        }
    };
    let models = candidates
        .into_iter()
        .flat_map(|candidate| candidate.profile.models)
        .collect::<HashSet<_>>();
    let data = models
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "codex-relay"}))
        .collect::<Vec<_>>();
    instrument_local_gateway_response(
        Json(json!({"object": "list", "data": data})).into_response(),
        &state,
        &client,
        "models",
        &GatewayProvider::OpenAiCompatible,
        0,
        None,
        None,
    )
}

async fn responses(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
    payload: Bytes,
) -> Response {
    forward_bytes(
        state,
        client,
        payload,
        "responses",
        GatewayProvider::OpenAiCompatible,
        Some(GatewayRequestKind::Responses),
    )
    .await
}

async fn chat_completions(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
    payload: Bytes,
) -> Response {
    forward_bytes(
        state,
        client,
        payload,
        "chat/completions",
        GatewayProvider::OpenAiCompatible,
        Some(GatewayRequestKind::ChatCompletions),
    )
    .await
}

async fn messages(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
    payload: Bytes,
) -> Response {
    forward_bytes(
        state,
        client,
        payload,
        "v1/messages",
        GatewayProvider::Anthropic,
        None,
    )
    .await
}

async fn gemini(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
    axum::extract::Path(path): axum::extract::Path<String>,
    payload: Bytes,
) -> Response {
    forward_bytes(
        state,
        client,
        payload,
        &format!("v1beta/{path}"),
        GatewayProvider::Gemini,
        None,
    )
    .await
}

async fn ollama_chat(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
    payload: Bytes,
) -> Response {
    forward_bytes(
        state,
        client,
        payload,
        "api/chat",
        GatewayProvider::Ollama,
        None,
    )
    .await
}

async fn ollama_generate(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
    payload: Bytes,
) -> Response {
    forward_bytes(
        state,
        client,
        payload,
        "api/generate",
        GatewayProvider::Ollama,
        None,
    )
    .await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GatewayRequestKind {
    Responses,
    ChatCompletions,
}

#[derive(Debug, Deserialize)]
struct GatewayRequestEnvelope {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    previous_response_id: Option<String>,
}

#[derive(Default)]
struct StreamTerminalState {
    buffer: String,
    terminated: bool,
    failure_emitted: bool,
}

impl StreamTerminalState {
    fn observe_sse(&mut self, bytes: &Bytes) {
        if self.terminated {
            return;
        }
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        if self.buffer.contains("data: [DONE]") {
            self.terminated = true;
        }
        for event in drain_sse_events(&mut self.buffer) {
            if matches!(
                event.get("type").and_then(Value::as_str),
                Some("response.completed" | "response.failed" | "response.incomplete" | "error")
            ) {
                self.terminated = true;
            }
        }
    }

    fn interruption(&mut self, kind: Option<GatewayRequestKind>) -> Option<Bytes> {
        if self.terminated || self.failure_emitted {
            return None;
        }
        self.failure_emitted = true;
        self.terminated = true;
        Some(Bytes::from(sse_upstream_error_event(kind)))
    }
}

#[derive(Clone)]
enum ApiResponseAdapter {
    Direct {
        visible_model: String,
    },
    ChatToResponses {
        client_stream: bool,
        visible_model: String,
    },
    ResponsesToChat {
        client_stream: bool,
        visible_model: String,
        profile_id: String,
    },
    ProviderToResponses {
        provider: GatewayProvider,
        client_stream: bool,
        visible_model: String,
    },
    ProviderToChat {
        provider: GatewayProvider,
        client_stream: bool,
        visible_model: String,
    },
}

fn upstream_attempt(
    profile: &MaskedProfile,
    payload: &Value,
    route: &str,
    request_kind: Option<GatewayRequestKind>,
    client_stream: bool,
    visible_model: String,
    upstream_model: String,
) -> Result<(String, Value, ApiResponseAdapter), String> {
    if request_kind.is_none()
        || matches!(
            profile.provider,
            GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible
        )
    {
        let mut upstream_route = route.to_owned();
        let mut upstream_payload = payload_with_model(payload, &upstream_model);
        let mut response_adapter = ApiResponseAdapter::Direct {
            visible_model: visible_model.clone(),
        };
        if profile.wire_api == GatewayWireApi::ChatCompletions
            && request_kind == Some(GatewayRequestKind::Responses)
        {
            upstream_route = "chat/completions".to_owned();
            upstream_payload = responses_to_chat_completion(payload, &upstream_model)?;
            response_adapter = ApiResponseAdapter::ChatToResponses {
                client_stream,
                visible_model,
            };
        } else if profile.wire_api == GatewayWireApi::Responses
            && request_kind == Some(GatewayRequestKind::ChatCompletions)
        {
            upstream_route = "responses".to_owned();
            upstream_payload = payload_with_model(&chat_to_responses(payload)?, &upstream_model);
            response_adapter = ApiResponseAdapter::ResponsesToChat {
                client_stream,
                visible_model,
                profile_id: profile.id.clone(),
            };
        }
        return Ok((upstream_route, upstream_payload, response_adapter));
    }

    let kind = request_kind.ok_or_else(|| "request kind is required".to_owned())?;
    let chat_payload = match kind {
        GatewayRequestKind::ChatCompletions => payload_with_model(payload, &upstream_model),
        GatewayRequestKind::Responses => responses_to_chat_completion(payload, &upstream_model)?,
    };
    validate_provider_chat_request(&profile.provider, &chat_payload)?;
    let upstream_payload = provider_chat_payload(
        &profile.provider,
        &chat_payload,
        &upstream_model,
        client_stream,
    )?;
    let upstream_route = provider_chat_route(&profile.provider, &upstream_model, client_stream);
    let adapter = match kind {
        GatewayRequestKind::Responses => ApiResponseAdapter::ProviderToResponses {
            provider: profile.provider.clone(),
            client_stream,
            visible_model,
        },
        GatewayRequestKind::ChatCompletions => ApiResponseAdapter::ProviderToChat {
            provider: profile.provider.clone(),
            client_stream,
            visible_model,
        },
    };
    Ok((upstream_route, upstream_payload, adapter))
}

enum ProviderFanoutAttempt {
    Response(Response, bool, i64, Vec<ProfilePermit>),
    RetryAfter(Duration),
    Unhealthy,
    TemporaryFailure,
    Backpressure(&'static str),
}

fn chat_choice_count(payload: &Value) -> Result<usize, String> {
    match payload.get("n") {
        None | Some(Value::Null) => Ok(1),
        Some(value) => {
            let Some(count) = value.as_u64() else {
                return Err("n must be a positive integer".to_owned());
            };
            if !(1..=128).contains(&count) {
                return Err("n must be between 1 and 128".to_owned());
            }
            Ok(count as usize)
        }
    }
}

fn provider_chat_route(provider: &GatewayProvider, model: &str, stream: bool) -> String {
    match provider {
        GatewayProvider::Anthropic => "v1/messages".to_owned(),
        GatewayProvider::Gemini if stream => {
            format!("v1beta/models/{model}:streamGenerateContent?alt=sse")
        }
        GatewayProvider::Gemini => format!("v1beta/models/{model}:generateContent"),
        GatewayProvider::Ollama => "api/chat".to_owned(),
        GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible => {
            "chat/completions".to_owned()
        }
    }
}

fn validate_provider_chat_request(
    provider: &GatewayProvider,
    payload: &Value,
) -> Result<(), String> {
    let unsupported = match provider {
        GatewayProvider::Anthropic | GatewayProvider::Gemini | GatewayProvider::Ollama => {
            ["audio", "logprobs", "top_logprobs"]
                .into_iter()
                .find(|field| payload.get(field).is_some_and(|value| !value.is_null()))
        }
        _ => None,
    };
    if let Some(field) = unsupported {
        return Err(format!("{field} is not supported by this provider adapter"));
    }
    Ok(())
}

fn provider_chat_payload(
    provider: &GatewayProvider,
    payload: &Value,
    model: &str,
    stream: bool,
) -> Result<Value, String> {
    match provider {
        GatewayProvider::Anthropic => anthropic_chat_payload(payload, model, stream),
        GatewayProvider::Gemini => gemini_chat_payload(payload, stream),
        GatewayProvider::Ollama => ollama_chat_payload_from_openai(payload, model, stream),
        _ => Ok(payload_with_model(payload, model)),
    }
}

fn visible_model_for(profile: &MaskedProfile, requested: Option<&str>) -> String {
    requested
        .map(str::to_owned)
        .or_else(|| {
            profile
                .model_mappings
                .first()
                .map(|mapping| mapping.model.clone())
        })
        .or_else(|| profile.models.first().cloned())
        .unwrap_or_else(|| "model".to_owned())
}

fn upstream_model_for(profile: &MaskedProfile, requested: Option<&str>) -> Option<String> {
    let requested = requested?;
    profile
        .model_mappings
        .iter()
        .find(|mapping| mapping.model == requested)
        .map(|mapping| mapping.upstream_model.clone())
        .or_else(|| {
            profile
                .models
                .iter()
                .any(|model| model == requested)
                .then(|| requested.to_owned())
        })
}

fn can_forward_raw(
    profile: &MaskedProfile,
    requested_provider: &GatewayProvider,
    request_kind: Option<GatewayRequestKind>,
    requested_model: Option<&str>,
    upstream_model: &str,
) -> bool {
    if requested_model != Some(upstream_model) {
        return false;
    }
    if request_kind.is_none() {
        return profile.provider == *requested_provider;
    }
    if !matches!(
        profile.provider,
        GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible
    ) {
        return false;
    }
    matches!(
        (request_kind, &profile.wire_api),
        (
            Some(GatewayRequestKind::Responses),
            GatewayWireApi::Responses
        ) | (
            Some(GatewayRequestKind::ChatCompletions),
            GatewayWireApi::ChatCompletions
        )
    )
}

fn payload_with_model(payload: &Value, model: &str) -> Value {
    let mut next = payload.clone();
    if let Some(object) = next.as_object_mut() {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }
    next
}

async fn ollama_tags(
    State(state): State<GatewayApiState>,
    Extension(client): Extension<AuthorizedClient>,
) -> Response {
    let candidates = match direct_profile_for_codex_client(&state, &client, None) {
        Ok(Some(profile)) => vec![profile],
        Ok(None) => candidates_for_model(&state.repository, None).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let models = candidates
        .into_iter()
        .filter(|candidate| candidate.profile.provider == GatewayProvider::Ollama)
        .flat_map(|candidate| candidate.profile.models)
        .collect::<HashSet<_>>();
    Json(json!({
        "models": models
            .into_iter()
            .map(|name| json!({"name": name}))
            .collect::<Vec<_>>()
    }))
    .into_response()
}

struct GatewayMetricSeed {
    request_id: String,
    started_at_ms: i64,
    started: Instant,
    route: String,
    provider: String,
    profile_id: Option<String>,
    auth_mode: String,
    auth_latency_ms: i64,
    queue_latency_ms: i64,
    request_bytes: i64,
    upstream_attempts: i64,
    retry_count: i64,
    error_category: Option<String>,
    _permits: Vec<ProfilePermit>,
}

struct GatewayMetricStreamState {
    seed: Option<GatewayMetricSeed>,
    telemetry: Arc<GatewayTelemetry>,
    status: u16,
    sse: bool,
    response_bytes: i64,
    ttfb_ms: Option<i64>,
    clean_eof: bool,
    terminal_success: bool,
    terminal_failure: bool,
    error_category: Option<String>,
    sse_buffer: String,
    json_buffer: Vec<u8>,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    upstream_response_id: Option<String>,
}

impl GatewayMetricStreamState {
    fn observe(&mut self, bytes: &Bytes) {
        if self.ttfb_ms.is_none() {
            self.ttfb_ms = self
                .seed
                .as_ref()
                .map(|seed| seed.started.elapsed().as_millis() as i64);
        }
        self.response_bytes = self.response_bytes.saturating_add(bytes.len() as i64);
        if self.sse {
            self.sse_buffer.push_str(&String::from_utf8_lossy(bytes));
            if self.sse_buffer.contains("data: [DONE]") {
                self.terminal_success = true;
            }
            for event in drain_sse_events(&mut self.sse_buffer) {
                match event.get("type").and_then(Value::as_str) {
                    Some("response.completed") => self.terminal_success = true,
                    Some("response.failed") | Some("error") => self.terminal_failure = true,
                    _ => {}
                }
                let (input, output, total) = usage_breakdown_from_event(&event);
                self.input_tokens = self.input_tokens.max(input);
                self.output_tokens = self.output_tokens.max(output);
                self.total_tokens = self.total_tokens.max(total);
                if self.upstream_response_id.is_none() {
                    self.upstream_response_id = response_id_from_event(&event);
                }
            }
        } else if self.json_buffer.len() < GATEWAY_JSON_BODY_LIMIT_BYTES {
            self.json_buffer.extend_from_slice(bytes);
        }
    }
}

impl Drop for GatewayMetricStreamState {
    fn drop(&mut self) {
        let Some(seed) = self.seed.take() else {
            return;
        };
        if !self.sse && !self.json_buffer.is_empty() {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.json_buffer) {
                let (input, output, total) = usage_breakdown(&value);
                self.input_tokens = self.input_tokens.max(input);
                self.output_tokens = self.output_tokens.max(output);
                self.total_tokens = self.total_tokens.max(total);
                if self.upstream_response_id.is_none() {
                    self.upstream_response_id =
                        value.get("id").and_then(Value::as_str).map(str::to_owned);
                }
            }
        }
        let status_success = (200..300).contains(&self.status);
        let initial_error_category = seed.error_category.clone();
        let (outcome, error_category) = if self.terminal_failure {
            ("failed", Some("upstream_response_failed".to_owned()))
        } else if self.error_category.is_some() {
            ("failed", self.error_category.clone())
        } else if initial_error_category.is_some() {
            ("failed", initial_error_category)
        } else if !self.clean_eof {
            ("failed", Some("client_disconnected".to_owned()))
        } else if self.sse && !self.terminal_success {
            ("failed", Some("stream_incomplete".to_owned()))
        } else if status_success {
            ("success", None)
        } else {
            ("failed", Some(format!("http_{}", self.status)))
        };
        self.telemetry.record(GatewayRequestMetricSummary {
            sequence: 0,
            request_id: seed.request_id,
            started_at_ms: seed.started_at_ms,
            route: seed.route,
            provider: seed.provider,
            profile_id: seed.profile_id,
            auth_mode: seed.auth_mode,
            stream: self.sse,
            auth_latency_ms: seed.auth_latency_ms,
            queue_latency_ms: seed.queue_latency_ms,
            ttfb_ms: self.ttfb_ms,
            total_latency_ms: seed.started.elapsed().as_millis() as i64,
            request_bytes: seed.request_bytes,
            response_bytes: self.response_bytes,
            http_status: self.status,
            outcome: outcome.to_owned(),
            error_category,
            upstream_attempts: seed.upstream_attempts,
            retry_count: seed.retry_count,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            upstream_response_id: self.upstream_response_id.clone(),
        });
    }
}

fn instrument_gateway_response(
    response: Response,
    seed: GatewayMetricSeed,
    telemetry: Arc<GatewayTelemetry>,
    _request_kind: Option<GatewayRequestKind>,
) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        "x-codex-relay-request-id",
        seed.request_id
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("invalid")),
    );
    let status = parts.status.as_u16();
    let sse = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let state = GatewayMetricStreamState {
        seed: Some(seed),
        telemetry,
        status,
        sse,
        response_bytes: 0,
        ttfb_ms: None,
        clean_eof: false,
        terminal_success: false,
        terminal_failure: false,
        error_category: None,
        sse_buffer: String::new(),
        json_buffer: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        upstream_response_id: None,
    };
    let stream = body
        .into_data_stream()
        .map(Some)
        .chain(futures_util::stream::once(async { None }))
        .scan(state, |state, item| {
            let result = match item {
                Some(Ok(bytes)) => {
                    state.observe(&bytes);
                    Some(Ok::<Bytes, axum::Error>(bytes))
                }
                Some(Err(error)) => {
                    state.error_category = Some("upstream_stream_interrupted".to_owned());
                    Some(Err(error))
                }
                None => {
                    state.clean_eof = true;
                    None
                }
            };
            futures_util::future::ready(result)
        });
    Response::from_parts(parts, Body::from_stream(stream))
}

fn gateway_provider_label(provider: &GatewayProvider) -> &'static str {
    match provider {
        GatewayProvider::OpenAi => "openai",
        GatewayProvider::OpenAiCompatible => "openai_compatible",
        GatewayProvider::Anthropic => "anthropic",
        GatewayProvider::Gemini => "gemini",
        GatewayProvider::Ollama => "ollama",
    }
}

fn backpressure_response(code: &'static str) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": {"code": code}})),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    response
}

// Keeping metric dimensions explicit at call sites avoids silently omitting a
// route/auth/body field when recording an early response.
#[allow(clippy::too_many_arguments)]
fn instrument_immediate_gateway_response(
    response: Response,
    state: &GatewayApiState,
    authorized_client: &AuthorizedClient,
    route: &str,
    requested_provider: &GatewayProvider,
    request_bytes: i64,
    request_kind: Option<GatewayRequestKind>,
    error_category: &str,
) -> Response {
    instrument_local_gateway_response(
        response,
        state,
        authorized_client,
        route,
        requested_provider,
        request_bytes,
        request_kind,
        Some(error_category),
    )
}

#[allow(clippy::too_many_arguments)]
fn instrument_local_gateway_response(
    response: Response,
    state: &GatewayApiState,
    authorized_client: &AuthorizedClient,
    route: &str,
    requested_provider: &GatewayProvider,
    request_bytes: i64,
    request_kind: Option<GatewayRequestKind>,
    error_category: Option<&str>,
) -> Response {
    let started = authorized_client.request_started;
    let seed = GatewayMetricSeed {
        request_id: uuid::Uuid::new_v4().to_string(),
        started_at_ms: timestamp_ms().saturating_sub(started.elapsed().as_millis() as i64),
        started,
        route: route.to_owned(),
        provider: gateway_provider_label(requested_provider).to_owned(),
        profile_id: None,
        auth_mode: authorized_client.auth_mode.to_owned(),
        auth_latency_ms: authorized_client.auth_latency_ms,
        queue_latency_ms: 0,
        request_bytes,
        upstream_attempts: 0,
        retry_count: 0,
        error_category: error_category.map(str::to_owned),
        _permits: Vec::new(),
    };
    instrument_gateway_response(response, seed, state.telemetry.clone(), request_kind)
}

#[cfg(test)]
async fn forward(
    state: GatewayApiState,
    authorized_client: AuthorizedClient,
    payload: Value,
    request_bytes: i64,
    route: &str,
    requested_provider: GatewayProvider,
    request_kind: Option<GatewayRequestKind>,
) -> Response {
    let raw_payload = match serde_json::to_vec(&payload) {
        Ok(payload) => Bytes::from(payload),
        Err(_) => return openai_bad_request("request body must be valid JSON"),
    };
    debug_assert_eq!(raw_payload.len() as i64, request_bytes);
    forward_bytes(
        state,
        authorized_client,
        raw_payload,
        route,
        requested_provider,
        request_kind,
    )
    .await
}

async fn forward_bytes(
    state: GatewayApiState,
    authorized_client: AuthorizedClient,
    raw_payload: Bytes,
    route: &str,
    requested_provider: GatewayProvider,
    request_kind: Option<GatewayRequestKind>,
) -> Response {
    let request_bytes = raw_payload.len() as i64;
    let envelope = match serde_json::from_slice::<GatewayRequestEnvelope>(&raw_payload) {
        Ok(envelope) => envelope,
        Err(_) => {
            return instrument_immediate_gateway_response(
                openai_bad_request("request body must be valid JSON"),
                &state,
                &authorized_client,
                route,
                &requested_provider,
                request_bytes,
                request_kind,
                "invalid_json",
            )
        }
    };
    let immediate = |response: Response, error_category: &str| {
        instrument_immediate_gateway_response(
            response,
            &state,
            &authorized_client,
            route,
            &requested_provider,
            request_bytes,
            request_kind,
            error_category,
        )
    };
    let mut parsed_payload = None;
    if request_kind == Some(GatewayRequestKind::ChatCompletions) {
        let payload = match serde_json::from_slice::<Value>(&raw_payload) {
            Ok(payload) => payload,
            Err(_) => {
                return immediate(
                    openai_bad_request("request body must be valid JSON"),
                    "invalid_json",
                )
            }
        };
        if let Err(message) = validate_chat_request(&payload) {
            return immediate(openai_bad_request(&message), "invalid_request");
        }
        parsed_payload = Some(payload);
    }
    let model = envelope.model.as_deref();
    let mut candidates = match direct_profile_for_codex_client(&state, &authorized_client, model) {
        Ok(Some(profile)) => vec![profile],
        Ok(None) => match candidates_for_model(&state.repository, model) {
            Ok(value) => value
                .into_iter()
                .filter(|candidate| {
                    if candidate.profile.kind == ProfileKind::CodexOauth {
                        request_kind.is_some()
                            && requested_provider == GatewayProvider::OpenAiCompatible
                    } else if request_kind.is_some()
                        && requested_provider == GatewayProvider::OpenAiCompatible
                    {
                        true
                    } else {
                        candidate.profile.provider == requested_provider
                            || matches!(
                                (&requested_provider, &candidate.profile.provider),
                                (GatewayProvider::OpenAiCompatible, GatewayProvider::OpenAi)
                            )
                    }
                })
                .collect::<Vec<_>>(),
            _ => {
                return immediate(
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": {"code": "model_not_available"}})),
                    )
                        .into_response(),
                    "model_not_available",
                )
            }
        },
        Err(AppError::GatewayModelUnavailable) => {
            return immediate(
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": {"code": "model_not_available"}})),
                )
                    .into_response(),
                "model_not_available",
            )
        }
        Err(_) => {
            return immediate(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": {"code": "direct_profile_unavailable"}})),
                )
                    .into_response(),
                "direct_profile_unavailable",
            )
        }
    };
    if candidates.is_empty() {
        return immediate(
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": {"code": "model_not_available"}})),
            )
                .into_response(),
            "model_not_available",
        );
    }
    cool_down_exhausted_profiles(&state.repository, &mut candidates);
    if candidates.is_empty() {
        return immediate(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": {"code": "all_profiles_exhausted"}})),
            )
                .into_response(),
            "all_profiles_exhausted",
        );
    }
    if request_kind == Some(GatewayRequestKind::Responses) {
        if let Some(previous_id) = envelope.previous_response_id.as_deref() {
            match affinity_profile(&state, previous_id) {
                Some(profile_id) => {
                    candidates.retain(|candidate| candidate.profile.id == profile_id);
                    if candidates.is_empty() {
                        return immediate(affinity_conflict(), "response_affinity_conflict");
                    }
                }
                None => {
                    candidates.retain(|candidate| candidate.profile.kind == ProfileKind::ApiKey);
                    if candidates.is_empty() {
                        return immediate(affinity_conflict(), "response_affinity_conflict");
                    }
                }
            }
        }
    }
    let route_key = format!(
        "{}:{}:{}",
        match request_kind {
            Some(GatewayRequestKind::Responses) => "responses",
            Some(GatewayRequestKind::ChatCompletions) => "chat",
            None => route,
        },
        model.unwrap_or("*"),
        candidates
            .iter()
            .map(|candidate| candidate.profile.priority)
            .min()
            .unwrap_or_default()
    );
    let candidates = ordered_candidates(candidates, &state.scheduler, &route_key);
    let http_client = state.http_client.clone();
    let started = authorized_client.request_started;
    let started_at_ms = timestamp_ms().saturating_sub(started.elapsed().as_millis() as i64);
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut upstream_attempts = 0_i64;
    let mut last_backpressure = None;
    for candidate in candidates.into_iter() {
        let (permit, queue_latency_ms) = match state.concurrency.acquire(&candidate.profile).await {
            Ok(value) => value,
            Err(ProfileAcquireError::QueueFull) => {
                last_backpressure = Some("relay_queue_full");
                continue;
            }
            Err(ProfileAcquireError::QueueTimeout) => {
                last_backpressure = Some("relay_queue_timeout");
                continue;
            }
        };
        upstream_attempts += 1;
        if candidate.profile.kind == ProfileKind::CodexOauth {
            let Some(kind) = request_kind else {
                continue;
            };
            if parsed_payload.is_none() {
                parsed_payload = serde_json::from_slice::<Value>(&raw_payload).ok();
            }
            let Some(payload) = parsed_payload.as_ref() else {
                return immediate(
                    openai_bad_request("request body must be valid JSON"),
                    "invalid_json",
                );
            };
            match forward_oauth_candidate(&state, &http_client, &candidate, payload, kind).await {
                OAuthAttempt::Response(response, physical_attempts) => {
                    upstream_attempts =
                        upstream_attempts.saturating_add(physical_attempts.saturating_sub(1));
                    if response.status().is_success() {
                        clear_gateway_upstream_error(&state.repository);
                    }
                    let seed = GatewayMetricSeed {
                        request_id: request_id.clone(),
                        started_at_ms,
                        started,
                        route: route.to_owned(),
                        provider: gateway_provider_label(&candidate.profile.provider).to_owned(),
                        profile_id: Some(candidate.profile.id.clone()),
                        auth_mode: authorized_client.auth_mode.to_owned(),
                        auth_latency_ms: authorized_client.auth_latency_ms,
                        queue_latency_ms,
                        request_bytes,
                        upstream_attempts,
                        retry_count: upstream_attempts.saturating_sub(1),
                        error_category: None,
                        _permits: vec![permit],
                    };
                    return instrument_gateway_response(
                        response,
                        seed,
                        state.telemetry.clone(),
                        request_kind,
                    );
                }
                OAuthAttempt::RetryAfter(duration) => {
                    cool_down_profile(&state.repository, &candidate.profile.id, duration);
                    continue;
                }
                OAuthAttempt::ReauthorizationRequired => {
                    mark_profile_health(
                        &state.repository,
                        &candidate.profile.id,
                        "reauthorization_required",
                    );
                    continue;
                }
                OAuthAttempt::TemporaryFailure => {
                    cool_down_profile(
                        &state.repository,
                        &candidate.profile.id,
                        Duration::from_secs(15),
                    );
                    continue;
                }
            }
        }
        let Some(base_url) = candidate.profile.base_url.clone() else {
            continue;
        };
        let Some(secret_ref) = candidate.secret_ref.clone() else {
            continue;
        };
        let visible_model = visible_model_for(&candidate.profile, model);
        let upstream_model =
            upstream_model_for(&candidate.profile, model).unwrap_or_else(|| visible_model.clone());
        let client_stream = envelope.stream;
        let raw_passthrough = can_forward_raw(
            &candidate.profile,
            &requested_provider,
            request_kind,
            model,
            &upstream_model,
        );
        let (upstream_route, upstream_payload, response_adapter) = if raw_passthrough {
            (
                route.to_owned(),
                None,
                ApiResponseAdapter::Direct {
                    visible_model: visible_model.clone(),
                },
            )
        } else {
            if parsed_payload.is_none() {
                parsed_payload = serde_json::from_slice::<Value>(&raw_payload).ok();
            }
            let Some(payload) = parsed_payload.as_ref() else {
                return immediate(
                    openai_bad_request("request body must be valid JSON"),
                    "invalid_json",
                );
            };
            match upstream_attempt(
                &candidate.profile,
                payload,
                route,
                request_kind,
                client_stream,
                visible_model.clone(),
                upstream_model.clone(),
            ) {
                Ok((route, payload, adapter)) => (route, Some(payload), adapter),
                Err(message) => return immediate(openai_bad_request(&message), "invalid_request"),
            }
        };
        let endpoint = match build_upstream_url(&base_url, &upstream_route) {
            Ok(url) => url,
            Err(_) => continue,
        };
        let key = match state.secrets.get(&secret_ref).await {
            Ok(key) => key,
            Err(_) => continue,
        };
        if let ApiResponseAdapter::ProviderToChat {
            provider,
            client_stream,
            visible_model,
        } = &response_adapter
        {
            let Some(payload) = parsed_payload.as_ref() else {
                return immediate(
                    openai_bad_request("request body must be valid JSON"),
                    "invalid_json",
                );
            };
            let choice_count = match chat_choice_count(payload) {
                Ok(value) => value,
                Err(message) => return immediate(openai_bad_request(&message), "invalid_request"),
            };
            if choice_count > 1 {
                drop(permit);
                match forward_provider_chat_choices(ProviderFanoutRequest {
                    client: &http_client,
                    endpoint: endpoint.clone(),
                    provider: provider.clone(),
                    key: key.clone(),
                    upstream_payload: upstream_payload.clone().unwrap_or_default(),
                    choice_count,
                    client_stream: *client_stream,
                    model: visible_model.clone(),
                    repository: state.repository.clone(),
                    concurrency: state.concurrency.clone(),
                    profile: candidate.profile.clone(),
                })
                .await
                {
                    ProviderFanoutAttempt::Response(
                        response,
                        successful,
                        fanout_queue_ms,
                        permits,
                    ) => {
                        if successful {
                            clear_gateway_upstream_error(&state.repository);
                        }
                        let physical_attempts =
                            upstream_attempts.saturating_add(choice_count.saturating_sub(1) as i64);
                        let seed = GatewayMetricSeed {
                            request_id: request_id.clone(),
                            started_at_ms,
                            started,
                            route: route.to_owned(),
                            provider: gateway_provider_label(&candidate.profile.provider)
                                .to_owned(),
                            profile_id: Some(candidate.profile.id.clone()),
                            auth_mode: authorized_client.auth_mode.to_owned(),
                            auth_latency_ms: authorized_client.auth_latency_ms,
                            queue_latency_ms: queue_latency_ms.saturating_add(fanout_queue_ms),
                            request_bytes,
                            upstream_attempts: physical_attempts,
                            retry_count: physical_attempts.saturating_sub(1),
                            error_category: None,
                            _permits: permits,
                        };
                        return instrument_gateway_response(
                            response,
                            seed,
                            state.telemetry.clone(),
                            request_kind,
                        );
                    }
                    ProviderFanoutAttempt::RetryAfter(duration) => {
                        cool_down_profile(&state.repository, &candidate.profile.id, duration);
                        continue;
                    }
                    ProviderFanoutAttempt::Unhealthy => {
                        mark_profile_health(&state.repository, &candidate.profile.id, "unhealthy");
                        continue;
                    }
                    ProviderFanoutAttempt::TemporaryFailure => {
                        record_gateway_upstream_error(
                            &state.repository,
                            GATEWAY_ERROR_FIRST_RESPONSE,
                        );
                        cool_down_profile(
                            &state.repository,
                            &candidate.profile.id,
                            Duration::from_secs(15),
                        );
                        continue;
                    }
                    ProviderFanoutAttempt::Backpressure(code) => {
                        last_backpressure = Some(code);
                        continue;
                    }
                }
            }
        }
        let request = if let Some(upstream_payload) = upstream_payload.as_ref() {
            upstream_request(
                http_client.post(endpoint),
                &candidate.profile.provider,
                &key,
                upstream_payload,
            )
        } else {
            upstream_request_bytes(
                http_client.post(endpoint),
                &candidate.profile.provider,
                &key,
                raw_payload.clone(),
            )
        };
        match send_with_first_response_timeout(request).await {
            Ok(response)
                if response.status().is_server_error()
                    || response.status() == StatusCode::TOO_MANY_REQUESTS =>
            {
                cool_down_profile(
                    &state.repository,
                    &candidate.profile.id,
                    retry_after(&response).unwrap_or_else(|| Duration::from_secs(30)),
                );
                continue;
            }
            Ok(response)
                if matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                mark_profile_health(&state.repository, &candidate.profile.id, "unhealthy");
                continue;
            }
            Ok(response) => {
                let successful = response.status().is_success();
                if successful {
                    clear_gateway_upstream_error(&state.repository);
                }
                let response = match response_adapter {
                    ApiResponseAdapter::Direct { visible_model } => {
                        upstream_response_with_visible_model(
                            response,
                            state.repository.clone(),
                            request_kind,
                            visible_model,
                        )
                        .await
                    }
                    ApiResponseAdapter::ChatToResponses {
                        client_stream,
                        visible_model,
                    } => {
                        adapt_chat_completion_response(
                            response,
                            client_stream,
                            visible_model,
                            state.repository.clone(),
                        )
                        .await
                    }
                    ApiResponseAdapter::ResponsesToChat {
                        client_stream,
                        visible_model,
                        profile_id,
                    } => {
                        adapt_oauth_response(
                            response,
                            GatewayRequestKind::ChatCompletions,
                            client_stream,
                            visible_model,
                            profile_id,
                            state.repository.clone(),
                            state.affinities.clone(),
                        )
                        .await
                    }
                    ApiResponseAdapter::ProviderToResponses {
                        provider,
                        client_stream,
                        visible_model,
                    } => {
                        adapt_provider_response(
                            response,
                            provider,
                            GatewayRequestKind::Responses,
                            client_stream,
                            visible_model,
                            state.repository.clone(),
                        )
                        .await
                    }
                    ApiResponseAdapter::ProviderToChat {
                        provider,
                        client_stream,
                        visible_model,
                    } => {
                        adapt_provider_response(
                            response,
                            provider,
                            GatewayRequestKind::ChatCompletions,
                            client_stream,
                            visible_model,
                            state.repository.clone(),
                        )
                        .await
                    }
                };
                let seed = GatewayMetricSeed {
                    request_id: request_id.clone(),
                    started_at_ms,
                    started,
                    route: route.to_owned(),
                    provider: gateway_provider_label(&candidate.profile.provider).to_owned(),
                    profile_id: Some(candidate.profile.id.clone()),
                    auth_mode: authorized_client.auth_mode.to_owned(),
                    auth_latency_ms: authorized_client.auth_latency_ms,
                    queue_latency_ms,
                    request_bytes,
                    upstream_attempts,
                    retry_count: upstream_attempts.saturating_sub(1),
                    error_category: None,
                    _permits: vec![permit],
                };
                return instrument_gateway_response(
                    response,
                    seed,
                    state.telemetry.clone(),
                    request_kind,
                );
            }
            Err(_) => {
                record_gateway_upstream_error(&state.repository, GATEWAY_ERROR_FIRST_RESPONSE);
                cool_down_profile(
                    &state.repository,
                    &candidate.profile.id,
                    Duration::from_secs(15),
                );
                continue;
            }
        }
    }
    if let Some(code) = last_backpressure {
        return immediate(backpressure_response(code), code);
    }
    immediate(
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "upstream_unavailable"}})),
        )
            .into_response(),
        "upstream_unavailable",
    )
}

fn ordered_candidates(
    candidates: Vec<StoredProfile>,
    scheduler: &Arc<Mutex<WeightedScheduler>>,
    route_key: &str,
) -> Vec<StoredProfile> {
    let Some(priority) = candidates
        .iter()
        .map(|candidate| candidate.profile.priority)
        .min()
    else {
        return candidates;
    };
    let mut tier = candidates
        .into_iter()
        .filter(|candidate| candidate.profile.priority == priority)
        .collect::<Vec<_>>();
    let now = timestamp_ms();
    let fresh_quota = tier
        .iter()
        .filter_map(|candidate| {
            let quota = candidate.profile.account.as_ref()?.quota.primary.as_ref()?;
            let synced = candidate.profile.account.as_ref()?.quota.synced_at_ms?;
            (candidate.profile.account.as_ref()?.quota.status == "available"
                && now.saturating_sub(synced) <= 5 * 60 * 1000)
                .then_some((candidate.profile.id.clone(), quota.remaining_percent))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut fallback = Vec::new();
    if !fresh_quota.is_empty() {
        let mut preferred = Vec::new();
        for candidate in tier {
            if fresh_quota.contains_key(&candidate.profile.id) {
                preferred.push(candidate);
            } else {
                fallback.push(candidate);
            }
        }
        tier = preferred;
    }
    let selected_id = scheduler
        .lock()
        .ok()
        .and_then(|mut scheduler| scheduler.select(route_key, &tier));
    tier.sort_by(|left, right| {
        let left_selected = selected_id.as_deref() == Some(left.profile.id.as_str());
        let right_selected = selected_id.as_deref() == Some(right.profile.id.as_str());
        right_selected
            .cmp(&left_selected)
            .then_with(|| {
                let left_quota = fresh_quota.get(&left.profile.id).copied().unwrap_or(-1.0);
                let right_quota = fresh_quota.get(&right.profile.id).copied().unwrap_or(-1.0);
                right_quota
                    .partial_cmp(&left_quota)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| right.profile.weight.cmp(&left.profile.weight))
    });
    fallback.sort_by(|left, right| right.profile.weight.cmp(&left.profile.weight));
    tier.extend(fallback);
    tier
}

impl WeightedScheduler {
    fn select(&mut self, route_key: &str, candidates: &[StoredProfile]) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let scores = self.scores.entry(route_key.to_owned()).or_default();
        let eligible = candidates
            .iter()
            .map(|candidate| candidate.profile.id.as_str())
            .collect::<HashSet<_>>();
        scores.retain(|id, _| eligible.contains(id.as_str()));
        let total = candidates
            .iter()
            .map(|candidate| candidate.profile.weight.max(1))
            .sum::<i64>();
        for candidate in candidates {
            *scores.entry(candidate.profile.id.clone()).or_default() +=
                candidate.profile.weight.max(1);
        }
        let selected = candidates
            .iter()
            .max_by_key(|candidate| {
                scores
                    .get(&candidate.profile.id)
                    .copied()
                    .unwrap_or_default()
            })?
            .profile
            .id
            .clone();
        *scores.entry(selected.clone()).or_default() -= total;
        Some(selected)
    }
}

enum OAuthAttempt {
    Response(Response, i64),
    RetryAfter(Duration),
    ReauthorizationRequired,
    TemporaryFailure,
}

async fn forward_oauth_candidate(
    state: &GatewayApiState,
    client: &Client,
    candidate: &StoredProfile,
    client_payload: &Value,
    kind: GatewayRequestKind,
) -> OAuthAttempt {
    let client_stream = client_payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let upstream_payload = match kind {
        GatewayRequestKind::Responses => {
            let mut value = client_payload.clone();
            if let Some(object) = value.as_object_mut() {
                object.insert("stream".to_owned(), Value::Bool(true));
                object.insert("store".to_owned(), Value::Bool(false));
            }
            value
        }
        GatewayRequestKind::ChatCompletions => match chat_to_responses(client_payload) {
            Ok(value) => value,
            Err(message) => return OAuthAttempt::Response(openai_bad_request(&message), 0),
        },
    };
    let mut credential = match state
        .oauth_credentials
        .current_or_refresh(candidate, CredentialAccess::Background)
        .await
    {
        Ok(credential) => credential,
        Err(AppError::KeychainInteractionRequired | AppError::ProfileRuntimeUnavailable) => {
            return OAuthAttempt::ReauthorizationRequired
        }
        Err(_) => return OAuthAttempt::TemporaryFailure,
    };
    let mut physical_attempts = 1_i64;
    let mut response = match send_oauth_request(
        client,
        &state.oauth_responses_url,
        &credential,
        &upstream_payload,
    )
    .await
    {
        Ok(response) => response,
        Err(_) => {
            record_gateway_upstream_error(&state.repository, GATEWAY_ERROR_FIRST_RESPONSE);
            return OAuthAttempt::TemporaryFailure;
        }
    };
    if response.status() == StatusCode::UNAUTHORIZED {
        physical_attempts = 2;
        credential = match state
            .oauth_credentials
            .refresh(candidate, CredentialAccess::Background, true)
            .await
        {
            Ok(credential) => credential,
            Err(_) => return OAuthAttempt::ReauthorizationRequired,
        };
        let _ = crate::profiles::save_oauth_credential_metadata(
            &state.repository,
            &candidate.profile.id,
            &credential,
        );
        response = match send_oauth_request(
            client,
            &state.oauth_responses_url,
            &credential,
            &upstream_payload,
        )
        .await
        {
            Ok(response) => response,
            Err(_) => {
                record_gateway_upstream_error(&state.repository, GATEWAY_ERROR_FIRST_RESPONSE);
                return OAuthAttempt::TemporaryFailure;
            }
        };
        if response.status() == StatusCode::UNAUTHORIZED {
            return OAuthAttempt::ReauthorizationRequired;
        }
    }
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return OAuthAttempt::RetryAfter(
            retry_after(&response).unwrap_or_else(|| Duration::from_secs(30)),
        );
    }
    if response.status().is_server_error() {
        return OAuthAttempt::RetryAfter(Duration::from_secs(15));
    }
    if !response.status().is_success() {
        return OAuthAttempt::Response(
            upstream_response(response, state.repository.clone(), Some(kind)),
            physical_attempts,
        );
    }
    let model = client_payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("codex")
        .to_owned();
    OAuthAttempt::Response(
        adapt_oauth_response(
            response,
            kind,
            client_stream,
            model,
            candidate.profile.id.clone(),
            state.repository.clone(),
            state.affinities.clone(),
        )
        .await,
        physical_attempts,
    )
}

async fn send_oauth_request(
    client: &Client,
    endpoint: &Url,
    credential: &crate::profiles::CodexOAuthCredential,
    payload: &Value,
) -> AppResult<reqwest::Response> {
    let mut request = client
        .post(endpoint.clone())
        .bearer_auth(&credential.access_token)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header("originator", "codex_cli_rs")
        .json(payload);
    if let Some(account_id) = credential
        .account_id
        .as_deref()
        .filter(|account_id| !account_id.is_empty())
    {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    send_with_first_response_timeout(request).await
}

async fn adapt_oauth_response(
    response: reqwest::Response,
    kind: GatewayRequestKind,
    client_stream: bool,
    model: String,
    profile_id: String,
    repository: Arc<Repository>,
    affinities: Arc<Mutex<HashMap<String, ResponseAffinity>>>,
) -> Response {
    if client_stream {
        return match kind {
            GatewayRequestKind::Responses => {
                let repository = repository.clone();
                let stream = response
                    .bytes_stream()
                    .map_err(std::io::Error::other)
                    .map(Some)
                    .chain(futures_util::stream::once(async { None }))
                    .scan(
                        (String::new(), StreamTerminalState::default()),
                        move |(buffer, terminal), chunk| {
                            let affinities = affinities.clone();
                            let profile_id = profile_id.clone();
                            let result = match chunk {
                                Some(Ok(bytes)) => {
                                    terminal.observe_sse(&bytes);
                                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                                    for event in drain_sse_events(buffer) {
                                        if let Some(id) = response_id_from_event(&event) {
                                            record_affinity(&affinities, &id, &profile_id);
                                        }
                                    }
                                    Some(Ok::<Bytes, std::io::Error>(bytes))
                                }
                                Some(Err(_)) | None => {
                                    let interruption =
                                        terminal.interruption(Some(GatewayRequestKind::Responses));
                                    if interruption.is_none() {
                                        return futures_util::future::ready(None);
                                    }
                                    record_gateway_upstream_error(
                                        &repository,
                                        GATEWAY_ERROR_STREAM_INTERRUPTED,
                                    );
                                    Some(Ok(interruption.unwrap_or_default()))
                                }
                            };
                            futures_util::future::ready(result)
                        },
                    );
                sse_response(Body::from_stream(stream))
            }
            GatewayRequestKind::ChatCompletions => {
                let repository = repository.clone();
                let stream = response
                    .bytes_stream()
                    .map_err(std::io::Error::other)
                    .map(Some)
                    .chain(futures_util::stream::once(async { None }))
                    .scan(
                        ChatStreamState::new(model, profile_id, affinities),
                        move |state, chunk| {
                            let result = match chunk {
                                Some(Ok(bytes)) => {
                                    state.buffer.push_str(&String::from_utf8_lossy(&bytes));
                                    let events = drain_sse_events(&mut state.buffer);
                                    let mut output = String::new();
                                    for event in events {
                                        output.push_str(&state.translate(event));
                                    }
                                    Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                                }
                                Some(Err(_)) | None => {
                                    if state.done {
                                        return futures_util::future::ready(None);
                                    }
                                    record_gateway_upstream_error(
                                        &repository,
                                        GATEWAY_ERROR_STREAM_INTERRUPTED,
                                    );
                                    state.done = true;
                                    Some(Ok(Bytes::from(sse_upstream_error_event(Some(
                                        GatewayRequestKind::ChatCompletions,
                                    )))))
                                }
                            };
                            futures_util::future::ready(result)
                        },
                    );
                sse_response(Body::from_stream(stream))
            }
        };
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let Some(completed) = completed_response(&bytes) else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "invalid_upstream_response"}})),
        )
            .into_response();
    };
    if let Some(id) = completed.get("id").and_then(Value::as_str) {
        record_affinity(&affinities, id, &profile_id);
    }
    let _ = repository.add_estimated_tokens(usage_tokens_from_value(&completed));
    match kind {
        GatewayRequestKind::Responses => Json(completed).into_response(),
        GatewayRequestKind::ChatCompletions => {
            Json(response_to_chat_completion(&completed, &model)).into_response()
        }
    }
}

fn sse_response(body: Body) -> Response {
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    response
}

fn validate_chat_request(payload: &Value) -> Result<(), String> {
    if payload.get("messages").and_then(Value::as_array).is_none() {
        return Err("messages is required".to_owned());
    }
    Ok(())
}

fn openai_bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": "unsupported_parameter"
            }
        })),
    )
        .into_response()
}

fn chat_to_responses(payload: &Value) -> Result<Value, String> {
    validate_chat_request(payload)?;
    let mut input = Vec::new();
    for message in payload
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool_call_id is required".to_owned())?;
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": message.get("content").cloned().unwrap_or(Value::String(String::new()))
            }));
            continue;
        }
        if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
            input.push(json!({"role": role, "content": content}));
        }
        if role == "assistant" {
            for tool_call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = tool_call.get("function").cloned().unwrap_or_default();
                input.push(json!({
                    "type": "function_call",
                    "call_id": tool_call.get("id").cloned().unwrap_or_default(),
                    "name": function.get("name").cloned().unwrap_or_default(),
                    "arguments": function.get("arguments").cloned().unwrap_or(Value::String("{}".to_owned()))
                }));
            }
        }
    }
    let tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("function"))
                .map(|function| {
                    json!({
                        "type": "function",
                        "name": function.get("name").cloned().unwrap_or_default(),
                        "description": function.get("description").cloned().unwrap_or_default(),
                        "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                        "strict": function.get("strict").cloned().unwrap_or(Value::Bool(false))
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut output = json!({
        "model": payload.get("model").cloned().unwrap_or_default(),
        "input": input,
        "stream": true,
        "store": false
    });
    let object = output
        .as_object_mut()
        .ok_or_else(|| "invalid request".to_owned())?;
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    for field in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(value) = payload.get(field).filter(|value| !value.is_null()) {
            object.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(value) = payload
        .get("max_completion_tokens")
        .or_else(|| payload.get("max_tokens"))
        .filter(|value| !value.is_null())
    {
        object.insert("max_output_tokens".to_owned(), value.clone());
    }
    if let Some(effort) = payload
        .get("reasoning_effort")
        .filter(|value| !value.is_null())
    {
        object.insert("reasoning".to_owned(), json!({"effort": effort}));
    }
    if let Some(choice) = payload.get("tool_choice").filter(|value| !value.is_null()) {
        let mapped = choice
            .get("function")
            .and_then(|function| function.get("name"))
            .map(|name| json!({"type": "function", "name": name}))
            .unwrap_or_else(|| choice.clone());
        object.insert("tool_choice".to_owned(), mapped);
    }
    Ok(output)
}

fn responses_to_chat_completion(payload: &Value, upstream_model: &str) -> Result<Value, String> {
    let mut messages = Vec::new();
    if let Some(instructions) = payload
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    match payload.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role": "user", "content": text}));
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        messages.push(json!({
                            "role": role,
                            "content": responses_content_to_chat(item.get("content").unwrap_or(&Value::Null))
                        }));
                    }
                    Some("function_call_output") => {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").cloned().unwrap_or_default(),
                            "content": item.get("output").cloned().unwrap_or_else(|| Value::String(String::new()))
                        }));
                    }
                    Some("function_call") => {
                        messages.push(json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or_default(),
                                "type": "function",
                                "function": {
                                    "name": item.get("name").cloned().unwrap_or_default(),
                                    "arguments": item.get("arguments").cloned().unwrap_or_else(|| Value::String("{}".to_owned()))
                                }
                            }]
                        }));
                    }
                    _ => {
                        if let Some(role) = item.get("role").and_then(Value::as_str) {
                            messages.push(json!({
                                "role": role,
                                "content": responses_content_to_chat(item.get("content").unwrap_or(item))
                            }));
                        }
                    }
                }
            }
        }
        _ => return Err("input is required".to_owned()),
    }
    if messages.is_empty() {
        return Err("input is required".to_owned());
    }
    let tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.get("name").cloned().unwrap_or_default(),
                            "description": tool.get("description").cloned().unwrap_or_default(),
                            "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                            "strict": tool.get("strict").cloned().unwrap_or(Value::Bool(false))
                        }
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut output = json!({
        "model": upstream_model,
        "messages": messages,
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false)
    });
    let object = output
        .as_object_mut()
        .ok_or_else(|| "invalid request".to_owned())?;
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    for field in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(value) = payload.get(field).filter(|value| !value.is_null()) {
            object.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(value) = payload
        .get("max_output_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .or_else(|| payload.get("max_tokens"))
        .filter(|value| !value.is_null())
    {
        object.insert("max_tokens".to_owned(), value.clone());
    }
    if let Some(choice) = payload.get("tool_choice").filter(|value| !value.is_null()) {
        let mapped = choice
            .get("name")
            .map(|name| json!({"type": "function", "function": {"name": name}}))
            .unwrap_or_else(|| choice.clone());
        object.insert("tool_choice".to_owned(), mapped);
    }
    Ok(output)
}

fn responses_content_to_chat(content: &Value) -> Value {
    match content {
        Value::String(_) => content.clone(),
        Value::Array(parts) => Value::String(
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("input_text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => Value::String(String::new()),
    }
}

fn chat_messages(payload: &Value) -> Result<Vec<Value>, String> {
    payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "messages is required".to_owned())
}

fn openai_tools_to_anthropic(payload: &Value) -> Vec<Value> {
    payload
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("function"))
        .map(|function| {
            json!({
                "name": function.get("name").cloned().unwrap_or_default(),
                "description": function.get("description").cloned().unwrap_or_default(),
                "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
            })
        })
        .collect()
}

fn openai_tools_to_gemini(payload: &Value) -> Vec<Value> {
    let declarations = payload
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("function"))
        .map(|function| {
            json!({
                "name": function.get("name").cloned().unwrap_or_default(),
                "description": function.get("description").cloned().unwrap_or_default(),
                "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
            })
        })
        .collect::<Vec<_>>();
    if declarations.is_empty() {
        Vec::new()
    } else {
        vec![json!({"functionDeclarations": declarations})]
    }
}

fn anthropic_chat_payload(payload: &Value, model: &str, stream: bool) -> Result<Value, String> {
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    for message in chat_messages(payload)? {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" {
            if let Some(text) = openai_content_text(message.get("content").unwrap_or(&Value::Null))
            {
                system_parts.push(text);
            }
            continue;
        }
        if role == "tool" {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message.get("tool_call_id").cloned().unwrap_or_default(),
                    "content": openai_content_text(message.get("content").unwrap_or(&Value::Null)).unwrap_or_default()
                }]
            }));
            continue;
        }
        let anthropic_role = if role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        let mut content =
            openai_content_to_anthropic_blocks(message.get("content").unwrap_or(&Value::Null));
        if role == "assistant" {
            for tool_call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = tool_call.get("function").cloned().unwrap_or_default();
                content.push(json!({
                    "type": "tool_use",
                    "id": tool_call.get("id").cloned().unwrap_or_default(),
                    "name": function.get("name").cloned().unwrap_or_default(),
                    "input": serde_json::from_str::<Value>(
                        function.get("arguments").and_then(Value::as_str).unwrap_or("{}")
                    ).unwrap_or_else(|_| json!({"arguments": function.get("arguments").cloned().unwrap_or_default()}))
                }));
            }
        }
        if content.is_empty() {
            content.push(json!({"type":"text", "text":""}));
        }
        messages.push(json!({"role": anthropic_role, "content": content}));
    }
    let mut output = json!({
        "model": model,
        "max_tokens": payload
            .get("max_tokens")
            .or_else(|| payload.get("max_completion_tokens"))
            .or_else(|| payload.get("max_output_tokens"))
            .cloned()
            .unwrap_or_else(|| json!(4096)),
        "messages": messages,
        "stream": stream
    });
    let object = output
        .as_object_mut()
        .ok_or_else(|| "invalid request".to_owned())?;
    if !system_parts.is_empty() {
        object.insert("system".to_owned(), Value::String(system_parts.join("\n")));
    }
    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(value) = payload.get(from).filter(|value| !value.is_null()) {
            object.insert(to.to_owned(), value.clone());
        }
    }
    if let Some(stop) = payload.get("stop").filter(|value| !value.is_null()) {
        object.insert("stop_sequences".to_owned(), stop.clone());
    }
    let tools = openai_tools_to_anthropic(payload);
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(choice) = payload.get("tool_choice").filter(|value| !value.is_null()) {
        object.insert("tool_choice".to_owned(), anthropic_tool_choice(choice));
    }
    Ok(output)
}

fn anthropic_tool_choice(choice: &Value) -> Value {
    if let Some(text) = choice.as_str() {
        return match text {
            "none" => json!({"type":"none"}),
            "required" => json!({"type":"any"}),
            _ => json!({"type":"auto"}),
        };
    }
    choice
        .get("function")
        .and_then(|function| function.get("name"))
        .map(|name| json!({"type":"tool", "name": name}))
        .unwrap_or_else(|| json!({"type":"auto"}))
}

fn openai_content_to_anthropic_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"type":"text", "text": text})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text" | "input_text") => Some(json!({
                    "type":"text",
                    "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                })),
                Some("image_url" | "input_image") => part
                    .get("image_url")
                    .and_then(|value| value.get("url").or(Some(value)))
                    .and_then(Value::as_str)
                    .and_then(data_url_image_source)
                    .map(|source| json!({"type":"image", "source": source})),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn data_url_image_source(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (media, data) = rest.split_once(",")?;
    let media_type = media.strip_suffix(";base64").unwrap_or(media);
    Some(json!({"type":"base64", "media_type": media_type, "data": data}))
}

fn openai_content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("input_text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Null => None,
        _ => Some(content.to_string()),
    }
}

fn gemini_chat_payload(payload: &Value, _stream: bool) -> Result<Value, String> {
    let mut contents = Vec::new();
    let mut system_parts = Vec::new();
    for message in chat_messages(payload)? {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" {
            if let Some(text) = openai_content_text(message.get("content").unwrap_or(&Value::Null))
            {
                system_parts.push(json!({"text": text}));
            }
            continue;
        }
        if role == "tool" {
            contents.push(json!({
                "role": "user",
                "parts": [{"functionResponse": {
                    "name": message.get("tool_name").and_then(Value::as_str).unwrap_or("tool"),
                    "response": {"content": message.get("content").cloned().unwrap_or_default()}
                }}]
            }));
            continue;
        }
        let mut parts =
            openai_content_to_gemini_parts(message.get("content").unwrap_or(&Value::Null));
        if role == "assistant" {
            for tool_call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = tool_call.get("function").cloned().unwrap_or_default();
                parts.push(json!({"functionCall": {
                    "name": function.get("name").cloned().unwrap_or_default(),
                    "args": serde_json::from_str::<Value>(
                        function.get("arguments").and_then(Value::as_str).unwrap_or("{}")
                    ).unwrap_or_default()
                }}));
            }
        }
        if parts.is_empty() {
            parts.push(json!({"text":""}));
        }
        contents.push(json!({
            "role": if role == "assistant" { "model" } else { "user" },
            "parts": parts
        }));
    }
    let mut output = json!({"contents": contents});
    let object = output
        .as_object_mut()
        .ok_or_else(|| "invalid request".to_owned())?;
    if !system_parts.is_empty() {
        object.insert(
            "systemInstruction".to_owned(),
            json!({"parts": system_parts}),
        );
    }
    let generation_config = gemini_generation_config(payload);
    if !generation_config.is_null() {
        object.insert("generationConfig".to_owned(), generation_config);
    }
    let tools = openai_tools_to_gemini(payload);
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(tool_config) = gemini_tool_config(payload.get("tool_choice")) {
        object.insert("toolConfig".to_owned(), tool_config);
    }
    Ok(output)
}

fn openai_content_to_gemini_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"text": text})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text" | "input_text") => Some(json!({
                    "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                })),
                Some("image_url" | "input_image") => part
                    .get("image_url")
                    .and_then(|value| value.get("url").or(Some(value)))
                    .and_then(Value::as_str)
                    .and_then(|url| {
                        let rest = url.strip_prefix("data:")?;
                        let (media, data) = rest.split_once(",")?;
                        Some(json!({"inlineData": {
                            "mimeType": media.strip_suffix(";base64").unwrap_or(media),
                            "data": data
                        }}))
                    }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn gemini_generation_config(payload: &Value) -> Value {
    let mut config = serde_json::Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("seed", "seed"),
    ] {
        if let Some(value) = payload.get(from).filter(|value| !value.is_null()) {
            config.insert(to.to_owned(), value.clone());
        }
    }
    if let Some(value) = payload
        .get("max_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .or_else(|| payload.get("max_output_tokens"))
        .filter(|value| !value.is_null())
    {
        config.insert("maxOutputTokens".to_owned(), value.clone());
    }
    if let Some(stop) = payload.get("stop").filter(|value| !value.is_null()) {
        config.insert("stopSequences".to_owned(), stop.clone());
    }
    if let Some(format) = payload
        .get("response_format")
        .filter(|value| !value.is_null())
    {
        if format.get("type").and_then(Value::as_str) == Some("json_object") {
            config.insert(
                "responseMimeType".to_owned(),
                Value::String("application/json".to_owned()),
            );
        }
        if let Some(schema) = format
            .get("json_schema")
            .and_then(|json_schema| json_schema.get("schema"))
            .or_else(|| format.get("schema"))
        {
            config.insert(
                "responseMimeType".to_owned(),
                Value::String("application/json".to_owned()),
            );
            config.insert("responseSchema".to_owned(), schema.clone());
        }
    }
    Value::Object(config)
}

fn gemini_tool_config(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?.clone();
    if choice.is_null() {
        return None;
    }
    let mode = match choice.as_str() {
        Some("none") => "NONE",
        Some("required") => "ANY",
        Some("auto") | None => "AUTO",
        _ => "AUTO",
    };
    let mut config = json!({"functionCallingConfig": {"mode": mode}});
    if let Some(name) = choice
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
    {
        config["functionCallingConfig"]["mode"] = Value::String("ANY".to_owned());
        config["functionCallingConfig"]["allowedFunctionNames"] = json!([name]);
    }
    Some(config)
}

fn ollama_chat_payload_from_openai(
    payload: &Value,
    model: &str,
    stream: bool,
) -> Result<Value, String> {
    let mut messages = chat_messages(payload)?;
    for message in &mut messages {
        if let Some(object) = message.as_object_mut() {
            if object.get("content").is_some_and(Value::is_array) {
                let content = Value::Array(
                    object
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| {
                            part.get("text")
                                .or_else(|| part.get("input_text"))
                                .and_then(Value::as_str)
                        })
                        .map(|text| Value::String(text.to_owned()))
                        .collect(),
                );
                object.insert(
                    "content".to_owned(),
                    Value::String(
                        content
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                );
            }
        }
    }
    let mut output = json!({"model": model, "messages": messages, "stream": stream});
    let object = output
        .as_object_mut()
        .ok_or_else(|| "invalid request".to_owned())?;
    if let Some(tools) = payload.get("tools").filter(|value| !value.is_null()) {
        object.insert("tools".to_owned(), tools.clone());
    }
    if let Some(format) = payload
        .get("response_format")
        .filter(|value| !value.is_null())
    {
        if format.get("type").and_then(Value::as_str) == Some("json_object") {
            object.insert("format".to_owned(), Value::String("json".to_owned()));
        } else if let Some(schema) = format
            .get("json_schema")
            .and_then(|json_schema| json_schema.get("schema"))
            .or_else(|| format.get("schema"))
        {
            object.insert("format".to_owned(), schema.clone());
        }
    }
    let mut options = serde_json::Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("seed", "seed"),
        ("stop", "stop"),
    ] {
        if let Some(value) = payload.get(from).filter(|value| !value.is_null()) {
            options.insert(to.to_owned(), value.clone());
        }
    }
    if let Some(value) = payload
        .get("max_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .or_else(|| payload.get("max_output_tokens"))
        .filter(|value| !value.is_null())
    {
        options.insert("num_predict".to_owned(), value.clone());
    }
    if !options.is_empty() {
        object.insert("options".to_owned(), Value::Object(options));
    }
    if let Some(reasoning) = payload
        .get("reasoning")
        .or_else(|| payload.get("reasoning_effort"))
    {
        object.insert("think".to_owned(), reasoning.clone());
    }
    Ok(output)
}

async fn adapt_chat_completion_response(
    response: reqwest::Response,
    client_stream: bool,
    model: String,
    repository: Arc<Repository>,
) -> Response {
    if client_stream {
        let stream = response
            .bytes_stream()
            .map_err(std::io::Error::other)
            .map(Some)
            .chain(futures_util::stream::once(async { None }))
            .scan(
                ChatToResponsesStreamState::new(model),
                move |state, chunk| {
                    let result = match chunk {
                        Some(Ok(bytes)) => {
                            state.buffer.push_str(&String::from_utf8_lossy(&bytes));
                            let events = drain_sse_events(&mut state.buffer);
                            let mut output = String::new();
                            for event in events {
                                output.push_str(&state.translate(event));
                            }
                            Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                        }
                        Some(Err(_)) | None => {
                            if state.done {
                                return futures_util::future::ready(None);
                            }
                            record_gateway_upstream_error(
                                &repository,
                                GATEWAY_ERROR_STREAM_INTERRUPTED,
                            );
                            state.done = true;
                            Some(Ok(Bytes::from(sse_upstream_error_event(Some(
                                GatewayRequestKind::Responses,
                            )))))
                        }
                    };
                    futures_util::future::ready(result)
                },
            );
        return sse_response(Body::from_stream(stream));
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let Some(completed) = completed_chat_completion(&bytes) else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "invalid_upstream_response"}})),
        )
            .into_response();
    };
    let _ = repository.add_estimated_tokens(usage_tokens_from_value(&completed));
    Json(chat_completion_to_response(&completed, &model)).into_response()
}

fn completed_chat_completion(bytes: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        return Some(value);
    }
    let text = String::from_utf8_lossy(bytes);
    parse_sse_events(&text).into_iter().rev().find(|event| {
        event
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| !choices.is_empty())
    })
}

fn chat_completion_to_response(completion: &Value, model: &str) -> Value {
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or_default();
    let message = choice.get("message").cloned().unwrap_or_default();
    let mut output = Vec::new();
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": content}]
        }));
    }
    for tool_call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = tool_call.get("function").cloned().unwrap_or_default();
        output.push(json!({
            "type": "function_call",
            "id": tool_call.get("id").cloned().unwrap_or_default(),
            "call_id": tool_call.get("id").cloned().unwrap_or_default(),
            "name": function.get("name").cloned().unwrap_or_default(),
            "arguments": function.get("arguments").cloned().unwrap_or(Value::String("{}".to_owned()))
        }));
    }
    json!({
        "id": completion.get("id").cloned().unwrap_or_else(|| Value::String(format!("resp_{}", uuid::Uuid::new_v4()))),
        "object": "response",
        "created_at": completion.get("created").cloned().unwrap_or_else(|| json!(timestamp_ms() / 1000)),
        "model": model,
        "status": "completed",
        "output": output,
        "usage": responses_usage_from_chat(completion.get("usage"))
    })
}

fn responses_usage_from_chat(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output = usage
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": usage
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(input + output)
    })
}

struct ChatToResponsesStreamState {
    buffer: String,
    id: String,
    model: String,
    created: i64,
    text: String,
    emitted_created: bool,
    done: bool,
}

impl ChatToResponsesStreamState {
    fn new(model: String) -> Self {
        Self {
            buffer: String::new(),
            id: format!("resp_{}", uuid::Uuid::new_v4()),
            model,
            created: timestamp_ms() / 1000,
            text: String::new(),
            emitted_created: false,
            done: false,
        }
    }

    fn translate(&mut self, event: Value) -> String {
        if let Some(id) = event.get("id").and_then(Value::as_str) {
            self.id = id.to_owned();
        }
        let Some(choice) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return String::new();
        };
        let mut output = String::new();
        if !self.emitted_created {
            self.emitted_created = true;
            output.push_str(&format!(
                "event: response.created\ndata: {}\n\n",
                json!({
                    "type": "response.created",
                    "response": {
                        "id": self.id,
                        "object": "response",
                        "created_at": self.created,
                        "model": self.model,
                        "status": "in_progress",
                        "output": []
                    }
                })
            ));
        }
        if let Some(delta) = choice
            .get("delta")
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
        {
            self.text.push_str(delta);
            output.push_str(&format!(
                "event: response.output_text.delta\ndata: {}\n\n",
                json!({"type": "response.output_text.delta", "delta": delta})
            ));
        }
        if choice
            .get("finish_reason")
            .is_some_and(|value| !value.is_null())
        {
            self.done = true;
            output.push_str(&format!(
                "event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": self.id,
                        "object": "response",
                        "created_at": self.created,
                        "model": self.model,
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": self.text}]
                        }]
                    }
                })
            ));
        }
        output
    }
}

fn completed_response(bytes: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        return Some(value.get("response").cloned().unwrap_or(value));
    }
    let text = String::from_utf8_lossy(bytes);
    parse_sse_events(&text).into_iter().rev().find_map(|event| {
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            event.get("response").cloned()
        } else if event.get("object").and_then(Value::as_str) == Some("response") {
            Some(event)
        } else {
            None
        }
    })
}

fn parse_sse_events(text: &str) -> Vec<Value> {
    text.split("\n\n").filter_map(parse_sse_event).collect()
}

fn drain_sse_events(buffer: &mut String) -> Vec<Value> {
    let normalized = buffer.replace("\r\n", "\n");
    *buffer = normalized;
    let Some(last_boundary) = buffer.rfind("\n\n") else {
        return Vec::new();
    };
    let complete = buffer[..last_boundary + 2].to_owned();
    buffer.drain(..last_boundary + 2);
    parse_sse_events(&complete)
}

fn parse_sse_event(block: &str) -> Option<Value> {
    let data = block
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str(&data).ok()
}

fn response_id_from_event(event: &Value) -> Option<String> {
    event
        .get("response")
        .and_then(|response| response.get("id"))
        .or_else(|| event.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn response_to_chat_completion(response: &Value, model: &str) -> Value {
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text" | "text")
                    ) {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            content.push_str(text);
                        }
                    }
                }
            }
            Some("function_call") => tool_calls.push(json!({
                "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or_default(),
                    "arguments": item.get("arguments").cloned().unwrap_or(Value::String("{}".to_owned()))
                }
            })),
            _ => {}
        }
    }
    let mut message = json!({"role": "assistant", "content": content});
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": response.get("id").cloned().unwrap_or_else(|| Value::String(format!("chatcmpl-{}", uuid::Uuid::new_v4()))),
        "object": "chat.completion",
        "created": response.get("created_at").cloned().unwrap_or_else(|| json!(timestamp_ms() / 1000)),
        "model": response.get("model").cloned().unwrap_or_else(|| Value::String(model.to_owned())),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if message.get("tool_calls").is_some() {"tool_calls"} else {"stop"}
        }],
        "usage": chat_usage(response.get("usage"))
    })
}

fn chat_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output = usage
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": usage
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(input + output)
    })
}

struct ChatStreamState {
    buffer: String,
    id: String,
    model: String,
    created: i64,
    profile_id: String,
    affinities: Arc<Mutex<HashMap<String, ResponseAffinity>>>,
    tool_indexes: HashMap<i64, usize>,
    has_tools: bool,
    done: bool,
}

impl ChatStreamState {
    fn new(
        model: String,
        profile_id: String,
        affinities: Arc<Mutex<HashMap<String, ResponseAffinity>>>,
    ) -> Self {
        Self {
            buffer: String::new(),
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            model,
            created: timestamp_ms() / 1000,
            profile_id,
            affinities,
            tool_indexes: HashMap::new(),
            has_tools: false,
            done: false,
        }
    }

    fn translate(&mut self, event: Value) -> String {
        if let Some(id) = response_id_from_event(&event) {
            self.id = id.clone();
            record_affinity(&self.affinities, &id, &self.profile_id);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("response.created") => self.chunk(json!({"role": "assistant"}), None, None),
            Some("response.output_text.delta") => self.chunk(
                json!({"content": event.get("delta").cloned().unwrap_or_default()}),
                None,
                None,
            ),
            Some("response.output_item.added")
                if event
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("function_call") =>
            {
                self.has_tools = true;
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                let index = self.tool_indexes.len();
                self.tool_indexes.insert(output_index, index);
                let item = event.get("item").cloned().unwrap_or_default();
                self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or_default(),
                        "type": "function",
                        "function": {"name": item.get("name").cloned().unwrap_or_default(), "arguments": ""}
                    }]}),
                    None,
                    None,
                )
            }
            Some("response.function_call_arguments.delta") => {
                self.has_tools = true;
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                let next_index = self.tool_indexes.len();
                let index = *self.tool_indexes.entry(output_index).or_insert(next_index);
                self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "function": {"arguments": event.get("delta").cloned().unwrap_or_default()}
                    }]}),
                    None,
                    None,
                )
            }
            Some("response.completed") => {
                self.done = true;
                let usage = event
                    .get("response")
                    .and_then(|response| response.get("usage"));
                format!(
                    "{}data: [DONE]\n\n",
                    self.chunk(
                        json!({}),
                        Some(if self.has_tools { "tool_calls" } else { "stop" }),
                        Some(chat_usage(usage)),
                    )
                )
            }
            Some("response.failed" | "response.incomplete") => {
                self.done = true;
                "data: {\"error\":{\"code\":\"upstream_response_failed\"}}\n\ndata: [DONE]\n\n"
                    .to_owned()
            }
            _ => String::new(),
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> String {
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        format!("data: {chunk}\n\n")
    }
}

fn cool_down_exhausted_profiles(repository: &Repository, candidates: &mut Vec<StoredProfile>) {
    let now = timestamp_ms();
    candidates.retain(|candidate| {
        let Some(account) = candidate.profile.account.as_ref() else {
            return true;
        };
        let Some(synced) = account.quota.synced_at_ms else {
            return true;
        };
        let Some(primary) = account.quota.primary.as_ref() else {
            return true;
        };
        if now.saturating_sub(synced) > 5 * 60 * 1000 || primary.remaining_percent > 0.0 {
            return true;
        }
        let duration_ms = primary
            .resets_at_ms
            .map(|reset| reset.saturating_sub(now))
            .unwrap_or(60_000)
            .max(1_000) as u64;
        cool_down_profile(
            repository,
            &candidate.profile.id,
            Duration::from_millis(duration_ms),
        );
        false
    });
}

fn affinity_profile(state: &GatewayApiState, response_id: &str) -> Option<String> {
    let now = timestamp_ms();
    let mut affinities = state.affinities.lock().ok()?;
    affinities.retain(|_, affinity| affinity.expires_at_ms > now);
    affinities
        .get(response_id)
        .map(|affinity| affinity.profile_id.clone())
}

fn record_affinity(
    affinities: &Arc<Mutex<HashMap<String, ResponseAffinity>>>,
    response_id: &str,
    profile_id: &str,
) {
    let Ok(mut affinities) = affinities.lock() else {
        return;
    };
    let now = timestamp_ms();
    affinities.retain(|_, affinity| affinity.expires_at_ms > now);
    if affinities.len() >= MAX_AFFINITIES {
        if let Some(oldest) = affinities
            .iter()
            .min_by_key(|(_, affinity)| affinity.expires_at_ms)
            .map(|(id, _)| id.clone())
        {
            affinities.remove(&oldest);
        }
    }
    affinities.insert(
        response_id.to_owned(),
        ResponseAffinity {
            profile_id: profile_id.to_owned(),
            expires_at_ms: now.saturating_add(AFFINITY_TTL_MS),
        },
    );
}

fn affinity_conflict() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": {
                "code": "response_affinity_unavailable",
                "message": "The previous response is not available on an eligible profile."
            }
        })),
    )
        .into_response()
}

fn mark_profile_health(repository: &Repository, id: &str, health: &str) {
    let Ok(mut profile) = repository.profile(id) else {
        return;
    };
    profile.profile.health = health.to_owned();
    let _ = repository.update_profile(&profile);
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

struct ProviderFanoutRequest<'a> {
    client: &'a Client,
    endpoint: Url,
    provider: GatewayProvider,
    key: String,
    upstream_payload: Value,
    choice_count: usize,
    client_stream: bool,
    model: String,
    repository: Arc<Repository>,
    concurrency: Arc<ProfileConcurrencyManager>,
    profile: MaskedProfile,
}

async fn forward_provider_chat_choices(
    request: ProviderFanoutRequest<'_>,
) -> ProviderFanoutAttempt {
    let attempts = (0..request.choice_count).map(|_| {
        let concurrency = request.concurrency.clone();
        let profile = request.profile.clone();
        let endpoint = request.endpoint.clone();
        let provider = request.provider.clone();
        let key = request.key.clone();
        let payload = request.upstream_payload.clone();
        async move {
            let (permit, queue_latency_ms) = concurrency.acquire(&profile).await?;
            let outbound =
                upstream_request(request.client.post(endpoint), &provider, &key, &payload);
            let response = send_with_first_response_timeout(outbound).await;
            Ok::<_, ProfileAcquireError>((response, queue_latency_ms, permit))
        }
    });
    let results = join_all(attempts).await;
    let mut responses = Vec::with_capacity(request.choice_count);
    let mut permits = Vec::with_capacity(request.choice_count);
    let mut queue_latency_ms = 0_i64;
    for result in results {
        let (response, queued_ms, permit) = match result {
            Ok((Ok(response), queued_ms, permit)) => (response, queued_ms, permit),
            Ok((Err(_), _, _)) => return ProviderFanoutAttempt::TemporaryFailure,
            Err(ProfileAcquireError::QueueFull) => {
                return ProviderFanoutAttempt::Backpressure("relay_queue_full")
            }
            Err(ProfileAcquireError::QueueTimeout) => {
                return ProviderFanoutAttempt::Backpressure("relay_queue_timeout")
            }
        };
        permits.push(permit);
        queue_latency_ms = queue_latency_ms.max(queued_ms);
        if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS
        {
            return ProviderFanoutAttempt::RetryAfter(
                retry_after(&response).unwrap_or_else(|| Duration::from_secs(30)),
            );
        }
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return ProviderFanoutAttempt::Unhealthy;
        }
        if !response.status().is_success() {
            return ProviderFanoutAttempt::Response(
                upstream_provider_error_response(response).await,
                false,
                queue_latency_ms,
                permits,
            );
        }
        responses.push(response);
    }
    if request.client_stream {
        ProviderFanoutAttempt::Response(
            adapt_provider_chat_fanout_stream(
                responses,
                request.provider,
                request.model,
                request.repository,
            ),
            true,
            queue_latency_ms,
            permits,
        )
    } else {
        ProviderFanoutAttempt::Response(
            adapt_provider_chat_fanout_response(
                responses,
                request.provider,
                request.model,
                request.repository,
            )
            .await,
            true,
            queue_latency_ms,
            permits,
        )
    }
}

async fn upstream_provider_error_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let bytes = response.bytes().await.unwrap_or_else(|_| Bytes::new());
    let value = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).trim().to_owned()));
    let message = match &value {
        Value::Object(object) => object
            .get("error")
            .and_then(|error| {
                error
                    .get("message")
                    .or_else(|| error.get("code"))
                    .and_then(Value::as_str)
            })
            .or_else(|| object.get("message").and_then(Value::as_str))
            .unwrap_or("upstream request failed")
            .to_owned(),
        Value::String(text) if !text.is_empty() => text.clone(),
        _ => "upstream request failed".to_owned(),
    };
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "upstream_error",
                "code": "upstream_error",
                "upstream": value
            }
        })),
    )
        .into_response()
}

async fn adapt_provider_chat_fanout_response(
    responses: Vec<reqwest::Response>,
    provider: GatewayProvider,
    model: String,
    _repository: Arc<Repository>,
) -> Response {
    let mut choices = Vec::new();
    let mut prompt_tokens = 0_i64;
    let mut completion_tokens = 0_i64;
    let mut total_tokens = 0_i64;
    let created = timestamp_ms() / 1000;

    for (index, response) in responses.into_iter().enumerate() {
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        let value = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": {"code": "invalid_upstream_response"}})),
                )
                    .into_response()
            }
        };
        let chat = provider_value_to_chat(&provider, &value, &model);
        if let Some(usage) = chat.get("usage") {
            prompt_tokens += usage
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            completion_tokens += usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            total_tokens += usage
                .get("total_tokens")
                .and_then(Value::as_i64)
                .unwrap_or_default();
        }
        let Some(mut choice) = chat
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .cloned()
        else {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": {"code": "invalid_upstream_response"}})),
            )
                .into_response();
        };
        if let Some(choice) = choice.as_object_mut() {
            choice.insert("index".to_owned(), json!(index));
        }
        choices.push(choice);
    }

    Json(json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": choices,
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens
        }
    }))
    .into_response()
}

fn adapt_provider_chat_fanout_stream(
    responses: Vec<reqwest::Response>,
    provider: GatewayProvider,
    model: String,
    repository: Arc<Repository>,
) -> Response {
    let choice_count = responses.len();
    let streams = responses.into_iter().enumerate().map(|(index, response)| {
        response
            .bytes_stream()
            .map_err(std::io::Error::other)
            .map(move |chunk| (index, chunk))
            .boxed()
    });
    let merged = futures_util::stream::select_all(streams);
    let states = (0..choice_count)
        .map(|index| {
            (
                String::new(),
                ProviderSseState::new_choice(
                    provider.clone(),
                    GatewayRequestKind::ChatCompletions,
                    model.clone(),
                    index,
                    false,
                ),
            )
        })
        .collect::<Vec<_>>();
    let stream = merged
        .map(Some)
        .chain(futures_util::stream::once(async { None }))
        .scan(
            (states, vec![false; choice_count], 0_usize, false),
            move |(states, done, done_count, terminated), item| {
                let repository = repository.clone();
                let provider = provider.clone();
                let result = if *terminated {
                    None
                } else {
                    match item {
                        Some((index, Ok(bytes))) => {
                            let Some((buffer, state)) = states.get_mut(index) else {
                                return futures_util::future::ready(None);
                            };
                            buffer.push_str(&String::from_utf8_lossy(&bytes));
                            let events = if provider == GatewayProvider::Ollama {
                                drain_json_lines(buffer)
                            } else {
                                drain_sse_events(buffer)
                            };
                            let mut output = String::new();
                            for event in events {
                                let (translated, _tokens) = if provider == GatewayProvider::Ollama {
                                    translate_ollama_event(state, event)
                                } else {
                                    state.translate(event)
                                };
                                output.push_str(&translated);
                                if state.done && !done[index] {
                                    done[index] = true;
                                    *done_count += 1;
                                }
                            }
                            if *done_count == done.len() {
                                output.push_str("data: [DONE]\n\n");
                                *terminated = true;
                            }
                            Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                        }
                        Some((_, Err(_))) | None => {
                            if *done_count == done.len() {
                                return futures_util::future::ready(None);
                            }
                            record_gateway_upstream_error(
                                &repository,
                                GATEWAY_ERROR_STREAM_INTERRUPTED,
                            );
                            *terminated = true;
                            Some(Ok(Bytes::from(sse_upstream_error_event(Some(
                                GatewayRequestKind::ChatCompletions,
                            )))))
                        }
                    }
                };
                futures_util::future::ready(result)
            },
        );
    sse_response(Body::from_stream(stream))
}

async fn adapt_provider_response(
    response: reqwest::Response,
    provider: GatewayProvider,
    kind: GatewayRequestKind,
    client_stream: bool,
    model: String,
    repository: Arc<Repository>,
) -> Response {
    if !response.status().is_success() {
        return upstream_provider_error_response(response).await;
    }
    if client_stream {
        return match provider {
            GatewayProvider::Ollama => adapt_ollama_stream(response, kind, model, repository),
            GatewayProvider::Anthropic | GatewayProvider::Gemini => {
                adapt_provider_sse_stream(response, provider, kind, model, repository)
            }
            _ => upstream_response(response, repository, Some(kind)),
        };
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": {"code": "invalid_upstream_response"}})),
            )
                .into_response()
        }
    };
    let output = match kind {
        GatewayRequestKind::Responses => provider_value_to_response(&provider, &value, &model),
        GatewayRequestKind::ChatCompletions => provider_value_to_chat(&provider, &value, &model),
    };
    Json(output).into_response()
}

fn provider_value_to_response(provider: &GatewayProvider, value: &Value, model: &str) -> Value {
    match provider {
        GatewayProvider::Anthropic => anthropic_value_to_response(value, model),
        GatewayProvider::Gemini => gemini_value_to_response(value, model),
        GatewayProvider::Ollama => ollama_value_to_response(value, model),
        _ => value.clone(),
    }
}

fn provider_value_to_chat(provider: &GatewayProvider, value: &Value, model: &str) -> Value {
    match provider {
        GatewayProvider::Anthropic => {
            response_to_chat_completion(&anthropic_value_to_response(value, model), model)
        }
        GatewayProvider::Gemini => {
            response_to_chat_completion(&gemini_value_to_response(value, model), model)
        }
        GatewayProvider::Ollama => {
            response_to_chat_completion(&ollama_value_to_response(value, model), model)
        }
        _ => value.clone(),
    }
}

fn anthropic_value_to_response(value: &Value, model: &str) -> Value {
    let mut output = Vec::new();
    for block in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => output.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": block.get("text").and_then(Value::as_str).unwrap_or_default()}]
            })),
            Some("tool_use") => output.push(json!({
                "type": "function_call",
                "id": block.get("id").cloned().unwrap_or_default(),
                "call_id": block.get("id").cloned().unwrap_or_default(),
                "name": block.get("name").cloned().unwrap_or_default(),
                "arguments": block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".to_owned())
            })),
            _ => {}
        }
    }
    json!({
        "id": value.get("id").cloned().unwrap_or_else(|| Value::String(format!("resp_{}", uuid::Uuid::new_v4()))),
        "object": "response",
        "created_at": timestamp_ms() / 1000,
        "model": model,
        "status": "completed",
        "output": output,
        "usage": responses_usage_from_anthropic(value.get("usage"))
    })
}

fn gemini_value_to_response(value: &Value, model: &str) -> Value {
    let mut output = Vec::new();
    let parts = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    for part in parts {
        if let Some(delta) = part.get("text").and_then(Value::as_str) {
            text.push_str(delta);
        }
        if let Some(call) = part.get("functionCall") {
            let call_id = format!("call_{}", uuid::Uuid::new_v4());
            output.push(json!({
                "type": "function_call",
                "id": call_id,
                "call_id": call_id,
                "name": call.get("name").cloned().unwrap_or_default(),
                "arguments": call.get("args").map(Value::to_string).unwrap_or_else(|| "{}".to_owned())
            }));
        }
    }
    if !text.is_empty() || output.is_empty() {
        output.insert(
            0,
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }),
        );
    }
    json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4()),
        "object": "response",
        "created_at": timestamp_ms() / 1000,
        "model": model,
        "status": "completed",
        "output": output,
        "usage": responses_usage_from_gemini(value.get("usageMetadata"))
    })
}

fn ollama_value_to_response(value: &Value, model: &str) -> Value {
    let message = value.get("message").cloned().unwrap_or_default();
    let mut output = Vec::new();
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": content}]
        }));
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = call
            .get("function")
            .cloned()
            .unwrap_or_else(|| call.clone());
        let call_id = format!("call_{}", uuid::Uuid::new_v4());
        output.push(json!({
            "type": "function_call",
            "id": call_id,
            "call_id": call_id,
            "name": function.get("name").cloned().unwrap_or_default(),
            "arguments": function.get("arguments").map(Value::to_string).unwrap_or_else(|| "{}".to_owned())
        }));
    }
    json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4()),
        "object": "response",
        "created_at": timestamp_ms() / 1000,
        "model": model,
        "status": "completed",
        "output": output,
        "usage": responses_usage_from_ollama(value)
    })
}

fn responses_usage_from_anthropic(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output = usage
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": input + output})
}

fn responses_usage_from_gemini(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|usage| usage.get("promptTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output = usage
        .and_then(|usage| usage.get("candidatesTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": usage.and_then(|usage| usage.get("totalTokenCount")).and_then(Value::as_i64).unwrap_or(input + output)
    })
}

fn responses_usage_from_ollama(value: &Value) -> Value {
    let input = value
        .get("prompt_eval_count")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output = value
        .get("eval_count")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": input + output})
}

#[cfg(test)]
fn provider_usage_tokens(provider: &GatewayProvider, value: &Value) -> i64 {
    match provider {
        GatewayProvider::Anthropic => value
            .get("usage")
            .map(|usage| {
                usage
                    .get("input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    + usage
                        .get("output_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
            })
            .unwrap_or_default(),
        GatewayProvider::Gemini => value
            .get("usageMetadata")
            .and_then(|usage| usage.get("totalTokenCount"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        GatewayProvider::Ollama => {
            value
                .get("prompt_eval_count")
                .and_then(Value::as_i64)
                .unwrap_or_default()
                + value
                    .get("eval_count")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
        }
        _ => usage_tokens_from_value(value),
    }
}

fn usage_breakdown_from_event(event: &Value) -> (i64, i64, i64) {
    event
        .get("response")
        .and_then(|response| response.get("usage"))
        .or_else(|| event.get("usage"))
        .map(usage_breakdown)
        .unwrap_or_default()
}

fn usage_breakdown(value: &Value) -> (i64, i64, i64) {
    let usage = value.get("usage").unwrap_or(value);
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .or_else(|| usage.get("inputTokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default()
        .max(0);
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .or_else(|| usage.get("outputTokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default()
        .max(0);
    let total = usage
        .get("total_tokens")
        .or_else(|| usage.get("totalTokens"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| input.saturating_add(output))
        .max(input.saturating_add(output));
    (input, output, total)
}

fn usage_tokens_from_value(value: &Value) -> i64 {
    usage_breakdown(value).2
}

struct ProviderSseState {
    provider: GatewayProvider,
    kind: GatewayRequestKind,
    id: String,
    model: String,
    created: i64,
    choice_index: usize,
    terminal_done: bool,
    done: bool,
    emitted_created: bool,
    text: String,
    has_tools: bool,
    tool_indexes: HashMap<String, usize>,
    tool_block_ids: HashMap<i64, String>,
    tool_names: HashMap<String, String>,
    usage: Option<Value>,
    last_usage_tokens: i64,
}

impl ProviderSseState {
    fn new(provider: GatewayProvider, kind: GatewayRequestKind, model: String) -> Self {
        Self::new_choice(provider, kind, model, 0, true)
    }

    fn new_choice(
        provider: GatewayProvider,
        kind: GatewayRequestKind,
        model: String,
        choice_index: usize,
        terminal_done: bool,
    ) -> Self {
        Self {
            provider,
            kind,
            id: match kind {
                GatewayRequestKind::Responses => format!("resp_{}", uuid::Uuid::new_v4()),
                GatewayRequestKind::ChatCompletions => format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            },
            model,
            created: timestamp_ms() / 1000,
            choice_index,
            terminal_done,
            done: false,
            emitted_created: false,
            text: String::new(),
            has_tools: false,
            tool_indexes: HashMap::new(),
            tool_block_ids: HashMap::new(),
            tool_names: HashMap::new(),
            usage: None,
            last_usage_tokens: 0,
        }
    }

    fn translate(&mut self, event: Value) -> (String, i64) {
        match self.provider {
            GatewayProvider::Anthropic => self.translate_anthropic(event),
            GatewayProvider::Gemini => self.translate_gemini(event),
            _ => (String::new(), 0),
        }
    }

    fn response_created(&mut self) -> String {
        if self.emitted_created || self.kind != GatewayRequestKind::Responses {
            return String::new();
        }
        self.emitted_created = true;
        format!(
            "event: response.created\ndata: {}\n\n",
            json!({"type":"response.created","response":{"id": self.id,"object":"response","created_at": self.created,"model": self.model,"status":"in_progress","output":[]}})
        )
    }

    fn emit_text(&mut self, delta: &str) -> String {
        self.text.push_str(delta);
        match self.kind {
            GatewayRequestKind::Responses => format!(
                "{}event: response.output_text.delta\ndata: {}\n\n",
                self.response_created(),
                json!({"type":"response.output_text.delta","delta": delta})
            ),
            GatewayRequestKind::ChatCompletions => {
                self.chat_chunk(json!({"content": delta}), None, None)
            }
        }
    }

    fn emit_tool(&mut self, id: String, name: String, arguments_delta: Option<String>) -> String {
        self.has_tools = true;
        if !name.is_empty() {
            self.tool_names.insert(id.clone(), name.clone());
        }
        match self.kind {
            GatewayRequestKind::Responses => {
                let mut output = self.response_created();
                if !self.tool_indexes.contains_key(&id) {
                    let index = self.tool_indexes.len();
                    self.tool_indexes.insert(id.clone(), index);
                    output.push_str(&format!(
                        "event: response.output_item.added\ndata: {}\n\n",
                        json!({"type":"response.output_item.added","output_index": index,"item":{"type":"function_call","id": id,"call_id": id,"name": name,"arguments":""}})
                    ));
                }
                if let Some(delta) = arguments_delta {
                    let index = self.tool_indexes.get(&id).copied().unwrap_or_default();
                    output.push_str(&format!(
                        "event: response.function_call_arguments.delta\ndata: {}\n\n",
                        json!({"type":"response.function_call_arguments.delta","output_index": index,"delta": delta})
                    ));
                }
                output
            }
            GatewayRequestKind::ChatCompletions => {
                let next = self.tool_indexes.len();
                let index = *self.tool_indexes.entry(id.clone()).or_insert(next);
                self.chat_chunk(
                    json!({"tool_calls":[{"index": index,"id": id,"type":"function","function":{"name": name,"arguments": arguments_delta.unwrap_or_default()}}]}),
                    None,
                    None,
                )
            }
        }
    }

    fn emit_done(&mut self, usage: Option<Value>) -> String {
        self.done = true;
        let usage = usage.or_else(|| self.usage.clone());
        let terminal = if self.terminal_done {
            "data: [DONE]\n\n"
        } else {
            ""
        };
        match self.kind {
            GatewayRequestKind::Responses => format!(
                "{}event: response.completed\ndata: {}\n\n{}",
                self.response_created(),
                json!({"type":"response.completed","response":{"id": self.id,"object":"response","created_at": self.created,"model": self.model,"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text": self.text}]}],"usage": usage.unwrap_or_else(|| json!({"input_tokens":0,"output_tokens":0,"total_tokens":0}))}}),
                terminal
            ),
            GatewayRequestKind::ChatCompletions => format!(
                "{}{}",
                self.chat_chunk(
                    json!({}),
                    Some(if self.has_tools { "tool_calls" } else { "stop" }),
                    usage.map(|usage| chat_usage(Some(&usage))),
                ),
                terminal
            ),
        }
    }

    fn chat_chunk(
        &self,
        delta: Value,
        finish_reason: Option<&str>,
        usage: Option<Value>,
    ) -> String {
        let mut chunk = json!({
            "id": self.id,
            "object":"chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices":[{"index":self.choice_index,"delta":delta,"finish_reason":finish_reason}]
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        format!("data: {chunk}\n\n")
    }

    fn record_usage(&mut self, usage: Value) -> i64 {
        let total = usage
            .get("total_tokens")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let delta = total.saturating_sub(self.last_usage_tokens);
        self.last_usage_tokens = self.last_usage_tokens.max(total);
        self.usage = Some(usage);
        delta
    }

    fn translate_anthropic(&mut self, event: Value) -> (String, i64) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(id) = event
                    .get("message")
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_str)
                {
                    self.id = id.to_owned();
                }
                (self.response_created(), 0)
            }
            Some("content_block_start") => {
                let block = event.get("content_block").cloned().unwrap_or_default();
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call")
                        .to_owned();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let block_index = event
                        .get("index")
                        .and_then(Value::as_i64)
                        .unwrap_or_default();
                    self.tool_block_ids.insert(block_index, id.clone());
                    self.tool_names.insert(id.clone(), name.clone());
                    (self.emit_tool(id, name, None), 0)
                } else {
                    (String::new(), 0)
                }
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").cloned().unwrap_or_default();
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => (
                        self.emit_text(
                            delta
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ),
                        0,
                    ),
                    Some("input_json_delta") => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let block_index = event
                            .get("index")
                            .and_then(Value::as_i64)
                            .unwrap_or_default();
                        let id = self
                            .tool_block_ids
                            .get(&block_index)
                            .cloned()
                            .unwrap_or_else(|| block_index.to_string());
                        let name = self
                            .tool_names
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| "tool".to_owned());
                        (self.emit_tool(id, name, Some(partial)), 0)
                    }
                    _ => (String::new(), 0),
                }
            }
            Some("message_delta") => {
                let usage = event
                    .get("usage")
                    .map(|usage| responses_usage_from_anthropic(Some(usage)));
                let tokens = usage
                    .map(|usage| self.record_usage(usage))
                    .unwrap_or_default();
                (String::new(), tokens)
            }
            Some("message_stop") => (self.emit_done(None), 0),
            _ => (String::new(), 0),
        }
    }

    fn translate_gemini(&mut self, event: Value) -> (String, i64) {
        let mut output = String::new();
        let parts = event
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                output.push_str(&self.emit_text(text));
            }
            if let Some(call) = part.get("functionCall") {
                output.push_str(
                    &self.emit_tool(
                        format!("call_{}", uuid::Uuid::new_v4()),
                        call.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_owned(),
                        call.get("args").map(Value::to_string),
                    ),
                );
            }
        }
        let usage = event
            .get("usageMetadata")
            .map(|usage| responses_usage_from_gemini(Some(usage)));
        let tokens = usage
            .as_ref()
            .map(|usage| self.record_usage(usage.clone()))
            .unwrap_or_default();
        let done = event
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("finishReason"))
            .is_some();
        if done {
            output.push_str(&self.emit_done(usage));
        }
        (output, tokens)
    }
}

fn translate_ollama_event(state: &mut ProviderSseState, event: Value) -> (String, i64) {
    let mut output = String::new();
    if let Some(content) = event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
    {
        output.push_str(&state.emit_text(content));
    }
    if let Some(calls) = event
        .get("message")
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            let function = call
                .get("function")
                .cloned()
                .unwrap_or_else(|| call.clone());
            output.push_str(
                &state.emit_tool(
                    format!("call_{}", uuid::Uuid::new_v4()),
                    function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                    function.get("arguments").map(Value::to_string),
                ),
            );
        }
    }
    let usage = responses_usage_from_ollama(&event);
    let tokens = if event.get("done").and_then(Value::as_bool).unwrap_or(false) {
        let tokens = state.record_usage(usage.clone());
        output.push_str(&state.emit_done(Some(usage)));
        tokens
    } else {
        0
    };
    (output, tokens)
}

fn adapt_provider_sse_stream(
    response: reqwest::Response,
    provider: GatewayProvider,
    kind: GatewayRequestKind,
    model: String,
    repository: Arc<Repository>,
) -> Response {
    let stream = response
        .bytes_stream()
        .map_err(std::io::Error::other)
        .map(Some)
        .chain(futures_util::stream::once(async { None }))
        .scan(
            (String::new(), ProviderSseState::new(provider, kind, model)),
            move |(buffer, state), chunk| {
                let repository = repository.clone();
                let result = match chunk {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        let events = drain_sse_events(buffer);
                        let mut output = String::new();
                        for event in events {
                            let (translated, _tokens) = state.translate(event);
                            output.push_str(&translated);
                        }
                        Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                    }
                    Some(Err(_)) | None => {
                        if state.done {
                            return futures_util::future::ready(None);
                        }
                        record_gateway_upstream_error(
                            &repository,
                            GATEWAY_ERROR_STREAM_INTERRUPTED,
                        );
                        state.done = true;
                        Some(Ok(Bytes::from(sse_upstream_error_event(Some(kind)))))
                    }
                };
                futures_util::future::ready(result)
            },
        );
    sse_response(Body::from_stream(stream))
}

fn drain_json_lines(buffer: &mut String) -> Vec<Value> {
    let normalized = buffer.replace("\r\n", "\n");
    *buffer = normalized;
    let Some(last_newline) = buffer.rfind('\n') else {
        return Vec::new();
    };
    let complete = buffer[..=last_newline].to_owned();
    buffer.drain(..=last_newline);
    complete
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect()
}

fn adapt_ollama_stream(
    response: reqwest::Response,
    kind: GatewayRequestKind,
    model: String,
    repository: Arc<Repository>,
) -> Response {
    let stream = response
        .bytes_stream()
        .map_err(std::io::Error::other)
        .map(Some)
        .chain(futures_util::stream::once(async { None }))
        .scan(
            (
                String::new(),
                ProviderSseState::new(GatewayProvider::Ollama, kind, model),
            ),
            move |(buffer, state), chunk| {
                let repository = repository.clone();
                let result = match chunk {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        let events = drain_json_lines(buffer);
                        let mut output = String::new();
                        for event in events {
                            let (translated, _tokens) = translate_ollama_event(state, event);
                            output.push_str(&translated);
                        }
                        Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                    }
                    Some(Err(_)) | None => {
                        if state.done {
                            return futures_util::future::ready(None);
                        }
                        record_gateway_upstream_error(
                            &repository,
                            GATEWAY_ERROR_STREAM_INTERRUPTED,
                        );
                        state.done = true;
                        Some(Ok(Bytes::from(sse_upstream_error_event(Some(kind)))))
                    }
                };
                futures_util::future::ready(result)
            },
        );
    sse_response(Body::from_stream(stream))
}

fn upstream_response(
    response: reqwest::Response,
    repository: Arc<Repository>,
    kind: Option<GatewayRequestKind>,
) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    if is_sse {
        let stream = stream
            .map(Some)
            .chain(futures_util::stream::once(async { None }))
            .scan(StreamTerminalState::default(), move |terminal, chunk| {
                let output = match chunk {
                    Some(Ok(bytes)) => {
                        terminal.observe_sse(&bytes);
                        Some(Ok::<Bytes, std::io::Error>(bytes))
                    }
                    Some(Err(_)) | None => {
                        let interruption = terminal.interruption(kind);
                        if interruption.is_some() {
                            record_gateway_upstream_error(
                                &repository,
                                GATEWAY_ERROR_STREAM_INTERRUPTED,
                            );
                        }
                        interruption.map(Ok)
                    }
                };
                futures_util::future::ready(output)
            });
        let mut output = Response::new(Body::from_stream(stream));
        *output.status_mut() = status;
        if let Some(content_type) = content_type {
            output
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
        return output;
    }
    let mut output = Response::new(Body::from_stream(stream));
    *output.status_mut() = status;
    if let Some(content_type) = content_type {
        output
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    output
}

async fn upstream_response_with_visible_model(
    response: reqwest::Response,
    repository: Arc<Repository>,
    kind: Option<GatewayRequestKind>,
    visible_model: String,
) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    if is_sse {
        return upstream_response(response, repository, kind);
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let mut output = Response::new(Body::from(bytes.clone()));
    *output.status_mut() = status;
    if let Some(content_type) = content_type {
        output
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.clone());
        if content_type
            .to_str()
            .ok()
            .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
        {
            if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
                let _ = repository.add_estimated_tokens(usage_tokens_from_value(&value));
                rewrite_model_fields(&mut value, &visible_model);
                return (status, Json(value)).into_response();
            }
        }
    }
    output
}

fn rewrite_model_fields(value: &mut Value, visible_model: &str) {
    match value {
        Value::Object(object) => {
            if object.contains_key("model") {
                object.insert("model".to_owned(), Value::String(visible_model.to_owned()));
            }
            for value in object.values_mut() {
                rewrite_model_fields(value, visible_model);
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_model_fields(value, visible_model);
            }
        }
        _ => {}
    }
}

fn gateway_http_client(proxy: &GatewayUpstreamProxy) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(GATEWAY_CONNECT_TIMEOUT)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .pool_max_idle_per_host(16)
        .tcp_keepalive_interval(Some(Duration::from_secs(20)));
    match proxy {
        GatewayUpstreamProxy::System => {}
        GatewayUpstreamProxy::Manual { url } => {
            let proxy = Proxy::all(url)?.no_proxy(NoProxy::from_string(
                "localhost,127.0.0.1,127.0.0.0/8,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,fc00::/7,fe80::/10,.local",
            ));
            builder = builder.proxy(proxy);
        }
        GatewayUpstreamProxy::Disabled => {
            builder = builder.no_proxy();
        }
    }
    builder.build()
}

async fn send_with_first_response_timeout(
    request: reqwest::RequestBuilder,
) -> AppResult<reqwest::Response> {
    send_with_first_response_timeout_after(request, GATEWAY_FIRST_RESPONSE_TIMEOUT).await
}

async fn send_with_first_response_timeout_after(
    request: reqwest::RequestBuilder,
    duration: Duration,
) -> AppResult<reqwest::Response> {
    match tokio::time::timeout(duration, request.send()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) | Err(_) => Err(AppError::UpstreamUnavailable),
    }
}

pub async fn configure_gateway_upstream_proxy(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    mode: Option<&str>,
    raw_url: Option<&str>,
) -> AppResult<()> {
    let Some(mode) = mode.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    match mode {
        "system" | "disabled" => {
            secrets.delete(GATEWAY_PROXY_SECRET_REF).await?;
            repository.set_setting(GATEWAY_PROXY_MODE_SETTING, mode)?;
            repository.delete_setting(GATEWAY_PROXY_DISPLAY_SETTING)?;
            clear_gateway_upstream_error(repository);
            Ok(())
        }
        "manual" => {
            if let Some(raw_url) = raw_url.map(str::trim).filter(|value| !value.is_empty()) {
                let validated = validate_manual_proxy_url(raw_url)?;
                secrets.set(GATEWAY_PROXY_SECRET_REF, &validated).await?;
                repository
                    .set_setting(GATEWAY_PROXY_DISPLAY_SETTING, &mask_proxy_url(&validated))?;
            } else {
                match secrets.get(GATEWAY_PROXY_SECRET_REF).await {
                    Ok(_) => {}
                    Err(AppError::NotFound) => return Err(AppError::ValidationFailed),
                    Err(error) => return Err(error),
                }
            }
            repository.set_setting(GATEWAY_PROXY_MODE_SETTING, "manual")?;
            clear_gateway_upstream_error(repository);
            Ok(())
        }
        _ => Err(AppError::ValidationFailed),
    }
}

async fn load_gateway_upstream_proxy(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
) -> AppResult<GatewayUpstreamProxy> {
    match repository
        .setting(GATEWAY_PROXY_MODE_SETTING)?
        .unwrap_or_else(|| "system".to_owned())
        .as_str()
    {
        "system" => Ok(GatewayUpstreamProxy::System),
        "disabled" => Ok(GatewayUpstreamProxy::Disabled),
        "manual" => {
            let url = secrets.get(GATEWAY_PROXY_SECRET_REF).await?;
            let url = validate_manual_proxy_url(&url)?;
            Ok(GatewayUpstreamProxy::Manual { url })
        }
        _ => Err(AppError::ValidationFailed),
    }
}

fn validate_manual_proxy_url(raw: &str) -> AppResult<String> {
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed).map_err(|_| AppError::ValidationFailed)?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) || parsed.host_str().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(AppError::ValidationFailed);
    }
    Ok(parsed.to_string())
}

fn mask_proxy_url(raw: &str) -> String {
    let Ok(parsed) = Url::parse(raw) else {
        return "invalid-proxy-url".to_owned();
    };
    let Some(host) = parsed.host_str() else {
        return format!("{}://<host>", parsed.scheme());
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let credentials = if parsed.username().is_empty() && parsed.password().is_none() {
        ""
    } else {
        "***@"
    };
    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("{}://{}{}{}", parsed.scheme(), credentials, host, port)
}

fn record_gateway_upstream_error(repository: &Repository, category: &str) {
    let _ = repository.set_setting(GATEWAY_LAST_ERROR_SETTING, category);
}

fn clear_gateway_upstream_error(repository: &Repository) {
    let _ = repository.delete_setting(GATEWAY_LAST_ERROR_SETTING);
}

fn sse_upstream_error_event(kind: Option<GatewayRequestKind>) -> &'static str {
    match kind {
        Some(GatewayRequestKind::Responses) => {
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_gateway_error\",\"status\":\"failed\"},\"error\":{\"code\":\"upstream_stream_error\",\"message\":\"upstream stream interrupted\"}}\n\n"
        }
        _ => {
            "data: {\"error\":{\"code\":\"upstream_stream_error\",\"message\":\"upstream stream interrupted\"}}\n\ndata: [DONE]\n\n"
        }
    }
}

fn build_upstream_url(base_url: &str, route: &str) -> AppResult<Url> {
    let base = Url::parse(base_url).map_err(|_| AppError::ValidationFailed)?;
    if !matches!(base.scheme(), "https" | "http") || base.host_str().is_none() {
        return Err(AppError::ForbiddenNetworkTarget);
    }
    Url::parse(&format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        route.trim_start_matches('/')
    ))
    .map_err(|_| AppError::ValidationFailed)
}

fn upstream_request(
    request: reqwest::RequestBuilder,
    provider: &GatewayProvider,
    key: &str,
    payload: &Value,
) -> reqwest::RequestBuilder {
    match provider {
        GatewayProvider::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(payload),
        GatewayProvider::Gemini => request.header("x-goog-api-key", key).json(payload),
        _ => request.bearer_auth(key).json(payload),
    }
}

fn upstream_request_bytes(
    request: reqwest::RequestBuilder,
    provider: &GatewayProvider,
    key: &str,
    payload: Bytes,
) -> reqwest::RequestBuilder {
    let request = request.header(header::CONTENT_TYPE, "application/json");
    match provider {
        GatewayProvider::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .body(payload),
        GatewayProvider::Gemini => request.header("x-goog-api-key", key).body(payload),
        _ => request.bearer_auth(key).body(payload),
    }
}

fn token_digest(value: &str) -> TokenDigest {
    Sha256::digest(value.as_bytes()).into()
}

fn authorize(
    state: &GatewayApiState,
    peer: IpAddr,
    headers: &HeaderMap,
) -> Option<AuthorizedClient> {
    let started = Instant::now();
    if !state.cidrs.is_empty() && !state.cidrs.iter().any(|network| network.contains(&peer)) {
        return None;
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let value = bearer
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        })
        .or_else(|| {
            headers
                .get("x-goog-api-key")
                .and_then(|value| value.to_str().ok())
        })?;
    let digest = token_digest(value);

    if bearer.is_some() {
        state
            .auth_cache
            .refresh_oauth_if_due(&state.repository, &state.codex_home);
        if state.auth_cache.oauth_matches(&digest) {
            return Some(AuthorizedClient {
                codex_managed: true,
                auth_mode: "oauth",
                auth_latency_ms: started.elapsed().as_millis() as i64,
                request_started: started,
            });
        }
    }
    if let Some(mut cached) = state.auth_cache.cached_client(&digest) {
        cached.auth_latency_ms = started.elapsed().as_millis() as i64;
        cached.request_started = started;
        return Some(cached);
    }
    if state.auth_cache.recently_rejected(&digest) {
        return None;
    }

    let keys = state.repository.valid_key_hashes().ok()?;
    let codex_key_ref = state
        .repository
        .setting(GATEWAY_CODEX_CLIENT_KEY_REF_SETTING)
        .ok()
        .flatten();
    for (id, hash, secret_ref) in keys {
        if PasswordHash::new(&hash).ok().is_some_and(|parsed| {
            #[cfg(test)]
            ARGON2_VERIFY_CALLS.fetch_add(1, Ordering::Relaxed);
            Argon2::default()
                .verify_password(value.as_bytes(), &parsed)
                .is_ok()
        }) {
            let _ = state.repository.record_key_use(&id, timestamp_ms());
            let client = AuthorizedClient {
                codex_managed: codex_key_ref.as_deref() == Some(secret_ref.as_str()),
                auth_mode: "client_key",
                auth_latency_ms: started.elapsed().as_millis() as i64,
                request_started: started,
            };
            state.auth_cache.cache_client(digest, client.clone());
            return Some(client);
        }
    }
    state.auth_cache.cache_rejected(digest);
    None
}

fn direct_oauth_auth_snapshot(
    repository: &Repository,
    codex_home: &std::path::Path,
) -> Option<DirectOAuthAuthSnapshot> {
    let direct_profile_id = repository
        .setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)
        .ok()
        .flatten()?;
    let direct_profile = repository.profile(&direct_profile_id).ok()?;
    if direct_profile.profile.kind != ProfileKind::ApiKey
        || !matches!(
            direct_profile.profile.provider,
            GatewayProvider::OpenAi | GatewayProvider::OpenAiCompatible
        )
        || direct_profile.profile.wire_api != GatewayWireApi::Responses
        || !direct_profile.profile.enabled
        || !direct_profile.profile.credential_configured
        || direct_profile.secret_ref.is_none()
        || direct_profile.profile.health != "healthy"
        || direct_profile.profile.validation_status == "invalid"
    {
        return None;
    }
    let oauth_profile_id = direct_profile.profile.codex_oauth_profile_id.as_deref()?;
    let oauth_profile = repository.profile(oauth_profile_id).ok()?;
    if oauth_profile.profile.kind != ProfileKind::CodexOauth
        || oauth_profile.profile.auth_mode != CodexAuthMode::OAuth
        || oauth_profile.credential_fingerprint.is_some()
        || !oauth_profile.profile.enabled
        || !oauth_profile.profile.credential_configured
        || oauth_profile.profile.health != "healthy"
        || oauth_profile.profile.validation_status == "invalid"
    {
        return None;
    }
    let expected_account_id = oauth_profile
        .profile
        .account
        .as_ref()
        .and_then(|account| account.account_id.as_deref())?;
    let auth_json = std::fs::read_to_string(codex_home.join("auth.json")).ok()?;
    let auth = serde_json::from_str::<Value>(&auth_json).ok()?;
    let tokens = auth.get("tokens")?;
    let access_token = tokens.get("access_token").and_then(Value::as_str)?;
    let account_id = tokens.get("account_id").and_then(Value::as_str)?;
    if account_id != expected_account_id {
        return None;
    }
    Some(DirectOAuthAuthSnapshot {
        access_token_digest: token_digest(access_token),
    })
}

fn direct_profile_for_codex_client(
    state: &GatewayApiState,
    client: &AuthorizedClient,
    model: Option<&str>,
) -> AppResult<Option<StoredProfile>> {
    if !client.codex_managed {
        return Ok(None);
    }
    let Some(profile_id) = state
        .repository
        .setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING)?
    else {
        return Ok(None);
    };
    let stored = state.repository.profile(&profile_id)?;
    let profile = &stored.profile;
    let now = timestamp_ms();
    if profile.kind != ProfileKind::ApiKey
        || !profile.enabled
        || !profile.credential_configured
        || profile.health != "healthy"
        || profile.cooldown_until_ms.is_some_and(|until| until > now)
    {
        return Err(AppError::UpstreamUnavailable);
    }
    if model.is_some_and(|model| !profile.models.iter().any(|candidate| candidate == model)) {
        return Err(AppError::GatewayModelUnavailable);
    }
    Ok(Some(stored))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::SocketAddr,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::{Duration, Instant},
    };

    use url::Url;

    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };

    use axum::{
        body::{to_bytes, Body, Bytes},
        extract::State,
        http::{header, HeaderMap, HeaderValue, StatusCode},
        response::Response,
        routing::post,
        Json, Router,
    };
    use futures_util::future::join_all;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        adapt_provider_chat_fanout_response, anthropic_chat_payload, authorize, build_upstream_url,
        chat_choice_count, chat_to_responses, completed_response, direct_profile_for_codex_client,
        forward, gateway_http_client, gateway_router, gemini_chat_payload, mask_proxy_url,
        model_discovery_route, model_ids_from_provider_response, ollama_chat_payload_from_openai,
        payload_with_model, provider_usage_tokens, provider_value_to_chat,
        provider_value_to_response, response_to_chat_completion, responses_to_chat_completion,
        rewrite_model_fields, send_oauth_request, send_with_first_response_timeout_after,
        test_api_service, translate_ollama_event, upstream_model_for, upstream_response,
        validate_binding, validate_manual_proxy_url, visible_model_for, AuthorizedClient,
        GatewayApiState, GatewayRequestKind, GatewayUpstreamProxy, ProviderSseState,
        WeightedScheduler, CODEX_RESPONSES_URL, GATEWAY_JSON_BODY_LIMIT_BYTES,
    };
    use crate::error::AppError;
    use crate::{
        database::{Repository, StoredProfile},
        domain::{
            GatewayModelMapping, GatewayProvider, GatewayWireApi, MaskedClientKey, MaskedProfile,
            ProfileKind, GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING,
        },
        oauth_credentials::OAuthCredentialStore,
        profiles::{save_oauth_credential_metadata, CodexOAuthCredential},
        secrets::{MemorySecretStore, SecretStore},
    };

    fn candidate(id: &str, weight: i64) -> StoredProfile {
        StoredProfile {
            profile: MaskedProfile {
                id: id.into(),
                alias: id.into(),
                kind: ProfileKind::CodexOauth,
                base_url: None,
                provider: GatewayProvider::OpenAi,
                wire_api: GatewayWireApi::Responses,
                enabled: true,
                in_pool: true,
                priority: 0,
                weight,
                models: vec!["gpt-test".into()],
                model_mappings: Vec::new(),
                health: "healthy".into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
                max_concurrency: 4,
                max_queue_depth: 8,
                queue_timeout_ms: 15_000,
            },
            secret_ref: Some(format!("profile:{id}:oauth")),
            credential_fingerprint: None,
        }
    }

    fn direct_oauth_gateway_state(
        repository: Arc<Repository>,
        secrets: Arc<MemorySecretStore>,
        codex_home: PathBuf,
    ) -> GatewayApiState {
        GatewayApiState {
            repository,
            secrets: secrets.clone(),
            oauth_credentials: Arc::new(OAuthCredentialStore::new(secrets)),
            cidrs: Vec::new(),
            oauth_responses_url: Url::parse(CODEX_RESPONSES_URL).unwrap(),
            http_client: gateway_http_client(&GatewayUpstreamProxy::Disabled).unwrap(),
            auth_cache: Arc::new(super::GatewayAuthCache::new()),
            concurrency: super::ProfileConcurrencyManager::new(),
            telemetry: super::GatewayTelemetry::new(),
            certificate_ready: true,
            codex_home,
            scheduler: Arc::new(Mutex::new(WeightedScheduler::default())),
            affinities: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn install_direct_oauth_profiles(
        repository: &Repository,
        secrets: &MemorySecretStore,
        base_url: &str,
    ) {
        let mut direct = candidate("direct-api", 1);
        direct.profile.kind = ProfileKind::ApiKey;
        direct.profile.provider = GatewayProvider::OpenAiCompatible;
        direct.profile.base_url = Some(base_url.to_owned());
        direct.profile.models = vec!["direct-model".to_owned()];
        direct.profile.codex_oauth_profile_id = Some("oauth-login".to_owned());
        direct.secret_ref = Some("profile:direct-api:credential".to_owned());
        repository.insert_profile(&direct).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "direct-api")
            .unwrap();

        let oauth = candidate("oauth-login", 1);
        repository.insert_profile(&oauth).unwrap();
        save_oauth_credential_metadata(
            repository,
            "oauth-login",
            &CodexOAuthCredential {
                id_token: "id-token".to_owned(),
                access_token: "oauth-sentinel".to_owned(),
                refresh_token: Some("refresh-token".to_owned()),
                account_id: Some("account-a".to_owned()),
                last_refresh_ms: 1,
            },
        )
        .unwrap();
        secrets
            .set("profile:direct-api:credential", "zeron-key-sentinel")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn direct_oauth_authorization_requires_projected_token_and_bound_account() {
        let _counter_guard = super::AUTH_COUNTER_TEST_LOCK.lock().await;
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        install_direct_oauth_profiles(&repository, &secrets, "https://api.example.com/v1").await;
        let codex_home =
            std::env::temp_dir().join(format!("codex-relay-oauth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-sentinel","account_id":"account-a"}}"#,
        )
        .unwrap();
        let state = direct_oauth_gateway_state(repository.clone(), secrets, codex_home.clone());
        let peer = "127.0.0.1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer oauth-sentinel"),
        );

        super::ARGON2_VERIFY_CALLS.store(0, Ordering::Relaxed);
        let authorized = authorize(&state, peer, &headers).unwrap();
        assert!(authorized.codex_managed);
        assert_eq!(super::ARGON2_VERIFY_CALLS.load(Ordering::Relaxed), 0);

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer another-token"),
        );
        assert!(authorize(&state, peer, &headers).is_none());

        headers.remove(header::AUTHORIZATION);
        headers.insert("x-api-key", HeaderValue::from_static("oauth-sentinel"));
        assert!(authorize(&state, peer, &headers).is_none());
        headers.remove("x-api-key");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer oauth-sentinel"),
        );
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-sentinel","account_id":"account-b"}}"#,
        )
        .unwrap();
        state.auth_cache.refresh_oauth(&repository, &codex_home);
        assert!(authorize(&state, peer, &headers).is_none());

        let mut oauth = repository.profile("oauth-login").unwrap();
        oauth.profile.enabled = false;
        repository.update_profile(&oauth).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-sentinel","account_id":"account-a"}}"#,
        )
        .unwrap();
        state.auth_cache.refresh_oauth(&repository, &codex_home);
        assert!(authorize(&state, peer, &headers).is_none());
        let _ = std::fs::remove_dir_all(codex_home);
    }

    #[tokio::test]
    async fn invalid_bearer_is_rejected_before_large_request_body_is_read() {
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        let state = direct_oauth_gateway_state(repository, secrets, std::env::temp_dir());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move {
            axum::serve(
                listener,
                gateway_router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let headers = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer invalid\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            6 * 1024 * 1024
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut response))
            .await
            .expect("401 must be returned without waiting for the declared request body")
            .unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert!(!response.contains("413 Payload Too Large"));
        relay.abort();
    }

    #[tokio::test]
    async fn oauth_models_fast_path_avoids_argon2_and_stays_below_local_p95_budget() {
        let _counter_guard = super::AUTH_COUNTER_TEST_LOCK.lock().await;
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        install_direct_oauth_profiles(&repository, &secrets, "https://api.example.com/v1").await;
        let codex_home =
            std::env::temp_dir().join(format!("codex-relay-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-sentinel","account_id":"account-a"}}"#,
        )
        .unwrap();
        let state = direct_oauth_gateway_state(repository, secrets, codex_home.clone());
        state
            .auth_cache
            .refresh_oauth(&state.repository, &codex_home);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move {
            axum::serve(
                listener,
                gateway_router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::new();
        super::ARGON2_VERIFY_CALLS.store(0, Ordering::Relaxed);
        let mut benchmark = Vec::new();
        for (concurrency, total) in [(1_usize, 20_usize), (4, 24), (8, 32)] {
            let mut latencies = Vec::with_capacity(total);
            while latencies.len() < total {
                let batch_size = concurrency.min(total - latencies.len());
                let requests = (0..batch_size).map(|_| {
                    let client = client.clone();
                    async move {
                        let started = Instant::now();
                        let response = client
                            .get(format!("http://{address}/v1/models"))
                            .bearer_auth("oauth-sentinel")
                            .send()
                            .await
                            .unwrap();
                        assert_eq!(response.status(), StatusCode::OK);
                        started.elapsed().as_micros() as i64
                    }
                });
                latencies.extend(join_all(requests).await);
            }
            latencies.sort_unstable();
            let p50_us = latencies[(latencies.len() - 1) * 50 / 100];
            let p95_us = latencies[(latencies.len() - 1) * 95 / 100];
            let p99_us = latencies[(latencies.len() - 1) * 99 / 100];
            benchmark.push((concurrency, p50_us, p95_us, p99_us));
        }

        assert_eq!(super::ARGON2_VERIFY_CALLS.load(Ordering::Relaxed), 0);
        for (concurrency, p50_us, p95_us, p99_us) in benchmark {
            println!(
                "oauth_models c{concurrency} p50={:.2}ms p95={:.2}ms p99={:.2}ms",
                p50_us as f64 / 1_000.0,
                p95_us as f64 / 1_000.0,
                p99_us as f64 / 1_000.0,
            );
            assert!(
                p95_us < 200_000,
                "local OAuth /v1/models c{concurrency} p95 was {p95_us} us"
            );
        }
        relay.abort();
        let _ = std::fs::remove_dir_all(codex_home);
    }

    #[tokio::test]
    async fn client_key_argon2_verification_is_cached_after_first_success() {
        let _counter_guard = super::AUTH_COUNTER_TEST_LOCK.lock().await;
        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        let plaintext = "relay_test_client_key";
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(plaintext.as_bytes(), &salt)
            .unwrap()
            .to_string();
        repository
            .insert_client_key(
                &MaskedClientKey {
                    id: "key-a".to_owned(),
                    name: "测试 Key".to_owned(),
                    masked_value: "relay_****_key".to_owned(),
                    created_at_ms: 1,
                    last_used_at_ms: None,
                    revoked: false,
                    managed_by: "user".to_owned(),
                    can_revoke: true,
                },
                &hash,
                "client-key:key-a",
            )
            .unwrap();
        let state = direct_oauth_gateway_state(repository, secrets, std::env::temp_dir());
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static(plaintext));
        let peer = "127.0.0.1".parse().unwrap();

        super::ARGON2_VERIFY_CALLS.store(0, Ordering::Relaxed);
        assert!(authorize(&state, peer, &headers).is_some());
        assert_eq!(super::ARGON2_VERIFY_CALLS.load(Ordering::Relaxed), 1);
        assert!(authorize(&state, peer, &headers).is_some());
        assert_eq!(super::ARGON2_VERIFY_CALLS.load(Ordering::Relaxed), 1);

        state.auth_cache.invalidate_client_keys();
        assert!(authorize(&state, peer, &headers).is_some());
        assert_eq!(super::ARGON2_VERIFY_CALLS.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn responses_stream_terminal_state_emits_exactly_one_failure_without_done() {
        let mut incomplete = super::StreamTerminalState::default();
        let failure = incomplete
            .interruption(Some(GatewayRequestKind::Responses))
            .unwrap();
        let failure = String::from_utf8_lossy(&failure);
        assert!(failure.contains("response.failed"));
        assert!(!failure.contains("[DONE]"));
        assert!(incomplete
            .interruption(Some(GatewayRequestKind::Responses))
            .is_none());

        let mut completed = super::StreamTerminalState::default();
        completed.observe_sse(&Bytes::from_static(
            b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
        ));
        assert!(completed
            .interruption(Some(GatewayRequestKind::Responses))
            .is_none());
    }

    #[tokio::test]
    async fn profile_concurrency_gate_caps_active_and_rejects_queue_overflow() {
        let manager = super::ProfileConcurrencyManager::new();
        let mut profile = candidate("limited", 1).profile;
        profile.max_concurrency = 4;
        profile.max_queue_depth = 8;
        profile.queue_timeout_ms = 2_000;
        let mut active = Vec::new();
        for _ in 0..4 {
            active.push(manager.acquire(&profile).await.unwrap().0);
        }
        let mut queued = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            let profile = profile.clone();
            queued.push(tokio::spawn(async move { manager.acquire(&profile).await }));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(manager.counts(), (4, 8));
        assert!(matches!(
            manager.acquire(&profile).await,
            Err(super::ProfileAcquireError::QueueFull)
        ));
        let cancelled = queued.remove(0);
        cancelled.abort();
        let _ = cancelled.await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(manager.counts(), (4, 7));
        drop(active);
        for task in queued {
            let (permit, _) = task.await.unwrap().unwrap();
            drop(permit);
        }
        assert_eq!(manager.counts(), (0, 0));
    }

    #[tokio::test]
    async fn direct_oauth_request_is_forwarded_with_third_party_api_key() {
        async fn echo(
            headers: HeaderMap,
            Json(payload): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "id": "resp-direct",
                "object": "response",
                "status": "completed",
                "model": payload.get("model").cloned().unwrap_or_default(),
                "authorization": headers
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                "output": []
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/v1/responses", post(echo)))
                .await
                .unwrap();
        });

        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        install_direct_oauth_profiles(&repository, &secrets, &format!("http://{address}/v1")).await;
        let codex_home =
            std::env::temp_dir().join(format!("codex-relay-forward-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-sentinel","account_id":"account-a"}}"#,
        )
        .unwrap();
        let state = direct_oauth_gateway_state(repository, secrets, codex_home.clone());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer oauth-sentinel"),
        );

        let authorized_client = authorize(&state, "127.0.0.1".parse().unwrap(), &headers).unwrap();
        let payload = serde_json::json!({
            "model": "direct-model",
            "input": "hello",
            "stream": false
        });
        let request_bytes = serde_json::to_vec(&payload).unwrap().len() as i64;
        let response = forward(
            state,
            authorized_client,
            payload,
            request_bytes,
            "responses",
            GatewayProvider::OpenAiCompatible,
            Some(GatewayRequestKind::Responses),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["authorization"], "Bearer zeron-key-sentinel");
        assert!(!body.to_string().contains("oauth-sentinel"));

        server.abort();
        let _ = std::fs::remove_dir_all(codex_home);
    }

    #[tokio::test]
    async fn gateway_accepts_codex_json_payloads_larger_than_axum_default() {
        async fn echo(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
            let content = payload
                .get("input")
                .and_then(serde_json::Value::as_array)
                .and_then(|input| input.first())
                .and_then(|message| message.get("content"))
                .and_then(serde_json::Value::as_array);
            let image_count = content
                .map(|content| {
                    content
                        .iter()
                        .filter(|item| {
                            item.get("type").and_then(serde_json::Value::as_str)
                                == Some("input_image")
                        })
                        .count()
                })
                .unwrap_or_default();
            let image_url_bytes = content
                .map(|content| {
                    content
                        .iter()
                        .filter_map(|item| {
                            item.get("image_url").and_then(serde_json::Value::as_str)
                        })
                        .map(str::len)
                        .sum::<usize>()
                })
                .unwrap_or_default();
            Json(serde_json::json!({
                "id": "resp-large",
                "object": "response",
                "status": "completed",
                "model": payload.get("model").cloned().unwrap_or_default(),
                "image_count": image_count,
                "image_url_bytes": image_url_bytes,
                "output": []
            }))
        }

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let app = Router::new().route("/v1/responses", post(echo)).layer(
                axum::extract::DefaultBodyLimit::max(GATEWAY_JSON_BODY_LIMIT_BYTES),
            );
            axum::serve(upstream_listener, app).await.unwrap();
        });

        let repository = Arc::new(Repository::memory());
        let secrets = Arc::new(MemorySecretStore::new());
        install_direct_oauth_profiles(
            &repository,
            &secrets,
            &format!("http://{upstream_address}/v1"),
        )
        .await;
        let codex_home =
            std::env::temp_dir().join(format!("codex-relay-large-body-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-sentinel","account_id":"account-a"}}"#,
        )
        .unwrap();
        let state = direct_oauth_gateway_state(repository, secrets, codex_home.clone());

        let relay_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = relay_listener.local_addr().unwrap();
        let relay = tokio::spawn(async move {
            axum::serve(
                relay_listener,
                gateway_router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let image_url = format!("data:image/png;base64,{}", "A".repeat(896 * 1024));
        let content = (0..6)
            .map(|_| {
                serde_json::json!({
                    "type": "input_image",
                    "detail": "high",
                    "image_url": image_url
                })
            })
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "model": "direct-model",
            "input": [{
                "role": "user",
                "content": content
            }],
            "stream": false
        });
        let serialized_bytes = serde_json::to_vec(&payload).unwrap().len();
        assert!(serialized_bytes > 5 * 1024 * 1024);
        assert!(serialized_bytes < GATEWAY_JSON_BODY_LIMIT_BYTES);

        let response = reqwest::Client::new()
            .post(format!("http://{relay_address}/v1/responses"))
            .bearer_auth("oauth-sentinel")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["image_count"], 6);
        assert!(body["image_url_bytes"].as_u64().unwrap() > 5 * 1024 * 1024);

        relay.abort();
        upstream.abort();
        let _ = std::fs::remove_dir_all(codex_home);
    }

    #[test]
    fn codex_managed_client_uses_direct_profile_without_affecting_user_keys() {
        let repository = Arc::new(Repository::memory());
        let mut direct = candidate("direct-api", 1);
        direct.profile.kind = ProfileKind::ApiKey;
        direct.profile.provider = GatewayProvider::OpenAiCompatible;
        direct.profile.models = vec!["direct-model".to_owned()];
        direct.secret_ref = Some("profile:direct-api:credential".to_owned());
        repository.insert_profile(&direct).unwrap();
        let mut pooled = candidate("pooled-api", 1);
        pooled.profile.kind = ProfileKind::ApiKey;
        pooled.profile.provider = GatewayProvider::OpenAiCompatible;
        pooled.profile.models = vec!["pool-model".to_owned()];
        pooled.secret_ref = Some("profile:pooled-api:credential".to_owned());
        repository.insert_profile(&pooled).unwrap();
        repository
            .set_setting(GATEWAY_CODEX_DIRECT_PROFILE_ID_SETTING, "direct-api")
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let state = direct_oauth_gateway_state(repository.clone(), secrets, std::env::temp_dir());
        let codex_client = AuthorizedClient {
            codex_managed: true,
            auth_mode: "client_key",
            auth_latency_ms: 0,
            request_started: Instant::now(),
        };
        let user_client = AuthorizedClient {
            codex_managed: false,
            auth_mode: "client_key",
            auth_latency_ms: 0,
            request_started: Instant::now(),
        };

        let selected = direct_profile_for_codex_client(&state, &codex_client, Some("direct-model"))
            .unwrap()
            .unwrap();
        assert_eq!(selected.profile.id, "direct-api");
        assert!(matches!(
            direct_profile_for_codex_client(&state, &codex_client, Some("pool-model")),
            Err(AppError::GatewayModelUnavailable)
        ));
        assert!(
            direct_profile_for_codex_client(&state, &user_client, Some("pool-model"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn allows_loopback_binding_without_cidrs() {
        assert!(validate_binding("loopback", "127.0.0.1".parse().unwrap(), &[]).is_ok());
        assert!(matches!(
            validate_binding(
                "loopback",
                "127.0.0.1".parse().unwrap(),
                &["127.0.0.1/32".into()]
            ),
            Err(AppError::ForbiddenNetworkTarget)
        ));
    }

    #[test]
    fn maps_openai_chat_to_anthropic_messages_with_tools() {
        let payload = serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "system", "content": "Be concise"},
                {"role": "user", "content": [{"type":"text", "text":"Weather?"}]},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"SF\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "tools": [{"type":"function", "function": {
                "name":"weather", "description":"Read weather", "parameters":{"type":"object"}
            }}],
            "tool_choice": {"type":"function", "function":{"name":"weather"}},
            "max_tokens": 128,
            "stream": true
        });

        let mapped = anthropic_chat_payload(&payload, "claude-sonnet", true).unwrap();

        assert_eq!(mapped["model"], "claude-sonnet");
        assert_eq!(mapped["system"], "Be concise");
        assert_eq!(mapped["messages"][0]["role"], "user");
        assert_eq!(mapped["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(mapped["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(mapped["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(mapped["tool_choice"]["name"], "weather");
    }

    #[test]
    fn maps_openai_chat_to_gemini_generate_content_with_schema() {
        let payload = serde_json::json!({
            "messages": [
                {"role":"system", "content":"Answer JSON"},
                {"role":"user", "content":"Hi"}
            ],
            "tools": [{"type":"function", "function": {
                "name":"lookup", "parameters":{"type":"object"}
            }}],
            "response_format": {"type":"json_schema", "json_schema":{"schema":{"type":"object"}}},
            "temperature": 0.2
        });

        let mapped = gemini_chat_payload(&payload, false).unwrap();

        assert_eq!(
            mapped["systemInstruction"]["parts"][0]["text"],
            "Answer JSON"
        );
        assert_eq!(mapped["contents"][0]["role"], "user");
        assert_eq!(
            mapped["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
        assert_eq!(
            mapped["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(mapped["generationConfig"]["temperature"], 0.2);
    }

    #[test]
    fn maps_openai_chat_to_ollama_chat_options_and_format() {
        let payload = serde_json::json!({
            "messages": [{"role":"user", "content":"Hi"}],
            "response_format": {"type":"json_object"},
            "temperature": 0.1,
            "max_tokens": 64,
            "reasoning_effort": "high"
        });

        let mapped = ollama_chat_payload_from_openai(&payload, "qwen", true).unwrap();

        assert_eq!(mapped["model"], "qwen");
        assert_eq!(mapped["format"], "json");
        assert_eq!(mapped["options"]["temperature"], 0.1);
        assert_eq!(mapped["options"]["num_predict"], 64);
        assert_eq!(mapped["think"], "high");
    }

    #[test]
    fn converts_provider_usage_and_outputs_back_to_openai_shapes() {
        let anthropic = serde_json::json!({
            "id":"msg_1",
            "content":[
                {"type":"text", "text":"hello"},
                {"type":"tool_use", "id":"toolu_1", "name":"weather", "input":{"city":"SF"}}
            ],
            "usage":{"input_tokens":2,"output_tokens":3}
        });
        let chat = provider_value_to_chat(&GatewayProvider::Anthropic, &anthropic, "claude");
        assert_eq!(chat["choices"][0]["message"]["content"], "hello");
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            provider_usage_tokens(&GatewayProvider::Anthropic, &anthropic),
            5
        );

        let gemini = serde_json::json!({
            "candidates":[{"content":{"parts":[{"text":"hi"}]}, "finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":5,"totalTokenCount":9}
        });
        let response = provider_value_to_response(&GatewayProvider::Gemini, &gemini, "gemini");
        assert_eq!(response["output"][0]["content"][0]["text"], "hi");
        assert_eq!(response["usage"]["total_tokens"], 9);

        let ollama = serde_json::json!({
            "message":{"role":"assistant", "content":"local"},
            "prompt_eval_count":6,
            "eval_count":7
        });
        let response = provider_value_to_response(&GatewayProvider::Ollama, &ollama, "qwen");
        assert_eq!(response["output"][0]["content"][0]["text"], "local");
        assert_eq!(response["usage"]["total_tokens"], 13);
    }

    #[test]
    fn validates_chat_choice_count_for_adapter_fanout() {
        assert_eq!(chat_choice_count(&serde_json::json!({})).unwrap(), 1);
        assert_eq!(chat_choice_count(&serde_json::json!({"n": 3})).unwrap(), 3);
        assert!(chat_choice_count(&serde_json::json!({"n": 0})).is_err());
        assert!(chat_choice_count(&serde_json::json!({"n": "2"})).is_err());
    }

    #[tokio::test]
    async fn merges_provider_chat_fanout_choices_and_usage() {
        async fn anthropic_reply(
            State(counter): State<Arc<AtomicUsize>>,
        ) -> Json<serde_json::Value> {
            let index = counter.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({
                "id": format!("msg_{index}"),
                "content": [{"type": "text", "text": format!("reply {index}")}],
                "usage": {"input_tokens": 2, "output_tokens": 3}
            }))
        }

        let repository = Arc::new(Repository::memory());
        let counter = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn({
            let counter = counter.clone();
            async move {
                axum::serve(
                    listener,
                    Router::new()
                        .route("/v1/messages", post(anthropic_reply))
                        .with_state(counter),
                )
                .await
                .unwrap();
            }
        });
        let client = reqwest::Client::new();
        let first = client
            .post(format!("http://{address}/v1/messages"))
            .send()
            .await
            .unwrap();
        let second = client
            .post(format!("http://{address}/v1/messages"))
            .send()
            .await
            .unwrap();

        let response = adapt_provider_chat_fanout_response(
            vec![first, second],
            GatewayProvider::Anthropic,
            "claude-visible".to_owned(),
            repository.clone(),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        server.abort();
        let value = serde_json::from_slice::<serde_json::Value>(&body).unwrap();

        assert!(status.is_success());
        assert_eq!(value["model"], "claude-visible");
        assert_eq!(value["choices"][0]["index"], 0);
        assert_eq!(value["choices"][1]["index"], 1);
        assert_eq!(value["usage"]["total_tokens"], 10);
        // The outer request instrumentation owns aggregate accounting exactly once.
        assert_eq!(repository.metrics().unwrap().estimated_tokens, 0);
    }

    #[test]
    fn provider_stream_state_preserves_choice_and_tool_ids() {
        let mut anthropic = ProviderSseState::new_choice(
            GatewayProvider::Anthropic,
            GatewayRequestKind::ChatCompletions,
            "claude-visible".to_owned(),
            2,
            false,
        );
        let (started, _) = anthropic.translate(serde_json::json!({
            "type": "content_block_start",
            "index": 4,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "weather"}
        }));
        let (arguments, _) = anthropic.translate(serde_json::json!({
            "type": "content_block_delta",
            "index": 4,
            "delta": {"type": "input_json_delta", "partial_json": "{\"city\":\"SF\"}"}
        }));
        assert!(started.contains("\"index\":2"));
        assert!(started.contains("\"id\":\"toolu_1\""));
        assert!(arguments.contains("\"index\":2"));
        assert!(arguments.contains("toolu_1"));
        assert!(arguments.contains("weather"));

        let mut ollama = ProviderSseState::new_choice(
            GatewayProvider::Ollama,
            GatewayRequestKind::ChatCompletions,
            "qwen-visible".to_owned(),
            1,
            false,
        );
        let (delta, _) = translate_ollama_event(
            &mut ollama,
            serde_json::json!({"message": {"content": "local"}}),
        );
        let (done, tokens) = translate_ollama_event(
            &mut ollama,
            serde_json::json!({"done": true, "prompt_eval_count": 6, "eval_count": 7}),
        );
        assert!(delta.contains("\"index\":1"));
        assert_eq!(tokens, 13);
        assert!(!done.contains("[DONE]"));
    }

    #[test]
    fn refuses_public_and_wildcard_bindings() {
        assert!(matches!(
            validate_binding(
                "lan",
                "8.8.8.8".parse().unwrap(),
                &["192.168.1.0/24".into()]
            ),
            Err(AppError::ForbiddenNetworkTarget)
        ));
        assert!(matches!(
            validate_binding(
                "lan",
                "0.0.0.0".parse().unwrap(),
                &["192.168.1.0/24".into()]
            ),
            Err(AppError::ForbiddenNetworkTarget)
        ));
    }

    #[test]
    fn allows_an_unrestricted_private_lan_binding() {
        assert!(validate_binding("lan", "192.168.1.12".parse().unwrap(), &[]).is_ok());
    }

    #[test]
    fn discovers_native_model_identifiers_without_guessing() {
        let profile = StoredProfile {
            profile: MaskedProfile {
                id: "profile".into(),
                alias: "Gemini".into(),
                kind: ProfileKind::ApiKey,
                base_url: Some("https://example.test".into()),
                provider: GatewayProvider::Gemini,
                wire_api: GatewayWireApi::Responses,
                enabled: true,
                in_pool: true,
                priority: 0,
                weight: 1,
                models: Vec::new(),
                model_mappings: Vec::new(),
                health: "unknown".into(),
                cooldown_until_ms: None,
                credential_configured: true,
                auth_mode: Default::default(),
                codex_oauth_profile_id: None,
                is_current: false,
                account: None,
                validation_status: "unknown".to_owned(),
                validated_at_ms: None,
                validation_message: None,
                max_concurrency: 4,
                max_queue_depth: 8,
                queue_timeout_ms: 15_000,
            },
            secret_ref: None,
            credential_fingerprint: None,
        };
        let body = serde_json::json!({"models": [{"name": "models/gemini-2.5-pro"}]});
        assert_eq!(
            model_ids_from_provider_response(&profile.profile.provider, &body),
            vec!["gemini-2.5-pro"]
        );
        let _ = Repository::memory();
    }

    #[test]
    fn discovers_openai_compatible_string_and_models_arrays() {
        let data_strings = serde_json::json!({"data": ["coder-small", "coder-large"]});
        assert_eq!(
            model_ids_from_provider_response(&GatewayProvider::OpenAiCompatible, &data_strings),
            vec!["coder-large", "coder-small"]
        );

        let models_objects = serde_json::json!({
            "models": [
                "provider-string",
                {"name": "provider-name"},
                {"id": "provider-id"}
            ]
        });
        assert_eq!(
            model_ids_from_provider_response(&GatewayProvider::OpenAiCompatible, &models_objects),
            vec!["provider-id", "provider-name", "provider-string"]
        );
    }

    #[tokio::test]
    async fn test_api_service_returns_a_structured_report_for_invalid_json() {
        async fn invalid_json() -> &'static str {
            "not json"
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/models", axum::routing::get(invalid_json)),
            )
            .await
            .unwrap();
        });

        let report = test_api_service(
            &GatewayProvider::OpenAiCompatible,
            &format!("http://{address}/v1"),
            "sk-test",
        )
        .await
        .unwrap();
        assert_eq!(report.status, "failed");
        assert_eq!(report.category, "json");
        assert_eq!(report.http_status, Some(200));
        assert!(report.models.is_empty());
    }

    #[test]
    fn openai_routes_are_relative_to_normalized_v1_base_url() {
        let base = "https://example.test/v1";

        assert_eq!(
            build_upstream_url(base, model_discovery_route(&GatewayProvider::OpenAi))
                .unwrap()
                .as_str(),
            "https://example.test/v1/models"
        );
        assert_eq!(
            build_upstream_url(
                base,
                model_discovery_route(&GatewayProvider::OpenAiCompatible),
            )
            .unwrap()
            .as_str(),
            "https://example.test/v1/models"
        );
        assert_eq!(
            build_upstream_url(base, "responses").unwrap().as_str(),
            "https://example.test/v1/responses"
        );
        assert_eq!(
            build_upstream_url(base, "chat/completions")
                .unwrap()
                .as_str(),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn validates_and_masks_manual_proxy_urls() {
        let url = validate_manual_proxy_url(" socks5h://user:pass@127.0.0.1:7890 ")
            .expect("socks5h proxy URL must validate");
        assert_eq!(url, "socks5h://user:pass@127.0.0.1:7890");
        assert_eq!(mask_proxy_url(&url), "socks5h://***@127.0.0.1:7890");
        assert_eq!(
            mask_proxy_url("http://127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert!(validate_manual_proxy_url("ftp://127.0.0.1:21").is_err());
        assert!(validate_manual_proxy_url("http://127.0.0.1:7890/path").is_err());
        assert!(validate_manual_proxy_url("http://127.0.0.1:7890?token=secret").is_err());
    }

    #[test]
    fn reqwest_gateway_features_include_system_proxy_and_socks() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("Cargo.toml must be readable");
        assert!(manifest.contains("\"system-proxy\""));
        assert!(manifest.contains("\"socks\""));
    }

    #[test]
    fn smooth_weighted_scheduler_honors_a_one_to_three_distribution() {
        let candidates = vec![candidate("one", 1), candidate("three", 3)];
        let mut scheduler = WeightedScheduler::default();
        let mut counts = HashMap::<String, usize>::new();
        for _ in 0..40 {
            let selected = scheduler
                .select("responses:gpt-test:0", &candidates)
                .unwrap();
            *counts.entry(selected).or_default() += 1;
        }
        assert_eq!(counts.get("one"), Some(&10));
        assert_eq!(counts.get("three"), Some(&30));
    }

    #[test]
    fn maps_visible_models_to_upstream_payloads() {
        let mut stored = candidate("mapped", 1);
        stored.profile.models = vec!["codex-visible".into()];
        stored.profile.model_mappings = vec![GatewayModelMapping {
            model: "codex-visible".into(),
            upstream_model: "provider-real".into(),
            display_name: Some("Provider Real".into()),
            context_window: Some(64_000),
        }];

        assert_eq!(
            visible_model_for(&stored.profile, Some("codex-visible")),
            "codex-visible"
        );
        assert_eq!(
            upstream_model_for(&stored.profile, Some("codex-visible")).as_deref(),
            Some("provider-real")
        );
        let payload = payload_with_model(
            &serde_json::json!({"model": "codex-visible", "input": "hello"}),
            "provider-real",
        );
        assert_eq!(payload["model"], "provider-real");
    }

    #[test]
    fn rewrites_upstream_model_fields_back_to_visible_names() {
        let mut value = serde_json::json!({
            "model": "provider-real",
            "output": [{"model": "provider-real", "content": []}]
        });
        rewrite_model_fields(&mut value, "codex-visible");

        assert_eq!(value["model"], "codex-visible");
        assert_eq!(value["output"][0]["model"], "codex-visible");
    }

    #[test]
    fn converts_responses_requests_to_chat_completion_upstreams() {
        let converted = responses_to_chat_completion(
            &serde_json::json!({
                "model": "codex-visible",
                "instructions": "Be concise",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                }],
                "max_output_tokens": 128,
                "stream": false
            }),
            "provider-real",
        )
        .unwrap();

        assert_eq!(converted["model"], "provider-real");
        assert_eq!(converted["messages"][0]["role"], "system");
        assert_eq!(converted["messages"][1]["content"], "Hello");
        assert_eq!(converted["max_tokens"], 128);
    }

    #[test]
    fn converts_chat_text_and_function_tools_to_responses() {
        let converted = chat_to_responses(&serde_json::json!({
            "model": "gpt-test",
            "messages": [
                {"role": "system", "content": "Be concise"},
                {"role": "user", "content": "Weather?"},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "weather", "description": "Read weather",
                "parameters": {"type": "object"}
            }}],
            "tool_choice": {"type": "function", "function": {"name": "weather"}}
        }))
        .unwrap();

        assert_eq!(converted["stream"], true);
        assert_eq!(converted["store"], false);
        assert_eq!(converted["tools"][0]["name"], "weather");
        assert_eq!(converted["tool_choice"]["name"], "weather");
        assert!(converted["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output"));
    }

    #[test]
    fn aggregates_completed_responses_into_chat_completions() {
        let response = serde_json::json!({
            "id": "resp_1",
            "model": "gpt-test",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hello"}]},
                {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{}"}
            ],
            "usage": {"input_tokens": 2, "output_tokens": 3, "total_tokens": 5}
        });
        let sse = format!(
            "event: response.completed\ndata: {}\n\n",
            serde_json::json!({"type": "response.completed", "response": response})
        );
        let completed = completed_response(sse.as_bytes()).unwrap();
        let chat = response_to_chat_completion(&completed, "fallback");
        assert_eq!(chat["choices"][0]["message"]["content"], "hello");
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["usage"]["total_tokens"], 5);
    }

    #[tokio::test]
    async fn oauth_request_sets_bearer_account_and_originator_headers() {
        async fn echo(
            headers: HeaderMap,
            Json(payload): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "authorization": headers.get("authorization").and_then(|value| value.to_str().ok()),
                "account": headers.get("chatgpt-account-id").and_then(|value| value.to_str().ok()),
                "originator": headers.get("originator").and_then(|value| value.to_str().ok()),
                "payload": payload
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/responses", post(echo)))
                .await
                .unwrap();
        });
        let response = send_oauth_request(
            &reqwest::Client::new(),
            &url::Url::parse(&format!("http://{address}/responses")).unwrap(),
            &CodexOAuthCredential {
                id_token: "id".into(),
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                account_id: Some("account_1".into()),
                last_refresh_ms: 1,
            },
            &serde_json::json!({"model": "gpt-test"}),
        )
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
        server.abort();
        assert_eq!(response["authorization"], "Bearer access");
        assert_eq!(response["account"], "account_1");
        assert_eq!(response["originator"], "codex_cli_rs");
        assert_eq!(response["payload"]["model"], "gpt-test");
    }

    #[tokio::test]
    async fn manual_gateway_client_uses_the_configured_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(String::new()));
        let captured_for_server = captured.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let bytes_read = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer)
                .await
                .unwrap();
            *captured_for_server.lock().await =
                String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .await
            .unwrap();
        });
        let client = gateway_http_client(&GatewayUpstreamProxy::Manual {
            url: format!("http://{address}"),
        })
        .unwrap();

        let body = client
            .get("http://example.test/proxy-check")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        server.await.unwrap();

        let request = captured.lock().await.clone();
        assert_eq!(body, "ok");
        assert!(request.starts_with("GET http://example.test/proxy-check HTTP/1.1"));
    }

    #[tokio::test]
    async fn upstream_sse_read_error_finishes_with_a_gateway_failure_event() {
        let repository = Arc::new(Repository::memory());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
            let chunk =
                b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_partial\"}}\n\n";
            tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                format!("{:x}\r\n", chunk.len()).as_bytes(),
            )
            .await
            .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut socket, chunk)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut socket, b"\r\n")
                .await
                .unwrap();
        });
        let upstream = reqwest::Client::new()
            .post(format!("http://{address}/responses"))
            .send()
            .await
            .unwrap();
        let response = upstream_response(
            upstream,
            repository.clone(),
            Some(GatewayRequestKind::Responses),
        );
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        server.abort();
        let text = String::from_utf8_lossy(&body);

        assert!(text.contains("event: response.failed"));
        assert!(text.contains("\"code\":\"upstream_stream_error\""));
        assert!(!text.contains("data: [DONE]"));
        assert_eq!(
            repository
                .setting(super::GATEWAY_LAST_ERROR_SETTING)
                .unwrap()
                .as_deref(),
            Some(super::GATEWAY_ERROR_STREAM_INTERRUPTED)
        );
    }

    #[tokio::test]
    async fn gateway_client_keeps_reading_a_started_slow_sse_stream() {
        async fn slow_sse() -> Response {
            let stream = futures_util::stream::unfold(0, |state| async move {
                match state {
                    0 => Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from_static(
                            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_slow\"}}\n\n",
                        )),
                        1,
                    )),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        let payload = serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_slow",
                                "model": "gpt-test",
                                "output": []
                            }
                        });
                        Some((
                            Ok(Bytes::from(format!("data: {payload}\n\n"))),
                            2,
                        ))
                    }
                    _ => None,
                }
            });
            let mut response = Response::new(Body::from_stream(stream));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/responses", post(slow_sse)))
                .await
                .unwrap();
        });
        let client = gateway_http_client(&GatewayUpstreamProxy::System).unwrap();
        let response = send_with_first_response_timeout_after(
            client
                .post(format!("http://{address}/responses"))
                .json(&serde_json::json!({"stream": true})),
            Duration::from_millis(25),
        )
        .await
        .unwrap();
        let bytes = response.bytes().await.unwrap();
        server.abort();

        let completed = completed_response(&bytes).unwrap();
        assert_eq!(completed["id"], "resp_slow");
    }
}
