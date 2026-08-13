use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::VecDeque;

/// Rotating key chain for a single provider.
/// Rotates to the next key on failure, wraps around.
#[derive(Debug)]
pub struct KeyChain {
    keys: Vec<String>,
    /// Index of the current active key
    current: Arc<Mutex<usize>>,
    /// Keys known to be bad (e.g., 401). They get skipped until reset.
    bad_keys: Arc<Mutex<VecDeque<usize>>>,
}

impl KeyChain {
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys,
            current: Arc::new(Mutex::new(0)),
            bad_keys: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Get the current active key, or None if no keys available.
    pub async fn current_key(&self) -> Option<String> {
        if self.keys.is_empty() {
            return None;
        }
        let current = self.current.lock().await;
        let bad = self.bad_keys.lock().await;

        // Find next non-bad key starting from current
        for offset in 0..self.keys.len() {
            let idx = (*current + offset) % self.keys.len();
            if !bad.contains(&idx) {
                return Some(self.keys[idx].clone());
            }
        }
        None
    }

    /// Mark the current key as bad (e.g., on 401). Rotates to next.
    pub async fn mark_current_bad(&self) {
        let mut current = self.current.lock().await;
        let mut bad = self.bad_keys.lock().await;

        if !bad.contains(&*current) {
            tracing::warn!(
                "marking key index {} as bad (total bad: {})",
                *current,
                bad.len() + 1
            );
            bad.push_back(*current);
        }

        // Advance to next key
        *current = (*current + 1) % self.keys.len();

        // If all keys are bad, reset (give them another chance on next cooldown cycle)
        if bad.len() >= self.keys.len() {
            tracing::warn!("all keys marked bad, resetting key chain");
            bad.clear();
        }
    }

    /// Reset bad keys (called on breaker cooldown reset).
    pub async fn reset(&self) {
        let mut bad = self.bad_keys.lock().await;
        bad.clear();
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rotation() {
        let chain = KeyChain::new(vec!["key1".into(), "key2".into(), "key3".into()]);

        assert_eq!(chain.current_key().await.as_deref(), Some("key1"));
        chain.mark_current_bad().await;
        assert_eq!(chain.current_key().await.as_deref(), Some("key2"));
        chain.mark_current_bad().await;
        assert_eq!(chain.current_key().await.as_deref(), Some("key3"));
    }

    #[tokio::test]
    async fn test_all_bad_resets() {
        let chain = KeyChain::new(vec!["key1".into(), "key2".into()]);

        chain.mark_current_bad().await;
        chain.mark_current_bad().await;
        // All keys bad → reset → should have a working key again
        assert!(chain.current_key().await.is_some());
    }

    #[tokio::test]
    async fn test_empty_chain() {
        let chain = KeyChain::new(vec![]);
        assert!(chain.is_empty());
        assert!(chain.current_key().await.is_none());
    }
}
