use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use moka::Expiry;
use moka::future::Cache;

pub struct CachedSearch {
    pub body: Bytes,
    pub etag: String,
    pub ttl: Duration,
    created: Instant,
}

impl CachedSearch {
    fn new(body: Bytes, ttl: Duration) -> Self {
        let etag = format!("\"{}\"", &blake3::hash(&body).to_hex()[..16]);
        Self {
            body,
            etag,
            ttl,
            created: Instant::now(),
        }
    }

    /// Time left before this entry expires — what downstream caches may hold
    /// it for. Using the full TTL on every hit would stack origin and CDN
    /// lifetimes into up to double the intended staleness.
    pub fn remaining(&self) -> Duration {
        self.ttl.saturating_sub(self.created.elapsed())
    }

    fn weight(&self, key: &str) -> u32 {
        (self.body.len() + self.etag.len() + key.len())
            .try_into()
            .unwrap_or(u32::MAX)
    }
}

/// Each entry carries its own TTL (volatile queries expire sooner).
struct PerEntryTtl;

impl Expiry<String, Arc<CachedSearch>> for PerEntryTtl {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Arc<CachedSearch>,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

#[derive(Clone)]
pub struct SearchCache {
    inner: Cache<String, Arc<CachedSearch>>,
}

impl SearchCache {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|key: &String, value: &Arc<CachedSearch>| value.weight(key))
                .expire_after(PerEntryTtl)
                .build(),
        }
    }

    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    pub fn weighted_size(&self) -> u64 {
        self.inner.weighted_size()
    }

    pub async fn get_or_fetch<F, Fut, E>(
        &self,
        key: String,
        ttl: Duration,
        fetch: F,
    ) -> Result<(Arc<CachedSearch>, bool), Arc<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, E>>,
        E: Send + Sync + 'static,
    {
        if let Some(hit) = self.inner.get(&key).await {
            return Ok((hit, true));
        }

        let fetched = Arc::new(AtomicBool::new(false));
        let fetched_flag = fetched.clone();
        let value = self
            .inner
            .try_get_with(key, async move {
                fetched_flag.store(true, Ordering::Relaxed);
                Ok(Arc::new(CachedSearch::new(fetch().await?, ttl)))
            })
            .await?;

        Ok((value, !fetched.load(Ordering::Relaxed)))
    }
}
