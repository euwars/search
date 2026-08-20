use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Turbo,
    Fast,
    Basic,
    #[default]
    Advanced,
}

impl SearchMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "turbo" => Ok(Self::Turbo),
            "fast" => Ok(Self::Fast),
            "basic" => Ok(Self::Basic),
            "advanced" => Ok(Self::Advanced),
            other => Err(format!(
                "invalid mode '{other}'; expected turbo, fast, basic, or advanced"
            )),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    pub search_queries: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SearchMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars_total: Option<i64>,
    #[serde(skip_serializing, default)]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced_settings: Option<AdvancedSearchSettings>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AdvancedSearchSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_policy: Option<SourcePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_policy: Option<FetchPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt_settings: Option<ExcerptSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourcePolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_date: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache_fallback: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExcerptSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars_per_result: Option<i64>,
}

impl SearchRequest {
    pub fn normalize_for_upstream(&mut self, default_mode: SearchMode) {
        if let Some(objective) = &mut self.objective {
            let trimmed = collapse_ws(objective);
            if trimmed.is_empty() {
                self.objective = None;
            } else {
                *objective = trimmed;
            }
        }

        self.search_queries = self
            .search_queries
            .iter()
            .map(|q| collapse_ws(q))
            .filter(|q| !q.is_empty())
            .collect();

        if self.search_queries.is_empty()
            && let Some(objective) = &self.objective
        {
            self.search_queries.push(objective.clone());
        }

        if self.mode.is_none() {
            self.mode = Some(default_mode);
        }

        if let Some(settings) = &mut self.advanced_settings {
            if let Some(location) = &mut settings.location {
                *location = location.trim().to_ascii_lowercase();
                if location.is_empty() {
                    settings.location = None;
                }
            }
            if let Some(policy) = &mut settings.source_policy {
                normalize_domains(&mut policy.include_domains);
                normalize_domains(&mut policy.exclude_domains);
            }
        }
    }
}

/// `session_id` is absent: a shared cache must not join callers onto one
/// Parallel session, and keying on it would give every user a private entry.
#[derive(Serialize)]
struct CanonicalKey<'a> {
    objective: Option<String>,
    search_queries: Vec<String>,
    mode: Option<SearchMode>,
    max_chars_total: Option<i64>,
    client_model: Option<String>,
    location: Option<String>,
    max_results: Option<i64>,
    max_chars_per_result: Option<i64>,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
    after_date: Option<&'a str>,
    max_age_seconds: Option<i64>,
    timeout_seconds: Option<u64>,
    disable_cache_fallback: bool,
}

impl SearchRequest {
    pub fn cache_key(&self) -> String {
        let mut search_queries: Vec<String> = self
            .search_queries
            .iter()
            .map(|q| canonicalize_query(q))
            .filter(|q| !q.is_empty())
            .collect();
        search_queries.sort();
        search_queries.dedup();

        let settings = self.advanced_settings.as_ref();
        let source = settings.and_then(|s| s.source_policy.as_ref());
        let fetch = settings.and_then(|s| s.fetch_policy.as_ref());
        let excerpts = settings.and_then(|s| s.excerpt_settings.as_ref());

        let mut include_domains = source
            .map(|s| s.include_domains.clone())
            .unwrap_or_default();
        let mut exclude_domains = source
            .map(|s| s.exclude_domains.clone())
            .unwrap_or_default();
        normalize_domains(&mut include_domains);
        normalize_domains(&mut exclude_domains);

        let canonical = CanonicalKey {
            objective: self
                .objective
                .as_deref()
                .map(|o| collapse_ws(o).to_ascii_lowercase())
                .filter(|o| !o.is_empty()),
            search_queries,
            mode: self.mode,
            max_chars_total: self.max_chars_total,
            client_model: self
                .client_model
                .as_deref()
                .map(|m| collapse_ws(m).to_ascii_lowercase())
                .filter(|m| !m.is_empty()),
            location: settings
                .and_then(|s| s.location.as_deref())
                .map(|l| l.trim().to_ascii_lowercase())
                .filter(|l| !l.is_empty()),
            max_results: settings.and_then(|s| s.max_results),
            max_chars_per_result: excerpts.and_then(|e| e.max_chars_per_result),
            include_domains,
            exclude_domains,
            after_date: source.and_then(|s| s.after_date.as_deref()),
            max_age_seconds: fetch.and_then(|f| f.max_age_seconds),
            timeout_seconds: fetch.and_then(|f| f.timeout_seconds).map(|s| s.to_bits()),
            disable_cache_fallback: fetch.map(|f| f.disable_cache_fallback).unwrap_or(false),
        };

        let bytes = serde_json::to_vec(&canonical).expect("canonical key is serializable");
        blake3::hash(&bytes).to_hex().to_string()
    }
}

pub fn collapse_ws(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keyword bag for cache keys only: case-insensitive, word-order independent.
/// Not used on `objective` (natural language) or on the upstream Parallel payload.
fn canonicalize_query(value: &str) -> String {
    let mut tokens: Vec<String> = value
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.sort();
    tokens.join(" ")
}

/// Public search body: `results`, plus `warnings` only when non-empty.
/// Drops Parallel `search_id`, `session_id`, and `usage`.
pub fn project_search_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|err| format!("invalid JSON from Parallel: {err}"))?;
    let Some(obj) = value.as_object() else {
        return Err("invalid JSON from Parallel: expected object".into());
    };
    let results = match obj.get("results") {
        Some(serde_json::Value::Array(items)) => serde_json::Value::Array(items.clone()),
        Some(_) => return Err("invalid JSON from Parallel: results must be an array".into()),
        None => serde_json::json!([]),
    };

    let mut out = serde_json::Map::new();
    out.insert("results".into(), results);
    if let Some(warnings) = obj.get("warnings")
        && let serde_json::Value::Array(items) = warnings
        && !items.is_empty()
    {
        out.insert("warnings".into(), serde_json::Value::Array(items.clone()));
    }

    serde_json::to_vec(&serde_json::Value::Object(out))
        .map_err(|err| format!("failed to serialize search response: {err}"))
}

fn normalize_domains(domains: &mut Vec<String>) {
    for domain in domains.iter_mut() {
        *domain = domain.trim().to_ascii_lowercase();
    }
    domains.retain(|d| !d.is_empty());
    domains.sort();
    domains.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(queries: &[&str]) -> SearchRequest {
        SearchRequest {
            search_queries: queries.iter().map(|q| (*q).to_string()).collect(),
            mode: Some(SearchMode::Turbo),
            ..SearchRequest::default()
        }
    }

    #[test]
    fn cache_key_ignores_query_order_and_case() {
        let a = req(&["Nike Shoes", "stock price"]);
        let b = req(&["stock   price", "nike shoes"]);
        assert_eq!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn cache_key_ignores_token_order_in_queries() {
        let a = req(&["apple stock"]);
        let b = req(&["Stock   Apple"]);
        assert_eq!(a.cache_key(), b.cache_key());
        assert_eq!(canonicalize_query("stock apple"), "apple stock");
    }

    #[test]
    fn cache_key_keeps_objective_word_order() {
        let mut a = req(&["apple"]);
        let mut b = req(&["apple"]);
        a.objective = Some("latest apple stock price".into());
        b.objective = Some("stock price latest apple".into());
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn upstream_payload_keeps_original_query_order() {
        let mut search = req(&["stock apple"]);
        search.normalize_for_upstream(SearchMode::Turbo);
        assert_eq!(search.search_queries, vec!["stock apple"]);
    }

    #[test]
    fn cache_key_ignores_session_id() {
        let mut a = req(&["nike"]);
        let mut b = req(&["nike"]);
        a.session_id = Some("user-1".into());
        b.session_id = Some("user-2".into());
        assert_eq!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn cache_key_changes_with_mode() {
        let mut a = req(&["nike"]);
        let mut b = req(&["nike"]);
        a.mode = Some(SearchMode::Turbo);
        b.mode = Some(SearchMode::Advanced);
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn cache_key_changes_with_location() {
        let mut a = req(&["ev incentives"]);
        let mut b = req(&["ev incentives"]);
        a.advanced_settings = Some(AdvancedSearchSettings {
            location: Some("gb".into()),
            ..AdvancedSearchSettings::default()
        });
        b.advanced_settings = Some(AdvancedSearchSettings {
            location: Some("us".into()),
            ..AdvancedSearchSettings::default()
        });
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn empty_objective_falls_back_to_queries() {
        let mut req = SearchRequest {
            objective: Some("  latest nvidia price  ".into()),
            search_queries: vec![],
            ..SearchRequest::default()
        };
        req.normalize_for_upstream(SearchMode::Turbo);
        assert_eq!(req.search_queries, vec!["latest nvidia price"]);
        assert_eq!(req.mode, Some(SearchMode::Turbo));
    }

    #[test]
    fn session_id_is_not_sent_upstream() {
        let req = SearchRequest {
            search_queries: vec!["apple".into()],
            session_id: Some("session_secret".into()),
            mode: Some(SearchMode::Turbo),
            ..SearchRequest::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("session_id").is_none());
        assert_eq!(json["search_queries"][0], "apple");
    }

    #[test]
    fn project_search_response_keeps_results_and_drops_bookkeeping() {
        let raw = serde_json::json!({
            "search_id": "search_abc",
            "session_id": "session_abc",
            "usage": [{ "name": "sku_search", "count": 1 }],
            "warnings": null,
            "results": [{
                "url": "https://example.com",
                "title": "Example",
                "publish_date": "2026-08-20",
                "excerpts": ["hello"]
            }]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&project_search_response(raw.to_string().as_bytes()).unwrap())
                .unwrap();
        assert_eq!(out["results"][0]["url"], "https://example.com");
        assert!(out.get("search_id").is_none());
        assert!(out.get("session_id").is_none());
        assert!(out.get("usage").is_none());
        assert!(out.get("warnings").is_none());
    }

    #[test]
    fn project_search_response_keeps_nonempty_warnings() {
        let raw = serde_json::json!({
            "results": [],
            "warnings": [{ "type": "input_validation_warning", "message": "max_results capped" }]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&project_search_response(raw.to_string().as_bytes()).unwrap())
                .unwrap();
        assert_eq!(out["warnings"][0]["message"], "max_results capped");
    }
}
