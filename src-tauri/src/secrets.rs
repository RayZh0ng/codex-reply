use std::sync::Mutex;

#[cfg(test)]
use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::{AppError, AppResult};

const SERVICE_NAME: &str = "com.codexrelay.app";

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn set(&self, reference: &str, value: &str) -> AppResult<()>;
    async fn get(&self, reference: &str) -> AppResult<String>;
    async fn set_without_user_interaction(&self, reference: &str, value: &str) -> AppResult<()> {
        self.set(reference, value).await
    }
    async fn get_without_user_interaction(&self, reference: &str) -> AppResult<String> {
        self.get(reference).await
    }
    async fn delete(&self, reference: &str) -> AppResult<()>;
}

pub struct KeyringSecretStore {
    access_lock: Mutex<()>,
}

impl KeyringSecretStore {
    pub fn new() -> Self {
        Self {
            access_lock: Mutex::new(()),
        }
    }

    fn entry(reference: &str) -> AppResult<keyring::Entry> {
        keyring::Entry::new(SERVICE_NAME, reference).map_err(|_| AppError::SecretStoreUnavailable)
    }

    fn get_error(error: keyring::Error) -> AppError {
        match error {
            keyring::Error::NoEntry => AppError::NotFound,
            keyring::Error::PlatformFailure(_) => AppError::ProfileCredentialMigrationRequired,
            _ => AppError::SecretStoreUnavailable,
        }
    }

    fn without_user_interaction_error(error: keyring::Error) -> AppError {
        match error {
            keyring::Error::NoEntry => AppError::NotFound,
            _ => AppError::KeychainInteractionRequired,
        }
    }

    fn lock_access(&self) -> AppResult<std::sync::MutexGuard<'_, ()>> {
        self.access_lock.lock().map_err(|_| AppError::Internal)
    }
}

#[async_trait]
impl SecretStore for KeyringSecretStore {
    async fn set(&self, reference: &str, value: &str) -> AppResult<()> {
        let _access = self.lock_access()?;
        Self::entry(reference)?
            .set_password(value)
            .map_err(|_| AppError::SecretStoreUnavailable)
    }

    async fn get(&self, reference: &str) -> AppResult<String> {
        let _access = self.lock_access()?;
        Self::entry(reference)?
            .get_password()
            .map_err(Self::get_error)
    }

    async fn set_without_user_interaction(&self, reference: &str, value: &str) -> AppResult<()> {
        let _access = self.lock_access()?;
        #[cfg(target_os = "macos")]
        let _interaction_lock =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|_| AppError::KeychainInteractionRequired)?;
        Self::entry(reference)?
            .set_password(value)
            .map_err(Self::without_user_interaction_error)
    }

    async fn get_without_user_interaction(&self, reference: &str) -> AppResult<String> {
        let _access = self.lock_access()?;
        #[cfg(target_os = "macos")]
        let _interaction_lock =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|_| AppError::KeychainInteractionRequired)?;
        Self::entry(reference)?
            .get_password()
            .map_err(Self::without_user_interaction_error)
    }

    async fn delete(&self, reference: &str) -> AppResult<()> {
        let _access = self.lock_access()?;
        match Self::entry(reference)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(AppError::SecretStoreUnavailable),
        }
    }
}

#[cfg(test)]
pub struct MemorySecretStore(Mutex<HashMap<String, String>>);

#[cfg(test)]
impl MemorySecretStore {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

#[cfg(test)]
#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn set(&self, reference: &str, value: &str) -> AppResult<()> {
        self.0
            .lock()
            .map_err(|_| AppError::Internal)?
            .insert(reference.to_owned(), value.to_owned());
        Ok(())
    }

    async fn get(&self, reference: &str) -> AppResult<String> {
        self.0
            .lock()
            .map_err(|_| AppError::Internal)?
            .get(reference)
            .cloned()
            .ok_or(AppError::NotFound)
    }

    async fn set_without_user_interaction(&self, reference: &str, value: &str) -> AppResult<()> {
        self.set(reference, value).await
    }

    async fn get_without_user_interaction(&self, reference: &str) -> AppResult<String> {
        self.get(reference).await
    }

    async fn delete(&self, reference: &str) -> AppResult<()> {
        self.0
            .lock()
            .map_err(|_| AppError::Internal)?
            .remove(reference);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_a_missing_credential_from_an_inaccessible_keychain() {
        assert!(matches!(
            KeyringSecretStore::get_error(keyring::Error::NoEntry),
            AppError::NotFound
        ));
    }

    #[test]
    fn identifies_a_credential_rejected_by_the_previous_app_signature() {
        assert!(matches!(
            KeyringSecretStore::get_error(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other("access denied")
            ))),
            AppError::ProfileCredentialMigrationRequired
        ));
    }

    #[test]
    fn treats_noninteractive_keychain_failures_as_a_recoverable_authorization_need() {
        assert!(matches!(
            KeyringSecretStore::without_user_interaction_error(keyring::Error::PlatformFailure(
                Box::new(std::io::Error::other("interaction not allowed"))
            )),
            AppError::KeychainInteractionRequired
        ));
    }

    #[tokio::test]
    async fn memory_store_supports_silent_access_for_non_macos_tests() {
        let store = MemorySecretStore::new();
        store
            .set_without_user_interaction("oauth", "credential")
            .await
            .unwrap();
        assert_eq!(
            store.get_without_user_interaction("oauth").await.unwrap(),
            "credential"
        );
    }
}
