//! Short-lived, single-use values keyed by a random nonce, kept in memory: the
//! authorization codes and the consent requests awaiting a login. Losing them on a
//! restart only costs an in-flight login a retry. Bounded, because both are
//! created by unauthenticated requests.
use std::collections::HashMap;
use std::time::Duration;

use tokio::{sync::Mutex, time::Instant};

use super::secret::random_token;

pub struct Pending<T> {
    ttl: Duration,
    cap: usize,
    entries: Mutex<HashMap<String, (T, Instant)>>,
}

impl<T> std::fmt::Debug for Pending<T> {
    // Keys are live secrets (codes, consent nonces): never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("ttl", &self.ttl)
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl<T> Pending<T> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Store `value` under a fresh random key and return the key, or `None` when
    /// `cap` live entries already exist. Expired entries are swept first, so an
    /// abandoned value never outlives its TTL in memory.
    pub async fn insert(&self, value: T) -> Option<String> {
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        entries.retain(|_, (_, expires)| *expires > now);
        if entries.len() >= self.cap {
            return None;
        }
        let key = random_token();
        entries.insert(key.clone(), (value, now + self.ttl));
        Some(key)
    }

    /// Remove and return the value under `key` if it exists and is unexpired. A
    /// key works at most once, whatever the outcome.
    pub async fn take(&self, key: &str) -> Option<T> {
        let (value, expires) = self.entries.lock().await.remove(key)?;
        (expires > Instant::now()).then_some(value)
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_value_is_taken_once() {
        let pending = Pending::new(Duration::from_secs(60), 10);
        let key = pending.insert(7).await.unwrap();
        assert_eq!(pending.take(&key).await, Some(7));
        assert_eq!(pending.take(&key).await, None);
        assert_eq!(pending.take("never-issued").await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_value_is_refused_and_swept() {
        let pending = Pending::new(Duration::from_secs(60), 10);
        let stale = pending.insert(1).await.unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;
        let fresh = pending.insert(2).await.unwrap();
        assert_eq!(pending.len().await, 1, "the stale entry was swept");
        assert_eq!(pending.take(&stale).await, None);
        assert_eq!(pending.take(&fresh).await, Some(2));
    }

    #[tokio::test]
    async fn insert_refuses_past_the_cap() {
        let pending = Pending::new(Duration::from_secs(60), 2);
        assert!(pending.insert(1).await.is_some());
        assert!(pending.insert(2).await.is_some());
        assert!(pending.insert(3).await.is_none());
    }
}
