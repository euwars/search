use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use salvo::http::HeaderValue;
use salvo::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cache::{CachedSearch, SearchCache};
use crate::config::{CdnCacheMode, Config};
use crate::model::{
    AdvancedSearchSettings, ExcerptSettings, FetchPolicy, SearchMode, SearchRequest, SourcePolicy,
};
use crate::parallel::{ParallelClient, ParallelError};

#[derive(Default)]
pub struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub upstream_errors: AtomicU64,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub parallel: ParallelClient,
    pub cache: SearchCache,
    pub stats: Arc<Stats>,
}

/// Replaces salvo's default error pages (404/405/500…) with an empty body:
/// no framework fingerprint, no wasted egress. Routing errors are
/// deterministic per URL, so let the CDN absorb bot scans and broken links;
/// anything else must not stick in a cache.
#[handler]
pub async fn empty_error(res: &mut Response, ctrl: &mut FlowCtrl) {
    let cache = match res.status_code {
        Some(StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED) => "public, max-age=3600",
        _ => "no-store",
    };
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    res.write_body("").ok();
    ctrl.skip_rest();
}

#[handler]
pub async fn health(depot: &Depot, res: &mut Response) {
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let body = match depot.get_typed::<AppState>() {
        Ok(state) => {
            let hits = state.stats.hits.load(Ordering::Relaxed);
            let misses = state.stats.misses.load(Ordering::Relaxed);
            let total = hits + misses;
            json!({
                "ok": true,
                "cache": {
                    "entries": state.cache.entry_count(),
                    "bytes": state.cache.weighted_size(),
                    "hits": hits,
                    "misses": misses,
                    "upstream_errors": state.stats.upstream_errors.load(Ordering::Relaxed),
                    "hit_rate": if total == 0 { 0.0 } else { hits as f64 / total as f64 },
                },
            })
        }
        Err(_) => json!({ "ok": true }),
    };
    res.render(Json(body));
}

#[handler]
pub async fn require_key(
    req: &mut Request,
    depot: &Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let Ok(state) = depot.get_typed::<AppState>() else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(json!({ "error": "missing app state" })));
        ctrl.skip_rest();
        return;
    };
    let Some(expected) = state.config.search_api_key.as_deref() else {
        return;
    };
    let provided = req
        .header::<String>("x-api-key")
        .or_else(|| req.query::<String>("api_key"));
    if provided.as_deref() != Some(expected) {
        // no-store is load-bearing: the key travels in a header, so a shared
        // cache would serve this 401 to keyed requests for the same URL.
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res.status_code(StatusCode::UNAUTHORIZED);
        res.render(Json(json!({ "error": "invalid or missing x-api-key" })));
        ctrl.skip_rest();
    }
}

#[handler]
pub async fn search(req: &mut Request, depot: &Depot, res: &mut Response) {
    let Ok(state) = depot.get_typed::<AppState>() else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(json!({ "error": "missing app state" })));
        return;
    };

    let mut search_req = match parse_search_request(req).await {
        Ok(parsed) => parsed,
        Err(message) => {
            render_bad_request(res, Json(json!({ "error": message })));
            return;
        }
    };
    search_req.normalize_for_upstream(state.config.default_mode);
    if search_req.search_queries.is_empty() {
        render_bad_request(
            res,
            Json(json!({
                "error": "search_queries is required (or provide objective)"
            })),
        );
        return;
    }

    let key = search_req.cache_key();
    let parallel = state.parallel.clone();
    let if_none_match = req.headers().get(IF_NONE_MATCH).cloned();

    match state
        .cache
        .get_or_fetch(
            key.clone(),
            || async move { parallel.search(&search_req).await },
        )
        .await
    {
        Ok((entry, hit)) => {
            let counter = if hit {
                &state.stats.hits
            } else {
                &state.stats.misses
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(cache_key = %key, hit, "served search");
            apply_cache_headers(res, state, &entry, hit, &key);

            let revalidated = if_none_match
                .as_ref()
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(',').any(|tag| tag.trim() == entry.etag));
            if revalidated {
                res.status_code(StatusCode::NOT_MODIFIED);
                return;
            }
            res.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            res.write_body(entry.body.clone()).ok();
        }
        Err(err) => {
            state.stats.upstream_errors.fetch_add(1, Ordering::Relaxed);
            render_parallel_error(res, err.as_ref());
        }
    }
}

/// A given URL always fails validation the same way, so let caches hold the
/// 400 briefly instead of re-asking the origin.
fn render_bad_request(res: &mut Response, body: Json<Value>) {
    res.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(body);
}

fn apply_cache_headers(
    res: &mut Response,
    state: &AppState,
    entry: &CachedSearch,
    hit: bool,
    key: &str,
) {
    let ttl = state.cache.ttl().as_secs();
    let value = match state.config.cdn_cache {
        CdnCacheMode::Public => {
            format!("public, max-age={ttl}, stale-while-revalidate={ttl}, stale-if-error=3600")
        }
        CdnCacheMode::Private => format!("private, max-age={ttl}"),
        CdnCacheMode::Off => "no-store".to_string(),
    };
    if let Ok(header) = HeaderValue::from_str(&value) {
        res.headers_mut().insert(CACHE_CONTROL, header);
    }
    if let Ok(header) = HeaderValue::from_str(&entry.etag) {
        res.headers_mut().insert(ETAG, header);
    }
    res.headers_mut().insert(
        "x-cache",
        HeaderValue::from_static(if hit { "HIT" } else { "MISS" }),
    );
    if let Ok(header) = HeaderValue::from_str(&key[..key.len().min(16)]) {
        res.headers_mut().insert("x-cache-key", header);
    }
}

fn render_parallel_error(res: &mut Response, err: &ParallelError) {
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    match err {
        ParallelError::Upstream { status, body } => {
            res.status_code(*status);
            if let Ok(value) = serde_json::from_str::<Value>(body) {
                res.render(Json(value));
            } else {
                res.render(Json(json!({ "error": body })));
            }
        }
        ParallelError::Transport(err) => {
            tracing::error!(error = %err, "parallel transport error");
            res.status_code(StatusCode::BAD_GATEWAY);
            res.render(Json(json!({ "error": "search origin unavailable" })));
        }
    }
}

#[derive(Default, Deserialize)]
struct SearchQuery {
    objective: Option<String>,
    #[serde(default, alias = "q")]
    search_queries: Vec<String>,
    mode: Option<String>,
    max_chars_total: Option<i64>,
    session_id: Option<String>,
    client_model: Option<String>,
    location: Option<String>,
    max_results: Option<i64>,
    max_chars_per_result: Option<i64>,
    max_age_seconds: Option<i64>,
    after_date: Option<String>,
    #[serde(default)]
    include_domains: Vec<String>,
    #[serde(default)]
    exclude_domains: Vec<String>,
}

async fn parse_search_request(req: &mut Request) -> Result<SearchRequest, String> {
    if req.method() == salvo::http::Method::GET {
        return parse_get_request(req);
    }

    req.parse_json::<SearchRequest>()
        .await
        .map_err(|err| format!("invalid search request: {err}"))
}

fn parse_get_request(req: &mut Request) -> Result<SearchRequest, String> {
    let query = req
        .parse_queries::<SearchQuery>()
        .map_err(|err| format!("invalid query string: {err}"))?;

    let mut search_queries = query.search_queries;
    if search_queries.is_empty() {
        search_queries.extend(repeated_query(req, "q"));
        search_queries.extend(repeated_query(req, "search_queries"));
    }

    let include_domains = if query.include_domains.is_empty() {
        repeated_query(req, "include_domains")
    } else {
        query.include_domains
    };
    let exclude_domains = if query.exclude_domains.is_empty() {
        repeated_query(req, "exclude_domains")
    } else {
        query.exclude_domains
    };

    let mode = query.mode.as_deref().map(SearchMode::parse).transpose()?;

    let source_policy =
        if include_domains.is_empty() && exclude_domains.is_empty() && query.after_date.is_none() {
            None
        } else {
            Some(SourcePolicy {
                include_domains,
                exclude_domains,
                after_date: query.after_date,
            })
        };

    let fetch_policy = query.max_age_seconds.map(|max_age_seconds| FetchPolicy {
        max_age_seconds: Some(max_age_seconds),
        ..FetchPolicy::default()
    });

    let excerpt_settings = query
        .max_chars_per_result
        .map(|max_chars_per_result| ExcerptSettings {
            max_chars_per_result: Some(max_chars_per_result),
        });

    let advanced_settings = if source_policy.is_none()
        && fetch_policy.is_none()
        && excerpt_settings.is_none()
        && query.location.is_none()
        && query.max_results.is_none()
    {
        None
    } else {
        Some(AdvancedSearchSettings {
            source_policy,
            fetch_policy,
            excerpt_settings,
            location: query.location,
            max_results: query.max_results,
        })
    };

    Ok(SearchRequest {
        objective: query.objective,
        search_queries,
        mode,
        max_chars_total: query.max_chars_total,
        session_id: query.session_id,
        client_model: query.client_model,
        advanced_settings,
    })
}

fn repeated_query(req: &Request, name: &str) -> Vec<String> {
    req.queries().get_vec(name).cloned().unwrap_or_default()
}
