use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::TryStreamExt;
use ipnet::IpNet;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use reqwest::{redirect::Policy, Client};
use serde_json::{json, Value};
use url::Url;

use crate::{
    database::Repository,
    domain::GatewayStatus,
    error::{AppError, AppResult},
    profiles::{candidates_for_model, timestamp_ms},
    secrets::SecretStore,
};

#[derive(Clone)]
struct GatewayApiState {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    cidrs: Vec<IpNet>,
    lan_enabled: bool,
}

struct GatewayRuntime {
    handle: axum_server::Handle,
}

pub struct GatewayManager {
    repository: Arc<Repository>,
    secrets: Arc<dyn SecretStore>,
    certificate_dir: PathBuf,
    runtime: Mutex<Option<GatewayRuntime>>,
}

impl GatewayManager {
    pub fn new(
        repository: Arc<Repository>,
        secrets: Arc<dyn SecretStore>,
        certificate_dir: PathBuf,
    ) -> Self {
        Self {
            repository,
            secrets,
            certificate_dir,
            runtime: Mutex::new(None),
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
        self.repository
            .gateway_settings(self.is_running(), self.certificate_ready())
    }

    pub async fn start(&self) -> AppResult<GatewayStatus> {
        if self.is_running() {
            return Err(AppError::Conflict);
        }
        let settings = self
            .repository
            .gateway_settings(false, self.certificate_ready())?;
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
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(
            certificate_pem.into_bytes(),
            key_pem.into_bytes(),
        )
        .await
        .map_err(|_| AppError::Internal)?;
        let api_state = GatewayApiState {
            repository: self.repository.clone(),
            secrets: self.secrets.clone(),
            cidrs,
            lan_enabled: settings.bind_mode == "lan",
        };
        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/v1/models", get(list_models))
            .route("/v1/responses", post(responses))
            .route("/v1/chat/completions", post(chat_completions))
            .with_state(api_state);
        let handle = axum_server::Handle::new();
        let run_handle = handle.clone();
        let socket = SocketAddr::new(address, settings.port);
        tokio::spawn(async move {
            let _ = axum_server::bind_rustls(socket, config)
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

    async fn ensure_local_certificates(&self, address: IpAddr) -> AppResult<(String, String)> {
        std::fs::create_dir_all(&self.certificate_dir).map_err(|_| AppError::Internal)?;
        let ca_path = self.certificate_dir.join("gateway-ca.pem");
        let leaf_path = self.certificate_dir.join("gateway-leaf.pem");
        if ca_path.exists() && leaf_path.exists() {
            if let (Ok(ca), Ok(leaf), Ok(key)) = (
                std::fs::read_to_string(&ca_path),
                std::fs::read_to_string(&leaf_path),
                self.secrets.get("gateway:leaf-private-key").await,
            ) {
                return Ok((format!("{leaf}\n{ca}"), key));
            }
        }
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate().map_err(|_| AppError::Internal)?;
        let ca = ca_params
            .self_signed(&ca_key)
            .map_err(|_| AppError::Internal)?;
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
        let ca_pem = ca.pem();
        let leaf_pem = leaf.pem();
        std::fs::write(&ca_path, &ca_pem).map_err(|_| AppError::Internal)?;
        std::fs::write(&leaf_path, &leaf_pem).map_err(|_| AppError::Internal)?;
        Ok((format!("{leaf_pem}\n{ca_pem}"), leaf_key.serialize_pem()))
    }
}

pub fn validate_binding(mode: &str, address: IpAddr, cidrs: &[String]) -> AppResult<()> {
    match mode {
        "loopback" if address.is_loopback() && cidrs.is_empty() => Ok(()),
        "lan" if is_private_address(address) && !cidrs.is_empty() => {
            for cidr in cidrs {
                let _: IpNet = cidr.parse().map_err(|_| AppError::ValidationFailed)?;
            }
            Ok(())
        }
        _ => Err(AppError::ForbiddenNetworkTarget),
    }
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
    let mut models: Option<HashSet<String>> = None;
    for candidate in candidates {
        let available = candidate.profile.models.into_iter().collect::<HashSet<_>>();
        models = Some(models.map_or(available.clone(), |current| {
            current.intersection(&available).cloned().collect()
        }));
    }
    let data = models
        .unwrap_or_default()
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
    forward(state, peer.ip(), headers, payload, "responses").await
}

async fn chat_completions(
    State(state): State<GatewayApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    forward(state, peer.ip(), headers, payload, "chat/completions").await
}

async fn forward(
    state: GatewayApiState,
    peer: IpAddr,
    headers: HeaderMap,
    payload: Value,
    route: &str,
) -> Response {
    if !authorize(&state, peer, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let model = payload.get("model").and_then(Value::as_str);
    let candidates = match candidates_for_model(&state.repository, model) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": {"code": "model_not_available"}})),
            )
                .into_response()
        }
    };
    let client = match Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(90))
        .build()
    {
        Ok(client) => client,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let started = Instant::now();
    for candidate in candidates.into_iter().take(2) {
        let Some(base_url) = candidate.profile.base_url else {
            continue;
        };
        let Some(secret_ref) = candidate.secret_ref else {
            continue;
        };
        let endpoint = match build_upstream_url(&base_url, route) {
            Ok(url) => url,
            Err(_) => continue,
        };
        let key = match state.secrets.get(&secret_ref).await {
            Ok(key) => key,
            Err(_) => continue,
        };
        match client
            .post(endpoint)
            .bearer_auth(key)
            .json(&payload)
            .send()
            .await
        {
            Ok(response)
                if response.status().is_server_error()
                    || response.status() == StatusCode::TOO_MANY_REQUESTS =>
            {
                continue
            }
            Ok(response) => {
                let successful = response.status().is_success();
                let _ = state
                    .repository
                    .record_metric(successful, started.elapsed().as_millis() as i64);
                return upstream_response(response);
            }
            Err(_) => continue,
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

fn upstream_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    let mut output = Response::new(Body::from_stream(stream));
    *output.status_mut() = status;
    if let Some(content_type) = content_type {
        output
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    output
}

fn build_upstream_url(base_url: &str, route: &str) -> AppResult<Url> {
    let base = Url::parse(base_url).map_err(|_| AppError::ValidationFailed)?;
    if base.scheme() != "https" || base.host_str().is_none() {
        return Err(AppError::ForbiddenNetworkTarget);
    }
    base.join(route).map_err(|_| AppError::ValidationFailed)
}

fn authorize(state: &GatewayApiState, peer: IpAddr, headers: &HeaderMap) -> bool {
    if state.lan_enabled {
        if !state.cidrs.iter().any(|network| network.contains(&peer)) {
            return false;
        }
    } else if !peer.is_loopback() {
        return false;
    }
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
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
    use super::validate_binding;
    use crate::error::AppError;
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
}
