# search — cached search layer

A small, fast caching proxy in front of the [Parallel.ai Search API](https://parallel.ai), built to sit behind Bunny CDN on **Bunny Magic Containers**. Every upstream search call costs money; this service makes sure you pay for each distinct query once per TTL, no matter how many users ask it.

## How it saves money and latency

Three cache layers, cheapest first:

1. **Bunny CDN edge** (GET only) — hits never reach the container. Billed at CDN bandwidth rates, ~ms latency from the nearest PoP.
2. **In-process cache** (per replica) — normalized, deduplicated, byte-capped. Hits are served as pre-serialized bytes: no JSON parsing or re-serialization on the hot path.
3. **Parallel.ai** — only on a genuine miss. Concurrent identical misses are coalesced into a single upstream call (singleflight), so a traffic spike on one hot query costs one API call, not thousands.

Cache keys are canonicalized so near-identical requests share entries: query case, word order, whitespace, duplicate queries, and domain-list order are all normalized, and `session_id` is deliberately **excluded** from the key (it's forwarded upstream for observability but doesn't change results — keying on it would give every user a private cache).

Also on by default: gzip/brotli response compression, `ETag`/`If-None-Match` 304 revalidation, hit-rate stats at `/health`, and graceful drain on SIGTERM.

## API

```
GET  /v1/search?q=nike+shoes&q=stock+price&mode=turbo
POST /v1/search   { "objective": "...", "search_queries": ["..."], "mode": "turbo", ... }
GET  /health      → { ok, cache: { entries, bytes, hits, misses, upstream_errors, hit_rate } }
```

GET supports: `q`/`search_queries` (repeatable), `objective`, `mode` (turbo|fast|basic|advanced), `max_results`, `max_chars_total`, `max_chars_per_result`, `max_age_seconds`, `after_date`, `include_domains`/`exclude_domains` (repeatable), `location`, `session_id`, `client_model`. POST takes the native Parallel request body. Responses carry `x-cache: HIT|MISS` and `x-cache-key`.

**Prefer GET from clients** — only GET responses are CDN-cacheable.

## Configuration (env vars)

| Variable | Default | Notes |
|---|---|---|
| `PARALLEL_API_KEY` | *(required)* | Parallel.ai API key |
| `SEARCH_API_KEY` | *(unset)* | If set, clients must send `x-api-key` |
| `CACHE_TTL_SECS` | `300` | Cache TTL; the biggest cost lever (see below) |
| `CACHE_MAX_BYTES` | `268435456` (256 MiB) | In-process cache size cap, by response bytes |
| `CDN_CACHE` | auto | `public`, `private`, or `off`. Auto: `public` without `SEARCH_API_KEY`, `private` with it |
| `DEFAULT_MODE` | `turbo` | Mode when the client doesn't pick one |
| `REQUEST_TIMEOUT_SECS` | `30` | Upstream timeout |
| `TOKIO_WORKER_THREADS` | `4` | Keep small; Bunny hosts expose 32+ cores to containers |
| `PORT` / `HOST` | `8080` / `0.0.0.0` | Bunny does not inject a port; the image sets these |
| `PARALLEL_BASE_URL` | `https://api.parallel.ai` | |
| `RUST_LOG` | `info` | |

## Deploying to Bunny Magic Containers

Bunny only supports **linux/amd64** images from **Docker Hub or GHCR** (private repos: add an Image Registry with a read-only token in the Bunny dashboard first).

### 1. Build and push

```sh
docker build --platform linux/amd64 -t ghcr.io/YOUR_ORG/search-cache:v1 .
docker push ghcr.io/YOUR_ORG/search-cache:v1
```

The image is distroless, ~14 MB — replicas pull and start fast when autoscaling kicks in.

### 2. Create the app

Dashboard → Magic Containers → Deploy (or Terraform `bunnynet_compute_container_app`):

- **Image**: your pushed tag.
- **Env vars**: `PARALLEL_API_KEY` (note: Bunny has no separate secrets store — env vars are the mechanism), plus any overrides from the table above.
- **Regions**: start with 2–3 base regions near your users, or "Magic" AI placement. Region count multiplies your always-on cost — the CDN in front means you don't need many.
- **Scaling**: min 1–2, max ~5 per region (hard cap 10/region without a support ticket). There is **no scale-to-zero**; min replicas always run and always bill.
- **Health checks** (Container Settings → Monitoring): Startup + Readiness + Liveness, all HTTP GET `/health` on port 8080.

### 3. Endpoint + CDN caching (the important part)

Add a **CDN endpoint** with container port `8080`. You get an `mc-xxx.bunny.run` hostname backed by an auto-generated pull zone. Then configure that pull zone — **by default Bunny's Smart Cache will NOT cache `application/json`, regardless of Cache-Control**, so out of the box the CDN caches nothing:

1. **Edge Rule**: "Override Cache Time" (or "Set Cache TTL") for `/v1/search*` matching the origin TTL (e.g. 300 s). This forces JSON caching at the edge.
2. **Query String Sort**: enable — normalizes `?q=a&mode=x` vs `?mode=x&q=a` into one cache key.
3. **Stale Cache Delivery**: enable both "while updating" and "while origin offline". (Bunny ignores the RFC 5861 `stale-while-revalidate` header this service sends; these toggles are its equivalent.)
4. Leave sticky sessions off — replicas are stateless apart from their local cache.
5. `/health` sends `no-store` and stays uncached automatically.

### 4. Auth vs. CDN caching — pick one of three postures

| Posture | Setup | Trade-off |
|---|---|---|
| **Open edge** (cheapest) | No `SEARCH_API_KEY`; `CDN_CACHE=public` (auto) | Max CDN hit rate; endpoint is public — rely on CDN rate limiting / WAF |
| **Origin auth** | `SEARCH_API_KEY` set; `CDN_CACHE=private` (auto) | Every request hits a container; you lose L1 but keep L2 |
| **Edge auth** (best of both) | `SEARCH_API_KEY` + Bunny token auth or an Edge Rule that blocks requests missing your key header, then `CDN_CACHE=public` | Auth enforced at the PoP, cached responses still served from the edge |

### 5. Rolling updates from CI

```yaml
- uses: BunnyWay/actions/container-update-image@main
  with:
    app_id: ${{ vars.BUNNY_APP_ID }}
    api_key: ${{ secrets.BUNNY_API_KEY }}
    container: search
    image_tag: ${{ github.sha }}
```

Bunny gives containers a 30 s stop grace period; the service drains connections on SIGTERM well within it.

## Cost tuning at scale

- **TTL is the lever.** At 1M users the hit rate on popular queries is what determines your Parallel bill. Watch `hit_rate` in `/health`; every doubling of `CACHE_TTL_SECS` on a heavy-tail workload meaningfully cuts upstream calls. 300 s is a conservative start — search results tolerate 10–15 min staleness for most products.
- **Bunny bills CPU per second actually used and RAM hourly in 64 MB increments.** A replica of this service idles near zero CPU; RAM ≈ `CACHE_MAX_BYTES` + ~40 MB overhead. The 256 MiB default keeps a replica in the ~$1–1.50/month range before traffic; raise it in busy regions for hit rate rather than adding replicas.
- **Container egress through the CDN endpoint is billed as CDN bandwidth, not container egress** — another reason to keep clients on GET + edge caching.
- **`DEFAULT_MODE=turbo`** is the cheapest/fastest Parallel mode; clients can opt up per request.
- Misses that fail upstream are never cached, and errors return `no-store`.

## Local development

```sh
PARALLEL_API_KEY=sk-... cargo run
curl 'localhost:8080/v1/search?q=nike+shoes&mode=turbo'
cargo test
```
