use std::env;
use std::time::Duration;

use crate::model::SearchMode;

/// Controls the Cache-Control visibility sent on search responses.
/// `Public` lets Bunny CDN (and any shared cache) serve hits without touching
/// the container at all — the cheapest and fastest path. `Private` restricts
/// caching to the end client. `Off` disables downstream caching entirely
/// (the in-process cache still applies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdnCacheMode {
    Public,
    Private,
    Off,
}

impl CdnCacheMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "public" => Ok(Self::Public),
            "private" => Ok(Self::Private),
            "off" | "no-store" => Ok(Self::Off),
            other => Err(format!(
                "invalid CDN_CACHE '{other}'; expected public, private, or off"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    pub parallel_api_key: String,
    pub parallel_base_url: String,
    pub default_mode: SearchMode,
    pub cache_ttl: Duration,
    pub cache_ttl_volatile: Duration,
    pub cache_max_bytes: u64,
    pub search_api_key: Option<String>,
    pub request_timeout: Duration,
    pub cdn_cache: CdnCacheMode,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port = env::var("PORT").unwrap_or_else(|_| "8080".into());
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let parallel_api_key =
            env::var("PARALLEL_API_KEY").map_err(|_| "PARALLEL_API_KEY is required".to_string())?;
        if parallel_api_key.is_empty() {
            return Err("PARALLEL_API_KEY is required".into());
        }

        let default_mode = env::var("DEFAULT_MODE")
            .ok()
            .as_deref()
            .map(SearchMode::parse)
            .transpose()?
            .unwrap_or(SearchMode::Turbo);

        let cache_ttl = Duration::from_secs(parse_u64_env("CACHE_TTL_SECS", 300)?);
        // Time-sensitive queries (weather, prices, scores…) must not live as
        // long as evergreen ones, whatever the main TTL is cranked up to.
        let cache_ttl_volatile =
            Duration::from_secs(parse_u64_env("CACHE_TTL_VOLATILE_SECS", 120)?).min(cache_ttl);
        let cache_max_bytes = parse_u64_env("CACHE_MAX_BYTES", 256 * 1024 * 1024)?;
        let request_timeout = Duration::from_secs(parse_u64_env("REQUEST_TIMEOUT_SECS", 30)?);
        let search_api_key = env::var("SEARCH_API_KEY").ok().filter(|s| !s.is_empty());

        // With no auth key the responses are anonymous and safe for shared
        // caches; with a key, default to private so the CDN doesn't hand out
        // authenticated results — override with CDN_CACHE=public once edge
        // auth (e.g. Bunny token auth / edge rules) is in place.
        let cdn_cache = match env::var("CDN_CACHE") {
            Ok(value) => CdnCacheMode::parse(&value)?,
            Err(_) if search_api_key.is_some() => CdnCacheMode::Private,
            Err(_) => CdnCacheMode::Public,
        };

        Ok(Self {
            bind: format!("{host}:{port}"),
            parallel_api_key,
            parallel_base_url: env::var("PARALLEL_BASE_URL")
                .unwrap_or_else(|_| "https://api.parallel.ai".into()),
            default_mode,
            cache_ttl,
            cache_ttl_volatile,
            cache_max_bytes,
            search_api_key,
            request_timeout,
            cdn_cache,
        })
    }
}

fn parse_u64_env(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("{name} must be a positive integer")),
        Err(_) => Ok(default),
    }
}
