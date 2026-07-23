use std::{net::IpAddr, sync::Arc, time::Duration};

use reqwest::{redirect::Policy, Client};
use serde_json::json;
use url::Url;
use uuid::Uuid;

use crate::{
    database::{Repository, StoredChannel},
    domain::{MaskedChannel, TestChannelInput, UpsertChannelInput},
    error::{AppError, AppResult},
    secrets::SecretStore,
};

pub async fn upsert_channel(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    input: UpsertChannelInput,
) -> AppResult<MaskedChannel> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    if input.name.trim().is_empty() {
        return Err(AppError::ValidationFailed);
    }
    validate_endpoint(&input.endpoint)?;
    let endpoint_mask = mask_endpoint(&input.endpoint)?;
    let id = input.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let endpoint_ref = format!("channel:{id}:endpoint");
    let signing_ref = input
        .signing_secret
        .as_ref()
        .filter(|secret| !secret.trim().is_empty())
        .map(|_| format!("channel:{id}:signing"));
    secrets.set(&endpoint_ref, &input.endpoint).await?;
    if let (Some(reference), Some(value)) = (&signing_ref, &input.signing_secret) {
        secrets.set(reference, value).await?;
    }
    let stored = StoredChannel {
        channel: MaskedChannel {
            id: id.clone(),
            name: input.name.trim().to_owned(),
            kind: input.kind,
            enabled: input.enabled,
            endpoint_mask,
            last_status: "not_tested".to_owned(),
        },
        endpoint_ref,
        signing_ref,
    };
    let existing = repository.channel(&id);
    if existing.is_ok() {
        repository.update_channel(&stored)?;
    } else {
        repository.insert_channel(&stored)?;
    }
    Ok(stored.channel)
}

pub async fn test_channel(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    input: TestChannelInput,
) -> AppResult<MaskedChannel> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let stored = repository.channel(&input.id)?;
    let endpoint = secrets.get(&stored.endpoint_ref).await?;
    let signing_secret = match stored.signing_ref.as_deref() {
        Some(reference) => Some(secrets.get(reference).await?),
        None => None,
    };
    let result = deliver(
        &endpoint,
        "notification.test",
        "Codex Relay 测试通知已安全送出。",
        signing_secret.as_deref(),
    )
    .await;
    let status = if result.is_ok() {
        "delivered"
    } else {
        "failed"
    };
    repository.update_channel_status(&input.id, status)?;
    let mut channel = stored.channel;
    channel.last_status = status.to_owned();
    result.map(|_| channel)
}

pub async fn delete_channel(
    repository: &Repository,
    secrets: Arc<dyn SecretStore>,
    id: &str,
    confirmed: bool,
) -> AppResult<()> {
    if !confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let (endpoint_ref, signing_ref) = repository.delete_channel(id)?;
    secrets.delete(&endpoint_ref).await?;
    if let Some(reference) = signing_ref {
        secrets.delete(&reference).await?;
    }
    Ok(())
}

pub async fn deliver(
    endpoint: &str,
    event_type: &str,
    summary: &str,
    signing_secret: Option<&str>,
) -> AppResult<()> {
    validate_endpoint(endpoint)?;
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| AppError::Internal)?;
    let summary = redact_summary(summary);
    let mut request = client.post(endpoint).json(&json!({"event_type": event_type, "occurred_at_ms": crate::profiles::timestamp_ms(), "summary": summary}));
    if let Some(secret) = signing_secret {
        request = request.header("X-Codex-Relay-Signature", secret);
    }
    let mut last_error = AppError::UpstreamUnavailable;
    for attempt in 0..3 {
        match request.try_clone().ok_or(AppError::Internal)?.send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            _ => {
                last_error = AppError::UpstreamUnavailable;
                tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
            }
        }
    }
    Err(last_error)
}

pub fn redact_summary(summary: &str) -> String {
    summary
        .replace("sk-", "[redacted]-")
        .replace("Bearer ", "Bearer [redacted] ")
        .chars()
        .take(500)
        .collect()
}

fn validate_endpoint(endpoint: &str) -> AppResult<()> {
    let parsed = Url::parse(endpoint).map_err(|_| AppError::ValidationFailed)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(AppError::ForbiddenNetworkTarget);
    }
    if let Some(host) = parsed.host_str() {
        if let Ok(ip) = host.parse::<IpAddr>() {
            let private = match ip {
                IpAddr::V4(value) => value.is_private(),
                IpAddr::V6(value) => value.is_unique_local(),
            };
            if ip.is_loopback() || private || ip.is_unspecified() || ip.is_multicast() {
                return Err(AppError::ForbiddenNetworkTarget);
            }
        }
    }
    Ok(())
}

fn mask_endpoint(endpoint: &str) -> AppResult<String> {
    let parsed = Url::parse(endpoint).map_err(|_| AppError::ValidationFailed)?;
    Ok(format!(
        "{}://{}/••••",
        parsed.scheme(),
        parsed.host_str().ok_or(AppError::ValidationFailed)?
    ))
}

#[cfg(test)]
mod tests {
    use super::redact_summary;
    #[test]
    fn redacts_and_truncates_summary() {
        let value = redact_summary(&format!("Bearer sk-secret {}", "x".repeat(600)));
        assert!(!value.contains("sk-secret"));
        assert!(value.chars().count() <= 500);
    }
}
