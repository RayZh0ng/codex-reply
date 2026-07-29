use serde::{Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("输入不符合要求")]
    ValidationFailed,
    #[error("系统安全存储不可用")]
    SecretStoreUnavailable,
    #[error("系统钥匙串需要用户授权")]
    KeychainInteractionRequired,
    #[allow(dead_code)]
    #[error("已保存的账号凭据无法被当前版本读取；请重新授权该账号")]
    ProfileCredentialMigrationRequired,
    #[error("未找到请求的资源")]
    NotFound,
    #[error("该资源状态已发生变化")]
    Conflict,
    #[error("网关尚未运行")]
    GatewayNotRunning,
    #[error("选择的网关模型当前不可用；请刷新模型并确认账号已加入网关账号池")]
    GatewayModelUnavailable,
    #[error("网络目标不在允许范围内")]
    ForbiddenNetworkTarget,
    #[error("上游服务当前不可用")]
    UpstreamUnavailable,
    #[error("软件更新暂不可用")]
    AppUpdateUnavailable,
    #[error("此操作需要明确确认")]
    ConfirmationRequired,
    #[error("本机运行时不可用")]
    RuntimeUnavailable,
    #[error("该档案暂不能用于受管 Codex 会话；请完成 OAuth 授权后重试")]
    ProfileRuntimeUnavailable,
    #[error("Codex 凭据已切换，但 ChatGPT/Codex 桌面端未能重启；请手动打开它")]
    DesktopUnavailable,
    #[error("无法更新 macOS 的 Codex Auth 钥匙串")]
    CodexKeychainUnavailable,
    #[error("请先选择一个已授权的 OAuth 档案，再启动受管 Codex 任务")]
    CurrentProfileRequired,
    #[error("本机会话状态暂不可读")]
    LocalStateUnavailable,
    #[error("内部状态不可用")]
    Internal,
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::ValidationFailed => "validation_failed",
            Self::SecretStoreUnavailable => "secret_store_unavailable",
            Self::KeychainInteractionRequired => "keychain_interaction_required",
            Self::ProfileCredentialMigrationRequired => "profile_credential_migration_required",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::GatewayNotRunning => "gateway_not_running",
            Self::GatewayModelUnavailable => "gateway_model_unavailable",
            Self::ForbiddenNetworkTarget => "forbidden_network_target",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::AppUpdateUnavailable => "app_update_unavailable",
            Self::ConfirmationRequired => "confirmation_required",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::ProfileRuntimeUnavailable => "profile_runtime_unavailable",
            Self::DesktopUnavailable => "desktop_unavailable",
            Self::CodexKeychainUnavailable => "codex_keychain_unavailable",
            Self::CurrentProfileRequired => "current_profile_required",
            Self::LocalStateUnavailable => "local_state_unavailable",
            Self::Internal => "internal",
        }
    }
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct ErrorPayload<'a> {
            code: &'a str,
            message: String,
        }

        ErrorPayload {
            code: self.code(),
            message: self.to_string(),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_safe_code_and_message_for_ipc() {
        let payload = serde_json::to_value(AppError::Internal).unwrap();

        assert_eq!(payload["code"], "internal");
        assert_eq!(payload["message"], "内部状态不可用");
    }

    #[test]
    fn serializes_local_state_unavailable_with_actionable_code() {
        let payload = serde_json::to_value(AppError::LocalStateUnavailable).unwrap();

        assert_eq!(payload["code"], "local_state_unavailable");
        assert_eq!(payload["message"], "本机会话状态暂不可读");
    }

    #[test]
    fn serializes_keychain_authorization_without_platform_details() {
        let payload = serde_json::to_value(AppError::KeychainInteractionRequired).unwrap();

        assert_eq!(payload["code"], "keychain_interaction_required");
        assert_eq!(payload["message"], "系统钥匙串需要用户授权");
    }

    #[test]
    fn serializes_gateway_model_unavailable_with_actionable_code() {
        let payload = serde_json::to_value(AppError::GatewayModelUnavailable).unwrap();

        assert_eq!(payload["code"], "gateway_model_unavailable");
        assert!(payload["message"].as_str().unwrap().contains("网关模型"));
    }

    #[test]
    fn serializes_app_update_unavailable_with_actionable_code() {
        let payload = serde_json::to_value(AppError::AppUpdateUnavailable).unwrap();

        assert_eq!(payload["code"], "app_update_unavailable");
        assert_eq!(payload["message"], "软件更新暂不可用");
    }
}
