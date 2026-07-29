use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::process::Command;

use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{StreamExt, TryStreamExt};
use ipnet::IpNet;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use reqwest::{redirect::Policy, Client, NoProxy, Proxy};
use serde_json::{json, Value};
use url::Url;

use crate::{
    database::Repository,
    database::StoredProfile,
    domain::{
        ApiServiceTestReport, GatewayModelMapping, GatewayNetworkAddress, GatewayProvider,
        GatewayStatus, GatewayWireApi, MaskedProfile, ProfileKind,
    },
    error::{AppError, AppResult},
    oauth_credentials::{CredentialAccess, OAuthCredentialStore},
    profiles::{candidates_for_model, cool_down_profile, timestamp_ms},
    secrets::SecretStore,
};

#[derive(Clone)]
struct GatewayApiState {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    oauth_credentials: Arc<OAuthCredentialStore>,
    cidrs: Vec<IpNet>,
    oauth_responses_url: Url,
    upstream_proxy: GatewayUpstreamProxy,
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
        self.repository.gateway_settings(
            self.is_running(),
            self.certificate_ready(),
            available_lan_addresses(),
        )
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
        if settings.bind_mode != "lan"
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
        let cidrs = settings
            .cidrs
            .iter()
            .map(|value| {
                value
                    .parse::<IpNet>()
                    .map_err(|_| AppError::ValidationFailed)
            })
            .collect::<AppResult<Vec<_>>>()?;
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
        let api_state = GatewayApiState {
            repository: self.repository.clone(),
            secrets: self.secrets.clone(),
            oauth_credentials: self.oauth_credentials.clone(),
            cidrs,
            oauth_responses_url: Url::parse(CODEX_RESPONSES_URL).map_err(|_| AppError::Internal)?,
            upstream_proxy,
            scheduler: self.scheduler.clone(),
            affinities: self.affinities.clone(),
        };
        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/v1/models", get(list_models))
            .route("/v1/responses", post(responses))
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/messages", post(messages))
            .route("/v1beta/*path", post(gemini))
            .route("/api/chat", post(ollama_chat))
            .route("/api/generate", post(ollama_generate))
            .route("/api/tags", get(ollama_tags))
            .with_state(api_state);
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

    pub fn trust_ca_in_macos_keychain(&self) -> AppResult<()> {
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
        #[cfg(not(target_os = "macos"))]
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

async fn healthz(State(state): State<GatewayApiState>) -> impl IntoResponse {
    let has_candidates = candidates_for_model(&state.repository, None)
        .map(|profiles| !profiles.is_empty())
        .unwrap_or(false);
    let status = if has_candidates { "ok" } else { "unavailable" };
    (StatusCode::OK, Json(json!({"status": status})))
}

async fn list_models(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state, peer.ip(), &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let candidates = match candidates_for_model(&state.repository, None) {
        Ok(value) => value,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let models = candidates
        .into_iter()
        .flat_map(|candidate| candidate.profile.models)
        .collect::<HashSet<_>>();
    let data = models
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "codex-relay"}))
        .collect::<Vec<_>>();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn responses(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    forward(
        state,
        peer.ip(),
        headers,
        payload,
        "responses",
        GatewayProvider::OpenAiCompatible,
        Some(GatewayRequestKind::Responses),
    )
    .await
}

async fn chat_completions(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    forward(
        state,
        peer.ip(),
        headers,
        payload,
        "chat/completions",
        GatewayProvider::OpenAiCompatible,
        Some(GatewayRequestKind::ChatCompletions),
    )
    .await
}

async fn messages(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    forward(
        state,
        peer.ip(),
        headers,
        payload,
        "v1/messages",
        GatewayProvider::Anthropic,
        None,
    )
    .await
}

async fn gemini(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Path(path): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    forward(
        state,
        peer.ip(),
        headers,
        payload,
        &format!("v1beta/{path}"),
        GatewayProvider::Gemini,
        None,
    )
    .await
}

async fn ollama_chat(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    forward(
        state,
        peer.ip(),
        headers,
        payload,
        "api/chat",
        GatewayProvider::Ollama,
        None,
    )
    .await
}

async fn ollama_generate(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    forward(
        state,
        peer.ip(),
        headers,
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

fn payload_with_model(payload: &Value, model: &str) -> Value {
    let mut next = payload.clone();
    if let Some(object) = next.as_object_mut() {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }
    next
}

async fn ollama_tags(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state, peer.ip(), &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let models = candidates_for_model(&state.repository, None)
        .unwrap_or_default()
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

async fn forward(
    state: GatewayApiState,
    peer: IpAddr,
    headers: HeaderMap,
    payload: Value,
    route: &str,
    requested_provider: GatewayProvider,
    request_kind: Option<GatewayRequestKind>,
) -> Response {
    if !authorize(&state, peer, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request_kind == Some(GatewayRequestKind::ChatCompletions) {
        if let Err(message) = validate_chat_request(&payload) {
            return openai_bad_request(&message);
        }
    }
    let model = payload.get("model").and_then(Value::as_str);
    let mut candidates = match candidates_for_model(&state.repository, model) {
        Ok(value) => value
            .into_iter()
            .filter(|candidate| {
                if candidate.profile.kind == ProfileKind::CodexOauth {
                    request_kind.is_some()
                        && requested_provider == GatewayProvider::OpenAiCompatible
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
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": {"code": "model_not_available"}})),
            )
                .into_response()
        }
    };
    if candidates.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"code": "model_not_available"}})),
        )
            .into_response();
    }
    cool_down_exhausted_profiles(&state.repository, &mut candidates);
    if candidates.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"code": "all_profiles_exhausted"}})),
        )
            .into_response();
    }
    if request_kind == Some(GatewayRequestKind::Responses) {
        if let Some(previous_id) = payload.get("previous_response_id").and_then(Value::as_str) {
            match affinity_profile(&state, previous_id) {
                Some(profile_id) => {
                    candidates.retain(|candidate| candidate.profile.id == profile_id);
                    if candidates.is_empty() {
                        return affinity_conflict();
                    }
                }
                None => {
                    candidates.retain(|candidate| candidate.profile.kind == ProfileKind::ApiKey);
                    if candidates.is_empty() {
                        return affinity_conflict();
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
    let client = match gateway_http_client(&state.upstream_proxy) {
        Ok(client) => client,
        Err(_) => {
            record_gateway_upstream_error(&state.repository, GATEWAY_ERROR_PROXY_CONFIG);
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let started = Instant::now();
    for candidate in candidates.into_iter().take(2) {
        if candidate.profile.kind == ProfileKind::CodexOauth {
            let Some(kind) = request_kind else {
                continue;
            };
            match forward_oauth_candidate(&state, &client, &candidate, &payload, kind).await {
                OAuthAttempt::Response(response) => {
                    let successful = response.status().is_success();
                    if successful {
                        clear_gateway_upstream_error(&state.repository);
                    }
                    let _ = state
                        .repository
                        .record_metric(successful, started.elapsed().as_millis() as i64);
                    return response;
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
        let client_stream = payload
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut upstream_route = route.to_owned();
        let mut upstream_payload = payload_with_model(&payload, &upstream_model);
        let mut response_adapter = ApiResponseAdapter::Direct {
            visible_model: visible_model.clone(),
        };
        if candidate.profile.wire_api == GatewayWireApi::ChatCompletions
            && request_kind == Some(GatewayRequestKind::Responses)
        {
            upstream_route = "chat/completions".to_owned();
            upstream_payload = match responses_to_chat_completion(&payload, &upstream_model) {
                Ok(value) => value,
                Err(message) => return openai_bad_request(&message),
            };
            response_adapter = ApiResponseAdapter::ChatToResponses {
                client_stream,
                visible_model,
            };
        } else if candidate.profile.wire_api == GatewayWireApi::Responses
            && request_kind == Some(GatewayRequestKind::ChatCompletions)
        {
            upstream_route = "responses".to_owned();
            upstream_payload = match chat_to_responses(&payload) {
                Ok(value) => payload_with_model(&value, &upstream_model),
                Err(message) => return openai_bad_request(&message),
            };
            response_adapter = ApiResponseAdapter::ResponsesToChat {
                client_stream,
                visible_model,
                profile_id: candidate.profile.id.clone(),
            };
        }
        let endpoint = match build_upstream_url(&base_url, &upstream_route) {
            Ok(url) => url,
            Err(_) => continue,
        };
        let key = match state.secrets.get(&secret_ref).await {
            Ok(key) => key,
            Err(_) => continue,
        };
        let request = upstream_request(
            client.post(endpoint),
            &candidate.profile.provider,
            &key,
            &upstream_payload,
        );
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
            Ok(response) => {
                let successful = response.status().is_success();
                if successful {
                    clear_gateway_upstream_error(&state.repository);
                }
                let _ = state
                    .repository
                    .record_metric(successful, started.elapsed().as_millis() as i64);
                return match response_adapter {
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
                };
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
    let _ = state
        .repository
        .record_metric(false, started.elapsed().as_millis() as i64);
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"error": {"code": "upstream_unavailable"}})),
    )
        .into_response()
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
    Response(Response),
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
            Err(message) => return OAuthAttempt::Response(openai_bad_request(&message)),
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
        return OAuthAttempt::Response(upstream_response(
            response,
            state.repository.clone(),
            Some(kind),
        ));
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
                let stream = response.bytes_stream().map_err(std::io::Error::other).scan(
                    String::new(),
                    move |buffer, chunk| {
                        let affinities = affinities.clone();
                        let profile_id = profile_id.clone();
                        let result = match chunk {
                            Ok(bytes) => {
                                buffer.push_str(&String::from_utf8_lossy(&bytes));
                                for event in drain_sse_events(buffer) {
                                    if let Some(id) = response_id_from_event(&event) {
                                        record_affinity(&affinities, &id, &profile_id);
                                    }
                                }
                                Some(Ok::<Bytes, std::io::Error>(bytes))
                            }
                            Err(_) => {
                                record_gateway_upstream_error(
                                    &repository,
                                    GATEWAY_ERROR_STREAM_INTERRUPTED,
                                );
                                Some(Ok(Bytes::from(sse_upstream_error_event(Some(
                                    GatewayRequestKind::Responses,
                                )))))
                            }
                        };
                        futures_util::future::ready(result)
                    },
                );
                sse_response(Body::from_stream(stream))
            }
            GatewayRequestKind::ChatCompletions => {
                let repository = repository.clone();
                let stream = response.bytes_stream().map_err(std::io::Error::other).scan(
                    ChatStreamState::new(model, profile_id, affinities),
                    move |state, chunk| {
                        let result = match chunk {
                            Ok(bytes) => {
                                state.buffer.push_str(&String::from_utf8_lossy(&bytes));
                                let events = drain_sse_events(&mut state.buffer);
                                let mut output = String::new();
                                for event in events {
                                    output.push_str(&state.translate(event));
                                }
                                Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                            }
                            Err(_) => {
                                record_gateway_upstream_error(
                                    &repository,
                                    GATEWAY_ERROR_STREAM_INTERRUPTED,
                                );
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
    if payload.get("n").and_then(Value::as_i64).unwrap_or(1) != 1 {
        return Err("n must be 1".to_owned());
    }
    for field in ["audio", "logprobs", "top_logprobs"] {
        if payload.get(field).is_some_and(|value| !value.is_null()) {
            return Err(format!("{field} is not supported"));
        }
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

async fn adapt_chat_completion_response(
    response: reqwest::Response,
    client_stream: bool,
    model: String,
    repository: Arc<Repository>,
) -> Response {
    if client_stream {
        let stream = response.bytes_stream().map_err(std::io::Error::other).scan(
            ChatToResponsesStreamState::new(model),
            move |state, chunk| {
                let result = match chunk {
                    Ok(bytes) => {
                        state.buffer.push_str(&String::from_utf8_lossy(&bytes));
                        let events = drain_sse_events(&mut state.buffer);
                        let mut output = String::new();
                        for event in events {
                            output.push_str(&state.translate(event));
                        }
                        Some(Ok::<Bytes, std::io::Error>(Bytes::from(output)))
                    }
                    Err(_) => {
                        record_gateway_upstream_error(
                            &repository,
                            GATEWAY_ERROR_STREAM_INTERRUPTED,
                        );
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
        let stream = stream.map(move |chunk| match chunk {
            Ok(bytes) => Ok::<Bytes, std::io::Error>(bytes),
            Err(_) => {
                record_gateway_upstream_error(&repository, GATEWAY_ERROR_STREAM_INTERRUPTED);
                Ok(Bytes::from(sse_upstream_error_event(kind)))
            }
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
        .connect_timeout(GATEWAY_CONNECT_TIMEOUT);
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
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_gateway_error\",\"status\":\"failed\"},\"error\":{\"code\":\"upstream_stream_error\",\"message\":\"upstream stream interrupted\"}}\n\ndata: [DONE]\n\n"
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

fn authorize(state: &GatewayApiState, peer: IpAddr, headers: &HeaderMap) -> bool {
    if !state.cidrs.is_empty() && !state.cidrs.iter().any(|network| network.contains(&peer)) {
        return false;
    }
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        })
        .or_else(|| {
            headers
                .get("x-goog-api-key")
                .and_then(|value| value.to_str().ok())
        });
    let Some(value) = value else {
        return false;
    };
    let keys = match state.repository.valid_key_hashes() {
        Ok(keys) => keys,
        Err(_) => return false,
    };
    for (id, hash, _) in keys {
        if PasswordHash::new(&hash).ok().is_some_and(|parsed| {
            Argon2::default()
                .verify_password(value.as_bytes(), &parsed)
                .is_ok()
        }) {
            let _ = state.repository.record_key_use(&id, timestamp_ms());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use axum::{
        body::{to_bytes, Body, Bytes},
        http::{header, HeaderMap, HeaderValue},
        response::Response,
        routing::post,
        Json, Router,
    };

    use super::{
        build_upstream_url, chat_to_responses, completed_response, gateway_http_client,
        mask_proxy_url, model_discovery_route, model_ids_from_provider_response,
        payload_with_model, response_to_chat_completion, responses_to_chat_completion,
        rewrite_model_fields, send_oauth_request, send_with_first_response_timeout_after,
        test_api_service, upstream_model_for, upstream_response, validate_binding,
        validate_manual_proxy_url, visible_model_for, GatewayRequestKind, GatewayUpstreamProxy,
        WeightedScheduler,
    };
    use crate::error::AppError;
    use crate::{
        database::{Repository, StoredProfile},
        domain::{
            GatewayModelMapping, GatewayProvider, GatewayWireApi, MaskedProfile, ProfileKind,
        },
        profiles::CodexOAuthCredential,
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
                is_current: false,
                account: None,
            },
            secret_ref: Some(format!("profile:{id}:oauth")),
            credential_fingerprint: None,
        }
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
                is_current: false,
                account: None,
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
        assert!(text.contains("data: [DONE]"));
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
