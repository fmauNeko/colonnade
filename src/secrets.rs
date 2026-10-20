//! Secret store and OS keychain integration.
//!
//! Keychain calls block, so callers run them through `cx.background_spawn`.

use std::sync::Arc;
#[cfg(test)]
use std::{collections::HashMap, sync::Mutex};

use gpui_kit::{App, Global};
use keyring::v1::{Entry, Error as KeyringError};
use thiserror::Error;

use crate::catalog::model::ConnectionId;
use crate::db::model::Secret;

/// Keychain service name (the application id).
pub const SERVICE: &str = "ovh.dissidence.colonnade";

/// Whether a store can currently be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// The store works.
    Available,
    /// The store cannot be used; the payload says why.
    Unavailable(String),
}

/// A secret store failure. Never carries the secret itself.
#[derive(Debug, Error)]
pub enum SecretError {
    /// The platform keychain reported an error.
    #[error("keychain error: {0}")]
    Keychain(#[from] KeyringError),
    /// A scripted failure (tests).
    #[error("secret store failure: {0}")]
    Failed(String),
}

/// Storage of connection passwords, keyed by connection id.
///
/// Implementations may block: call them from `cx.background_spawn`.
pub trait SecretStore: Send + Sync {
    /// Whether the store is usable.
    fn availability(&self) -> Availability;

    /// The stored secret, if any.
    ///
    /// # Errors
    /// Returns [`SecretError`] when the store fails.
    fn get(&self, id: ConnectionId) -> Result<Option<Secret>, SecretError>;

    /// Stores (replaces) the secret.
    ///
    /// # Errors
    /// Returns [`SecretError`] when the store fails.
    fn set(&self, id: ConnectionId, secret: &Secret) -> Result<(), SecretError>;

    /// Deletes the secret; a missing entry is `Ok`.
    ///
    /// # Errors
    /// Returns [`SecretError`] when the store fails.
    fn delete(&self, id: ConnectionId) -> Result<(), SecretError>;
}

/// The OS keychain, through `keyring`.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringSecretStore;

fn entry(id: ConnectionId) -> Result<Entry, SecretError> {
    Ok(Entry::new(SERVICE, &id.as_uuid().to_string())?)
}

impl SecretStore for KeyringSecretStore {
    fn availability(&self) -> Availability {
        match Entry::store_status() {
            Ok(()) => Availability::Available,
            Err(error) => Availability::Unavailable(error.to_string()),
        }
    }

    fn get(&self, id: ConnectionId) -> Result<Option<Secret>, SecretError> {
        match entry(id)?.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn set(&self, id: ConnectionId, secret: &Secret) -> Result<(), SecretError> {
        Ok(entry(id)?.set_password(secret.expose())?)
    }

    fn delete(&self, id: ConnectionId) -> Result<(), SecretError> {
        match entry(id)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// In-memory store with scriptable failure and availability (tests).
#[cfg(test)]
#[derive(Debug)]
pub struct MemorySecretStore {
    entries: Mutex<HashMap<ConnectionId, Secret>>,
    failure: Mutex<Option<String>>,
    availability: Mutex<Availability>,
}

#[cfg(test)]
impl Default for MemorySecretStore {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            failure: Mutex::default(),
            availability: Mutex::new(Availability::Available),
        }
    }
}

#[cfg(test)]
impl MemorySecretStore {
    /// Makes every following get/set/delete fail with `message`.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned.
    pub fn fail_with(&self, message: &str) {
        *self.failure.lock().unwrap() = Some(message.to_owned());
    }

    /// Scripts the reported availability.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned.
    pub fn set_availability(&self, availability: Availability) {
        *self.availability.lock().unwrap() = availability;
    }

    fn check(&self) -> Result<(), SecretError> {
        match self.failure.lock().unwrap().clone() {
            Some(message) => Err(SecretError::Failed(message)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
impl SecretStore for MemorySecretStore {
    fn availability(&self) -> Availability {
        self.availability.lock().unwrap().clone()
    }

    fn get(&self, id: ConnectionId) -> Result<Option<Secret>, SecretError> {
        self.check()?;
        Ok(self.entries.lock().unwrap().get(&id).cloned())
    }

    fn set(&self, id: ConnectionId, secret: &Secret) -> Result<(), SecretError> {
        self.check()?;
        self.entries.lock().unwrap().insert(id, secret.clone());
        Ok(())
    }

    fn delete(&self, id: ConnectionId) -> Result<(), SecretError> {
        self.check()?;
        self.entries.lock().unwrap().remove(&id);
        Ok(())
    }
}

/// The application's secret store, as a GPUI global.
pub struct Secrets(pub Arc<dyn SecretStore>);

impl Global for Secrets {}

/// Installs `store` as the application's secret store.
pub fn install(cx: &mut App, store: Arc<dyn SecretStore>) {
    cx.set_global(Secrets(store));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Availability, KeyringSecretStore, MemorySecretStore, SecretStore as _};
    use crate::catalog::model::ConnectionId;
    use crate::db::model::Secret;

    #[test]
    fn memory_store_round_trips() {
        // Given: an empty memory store.
        let store = MemorySecretStore::default();
        let id = ConnectionId::new();
        // When: a secret is set, read, then deleted.
        store.set(id, &Secret::new("hunter2".into())).unwrap();
        let read = store.get(id).unwrap();
        store.delete(id).unwrap();
        // Then: it is read back, and gone after deletion.
        assert_eq!(read, Some(Secret::new("hunter2".into())));
        assert_eq!(store.get(id).unwrap(), None);
    }

    #[test]
    fn deleting_a_missing_entry_is_ok() {
        // Given: an empty store.
        let store = MemorySecretStore::default();
        // When: deleting an unknown id.
        let result = store.delete(ConnectionId::new());
        // Then: it succeeds.
        assert!(result.is_ok());
    }

    #[test]
    fn store_failure_is_reported() {
        // Given: a store scripted to fail.
        let store = MemorySecretStore::default();
        store.fail_with("locked");
        let id = ConnectionId::new();
        // When: using it.
        let set = store.set(id, &Secret::new("x".into()));
        let get = store.get(id);
        // Then: every operation reports the failure.
        assert!(set.unwrap_err().to_string().contains("locked"));
        assert!(get.is_err());
        assert!(store.delete(id).is_err());
    }

    #[test]
    fn availability_is_scriptable() {
        // Given: a store scripted as unavailable.
        let store = MemorySecretStore::default();
        assert_eq!(store.availability(), Availability::Available);
        store.set_availability(Availability::Unavailable("no keychain".into()));
        // When / Then: the availability is reported.
        assert_eq!(
            store.availability(),
            Availability::Unavailable("no keychain".into())
        );
    }

    #[test]
    fn install_registers_the_global_store() {
        // Given: an app context.
        let cx = gpui_kit::TestAppContext::single();
        let store = Arc::new(MemorySecretStore::default());
        // When: installing the store.
        cx.update(|cx| super::install(cx, store));
        // Then: it is reachable through the global.
        cx.update(|cx| {
            let id = ConnectionId::new();
            assert_eq!(cx.global::<super::Secrets>().0.get(id).unwrap(), None);
        });
    }

    #[test]
    #[ignore = "Tier C: touches the real OS keychain"]
    fn keychain_round_trip() {
        // Given: a random connection id on the real keychain.
        let store = KeyringSecretStore;
        let id = ConnectionId::new();
        // When: set, get, delete.
        store.set(id, &Secret::new("round-trip".into())).unwrap();
        let read = store.get(id).unwrap();
        store.delete(id).unwrap();
        // Then: the value round-trips and is gone afterwards.
        assert_eq!(read, Some(Secret::new("round-trip".into())));
        assert_eq!(store.get(id).unwrap(), None);
    }
}
