use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use moka::future::Cache;

pub struct CachedSearch {
    pub body: Bytes,
    pub etag: String,
}

impl CachedSearch {
    fn new(body: Bytes) -> Self {
        let etag = format!("\"{}\"", &blake3::hash(&body).to_hex()[..16]);
        Self { body, etag }
    }

    fn weight(&self, key: &str) -> u32 {
        (self.body.len() + self.etag.len() + key.len())
            .try_into()
            .unwrap_or(u32::MAX)
    }
}

#[derive(Clone)]
pub struct SearchCache {
    inner: Cache<String, Arc<CachedSearch>>,
    ttl: Duration,
}

impl SearchCache {
    pub fn new(max_bytes: u64, ttl: Duration) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|key: &String, value: &Arc<CachedSearch>| value.weight(key))
                .time_to_live(ttl)
                .build(),
            ttl,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
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
                Ok(Arc::new(CachedSearch::new(fetch().await?)))
            })
            .await?;

        Ok((value, !fetched.load(Ordering::Relaxed)))
    }
}
