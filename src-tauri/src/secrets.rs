use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

const LOCAL_KEY_FILE: &str = "secrets.key";
const LOCAL_VAULT_FILE: &str = "secrets.vault";

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

#[derive(Default, Serialize, Deserialize)]
struct LocalVaultFile {
    entries: HashMap<String, LocalVaultEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct LocalVaultEntry {
    nonce: String,
    ciphertext: String,
}

pub struct LocalEncryptedSecretStore {
    key: [u8; 32],
    vault_path: PathBuf,
    access_lock: Mutex<()>,
}

impl LocalEncryptedSecretStore {
    pub fn open(data_dir: impl AsRef<Path>) -> AppResult<Self> {
        let data_dir = data_dir.as_ref();
        fs::create_dir_all(data_dir).map_err(|_| AppError::SecretStoreUnavailable)?;
        let key = load_or_create_local_key(&data_dir.join(LOCAL_KEY_FILE))?;
        Ok(Self {
            key,
            vault_path: data_dir.join(LOCAL_VAULT_FILE),
            access_lock: Mutex::new(()),
        })
    }

    pub fn get_blocking(&self, reference: &str) -> AppResult<String> {
        let _access = self.lock_access()?;
        let vault = self.load_vault()?;
        let entry = vault.entries.get(reference).ok_or(AppError::NotFound)?;
        let nonce = URL_SAFE_NO_PAD
            .decode(&entry.nonce)
            .map_err(|_| AppError::SecretStoreUnavailable)?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(&entry.ciphertext)
            .map_err(|_| AppError::SecretStoreUnavailable)?;
        let cipher =
            Aes256Gcm::new_from_slice(&self.key).map_err(|_| AppError::SecretStoreUnavailable)?;
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| AppError::SecretStoreUnavailable)?;
        String::from_utf8(plaintext).map_err(|_| AppError::SecretStoreUnavailable)
    }

    fn set_blocking(&self, reference: &str, value: &str) -> AppResult<()> {
        let _access = self.lock_access()?;
        let mut nonce = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher =
            Aes256Gcm::new_from_slice(&self.key).map_err(|_| AppError::SecretStoreUnavailable)?;
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), value.as_bytes())
            .map_err(|_| AppError::SecretStoreUnavailable)?;
        let mut vault = self.load_vault()?;
        vault.entries.insert(
            reference.to_owned(),
            LocalVaultEntry {
                nonce: URL_SAFE_NO_PAD.encode(nonce),
                ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
            },
        );
        self.save_vault(&vault)
    }

    fn delete_blocking(&self, reference: &str) -> AppResult<()> {
        let _access = self.lock_access()?;
        let mut vault = self.load_vault()?;
        vault.entries.remove(reference);
        self.save_vault(&vault)
    }

    fn load_vault(&self) -> AppResult<LocalVaultFile> {
        match fs::read_to_string(&self.vault_path) {
            Ok(content) if content.trim().is_empty() => Ok(LocalVaultFile::default()),
            Ok(content) => {
                serde_json::from_str(&content).map_err(|_| AppError::SecretStoreUnavailable)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(LocalVaultFile::default())
            }
            Err(_) => Err(AppError::SecretStoreUnavailable),
        }
    }

    fn save_vault(&self, vault: &LocalVaultFile) -> AppResult<()> {
        let content = serde_json::to_vec(vault).map_err(|_| AppError::SecretStoreUnavailable)?;
        atomic_write_private(&self.vault_path, &content)
    }

    fn lock_access(&self) -> AppResult<std::sync::MutexGuard<'_, ()>> {
        self.access_lock.lock().map_err(|_| AppError::Internal)
    }
}

#[async_trait]
impl SecretStore for LocalEncryptedSecretStore {
    async fn set(&self, reference: &str, value: &str) -> AppResult<()> {
        self.set_blocking(reference, value)
    }

    async fn get(&self, reference: &str) -> AppResult<String> {
        self.get_blocking(reference)
    }

    async fn set_without_user_interaction(&self, reference: &str, value: &str) -> AppResult<()> {
        self.set_blocking(reference, value)
    }

    async fn get_without_user_interaction(&self, reference: &str) -> AppResult<String> {
        self.get_blocking(reference)
    }

    async fn delete(&self, reference: &str) -> AppResult<()> {
        self.delete_blocking(reference)
    }
}

fn load_or_create_local_key(path: &Path) -> AppResult<[u8; 32]> {
    match fs::read(path) {
        Ok(bytes) => bytes
            .try_into()
            .map_err(|_| AppError::SecretStoreUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0_u8; 32];
            OsRng.fill_bytes(&mut key);
            atomic_write_private(path, &key)?;
            Ok(key)
        }
        Err(_) => Err(AppError::SecretStoreUnavailable),
    }
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::SecretStoreUnavailable)?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("secret"),
        Uuid::new_v4()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|_| AppError::SecretStoreUnavailable)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(AppError::SecretStoreUnavailable);
    }
    fs::rename(&temporary, path).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        AppError::SecretStoreUnavailable
    })?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| AppError::SecretStoreUnavailable)?;
    Ok(())
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

    fn temp_secret_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("codex-relay-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn local_vault_round_trips_and_deletes_secrets() {
        let dir = temp_secret_dir("vault-roundtrip");
        let store = LocalEncryptedSecretStore::open(&dir).unwrap();
        store.set("profile:one", "secret-token").await.unwrap();
        assert_eq!(store.get("profile:one").await.unwrap(), "secret-token");
        assert_eq!(
            store
                .get_without_user_interaction("profile:one")
                .await
                .unwrap(),
            "secret-token"
        );
        store.delete("profile:one").await.unwrap();
        assert!(matches!(
            store.get("profile:one").await.unwrap_err(),
            AppError::NotFound
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn local_vault_never_writes_plaintext_secret_values() {
        let dir = temp_secret_dir("vault-redaction");
        let store = LocalEncryptedSecretStore::open(&dir).unwrap();
        store
            .set("profile:oauth", "at-secret-refresh-token")
            .await
            .unwrap();
        let key = fs::read(dir.join(LOCAL_KEY_FILE)).unwrap();
        let vault = fs::read_to_string(dir.join(LOCAL_VAULT_FILE)).unwrap();
        assert_eq!(key.len(), 32);
        assert!(!vault.contains("at-secret-refresh-token"));
        assert_eq!(
            LocalEncryptedSecretStore::open(&dir)
                .unwrap()
                .get("profile:oauth")
                .await
                .unwrap(),
            "at-secret-refresh-token"
        );
        let _ = fs::remove_dir_all(dir);
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
