//! Local handlers for AmpCode `webSearch2` and `extractWebPageContent` internal APIs.
//!
//! When `amp.web_tools.enabled = true`, these intercept the corresponding
//! `/api/internal` calls and serve them locally using Kagi search and
//! `agent-browser` CLI (Lightpanda engine), avoiding ampcode.com credit costs.
//!
//! Kagi HTML parsing is ported from OpenSetsuna's two-layer strategy:
//!   Layer A — Kagi structured selectors (`._0_SRI` result cards)
//!   Layer B — Generic anchor-driven fallback

use bytes::Bytes;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use crate::AppState;

/// User-Agent header for Kagi requests.
const KAGI_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Domains that appear as sub-links inside Kagi result cards, not real results.
const NOISE_DOMAINS: &[&str] = &["web.archive.org", "translate.kagi.com", "kagi.com"];

/// Kagi UI chrome text that leaks into generic extraction.
const NOISE_PHRASES: &[&str] = &[
    "来自该网站的更多结果",
    "从本网站删除结果",
    "用网站时光机打开页面",
    "kagi.com",
    "More results from",
    "Block this site",
];

// ── Helpers ─────────────────────────────────────────────────────────────

fn clean_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_noise_url(url_str: &str) -> bool {
    match rquest::Url::parse(url_str) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            NOISE_DOMAINS.contains(&host) || host.ends_with(".kagi.com")
        }
        Err(_) => true,
    }
}

fn canonicalize_url(input: &str) -> String {
    match rquest::Url::parse(input) {
        Ok(mut u) => {
            u.set_fragment(None);
            let remove_params: HashSet<&str> =
                ["utm_source", "utm_medium", "utm_campaign", "utm_content", "ref"]
                    .into_iter()
                    .collect();
            let pairs: Vec<(String, String)> = u
                .query_pairs()
                .filter(|(k, _)| !remove_params.contains(k.as_ref()))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if pairs.is_empty() {
                u.set_query(None);
            } else {
                u.query_pairs_mut().clear().extend_pairs(&pairs);
            }
            u.to_string()
        }
        Err(_) => input.to_string(),
    }
}

fn is_noise_text(text: &str) -> bool {
    NOISE_PHRASES.iter().any(|p| text.contains(p))
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
}

// ── Search result types ─────────────────────────────────────────────────

struct SearchResult {
    url: String,
    title: String,
    snippet: Option<String>,
}

// ── Layer A: Kagi structured selectors (._0_SRI result cards) ───────────

fn get_text_content(node: &tl::Node, parser: &tl::Parser) -> String {
    let mut text = String::new();
    collect_text(node, parser, &mut text);
    clean_text(&text)
}

fn collect_text(node: &tl::Node, parser: &tl::Parser, buf: &mut String) {
    match node {
        tl::Node::Raw(raw) => buf.push_str(raw.as_utf8_str().as_ref()),
        tl::Node::Tag(tag) => {
            for child in tag.children().top().iter() {
                if let Some(n) = child.get(parser) {
                    collect_text(n, parser, buf);
                }
            }
        }
        tl::Node::Comment(_) => {}
    }
}

fn parse_kagi_structured(dom: &tl::VDom) -> Vec<SearchResult> {
    let parser = dom.parser();
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    // Find all elements with class containing "_0_SRI" or "search-result"
    let cards = dom.query_selector("._0_SRI").into_iter().flatten();

    for card_handle in cards {
        let card_node = match card_handle.get(parser) {
            Some(n) => n,
            None => continue,
        };
        let card_tag = match card_node.as_tag() {
            Some(t) => t,
            None => continue,
        };

        // Find the title link: a._0_URL
        let title_link = card_tag
            .query_selector(parser, "a._0_URL")
            .into_iter()
            .flatten()
            .next();
        let title_link = match title_link {
            Some(h) => h,
            None => continue,
        };
        let title_node = match title_link.get(parser) {
            Some(n) => n,
            None => continue,
        };
        let title_tag = match title_node.as_tag() {
            Some(t) => t,
            None => continue,
        };

        let href = match title_tag.attributes().get("href").flatten() {
            Some(h) => html_decode(h.as_utf8_str().as_ref()),
            None => continue,
        };
        if is_noise_url(&href) {
            continue;
        }
        let canonical = canonicalize_url(&href);
        if seen.contains(&canonical) {
            continue;
        }

        let title_attr = title_tag
            .attributes()
            .get("title")
            .flatten()
            .map(|v| clean_text(v.as_utf8_str().as_ref()));
        let title_text = clean_text(&get_text_content(title_node, parser));
        let title = match (&title_attr, title_text.as_str()) {
            (Some(a), _) if a.len() >= 2 => a.clone(),
            (_, t) if t.len() >= 2 => t.to_string(),
            _ => continue,
        };

        // Snippet: .__sri-desc container
        let mut snippet: Option<String> = None;
        if let Some(desc_handle) = card_tag
            .query_selector(parser, ".__sri-desc")
            .into_iter()
            .flatten()
            .next()
        {
            if let Some(desc_node) = desc_handle.get(parser) {
                if let Some(desc_tag) = desc_node.as_tag() {
                    // Extract date prefix from __sri-time
                    let date_prefix = desc_tag
                        .query_selector(parser, ".__sri-time")
                        .into_iter()
                        .flatten()
                        .next()
                        .and_then(|h| h.get(parser))
                        .map(|n| clean_text(&get_text_content(n, parser)))
                        .unwrap_or_default();

                    let mut raw_text = clean_text(&get_text_content(desc_node, parser));
                    if !date_prefix.is_empty() && raw_text.starts_with(&date_prefix) {
                        raw_text = raw_text[date_prefix.len()..].trim().to_string();
                    }

                    if !date_prefix.is_empty() && !raw_text.is_empty() {
                        snippet = Some(format!("[{date_prefix}] {raw_text}"));
                    } else if !raw_text.is_empty() {
                        snippet = Some(raw_text);
                    }
                }
            }
        }

        seen.insert(canonical.clone());
        results.push(SearchResult {
            url: canonical,
            title,
            snippet,
        });
    }

    results
}

// ── Layer B: Generic anchor-driven fallback ─────────────────────────────

fn parse_generic_fallback(dom: &tl::VDom) -> Vec<SearchResult> {
    let parser = dom.parser();
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    let anchors = dom.query_selector("a[href]").into_iter().flatten();

    for a_handle in anchors {
        let a_node = match a_handle.get(parser) {
            Some(n) => n,
            None => continue,
        };
        let a_tag = match a_node.as_tag() {
            Some(t) => t,
            None => continue,
        };

        let href = match a_tag.attributes().get("href").flatten() {
            Some(h) => html_decode(h.as_utf8_str().as_ref()),
            None => continue,
        };

        // Resolve relative URLs against Kagi
        let resolved = match rquest::Url::parse(&href) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u.to_string(),
            _ => match rquest::Url::parse("https://kagi.com")
                .ok()
                .and_then(|base| base.join(&href).ok())
            {
                Some(u) if u.scheme() == "http" || u.scheme() == "https" => u.to_string(),
                _ => continue,
            },
        };

        if is_noise_url(&resolved) {
            continue;
        }
        let canonical = canonicalize_url(&resolved);
        if seen.contains(&canonical) {
            continue;
        }

        let title_attr = a_tag
            .attributes()
            .get("title")
            .flatten()
            .map(|v| clean_text(v.as_utf8_str().as_ref()));
        let title_text = clean_text(&get_text_content(a_node, parser));
        let title = match (&title_attr, title_text.as_str()) {
            (Some(a), _) if a.len() >= 2 => a.clone(),
            (_, t) if t.len() >= 2 => t.to_string(),
            _ => continue,
        };
        if is_noise_text(&title) {
            continue;
        }

        // Use surrounding text as snippet (from the anchor's inner HTML context)
        let snippet = {
            let full_text = get_text_content(a_node, parser);
            let remaining = clean_text(&full_text);
            if remaining.len() >= 20 && remaining.len() <= 500 && !is_noise_text(&remaining) {
                Some(remaining)
            } else {
                None
            }
        };

        seen.insert(canonical.clone());
        results.push(SearchResult {
            url: canonical,
            title,
            snippet,
        });
    }

    results
}

// ── Main parser: try structured first, fall back to generic ─────────────

fn parse_serp_html(html: &str) -> Vec<SearchResult> {
    let dom = tl::parse(html, tl::ParserOptions::default()).unwrap_or_else(|_| {
        tl::parse("", tl::ParserOptions::default()).expect("empty parse")
    });

    // Layer A: Kagi structured selectors
    let structured = parse_kagi_structured(&dom);
    if !structured.is_empty() {
        return structured;
    }

    // Layer B: generic anchor fallback
    parse_generic_fallback(&dom)
}

// ── webSearch2 ──────────────────────────────────────────────────────────

/// Handle a `webSearch2` request locally via Kagi.
pub async fn handle_web_search(state: &Arc<AppState>, body: &Bytes) -> Option<Bytes> {
    let config = state.config.load();
    let wt = &config.amp.web_tools;
    if !wt.enabled {
        return None;
    }
    let (search_cookie, session_cookie) = match (&wt.kagi_search_cookie, &wt.kagi_session_cookie) {
        (Some(s), Some(sess)) => (s.clone(), sess.clone()),
        _ => {
            tracing::warn!("web_tools enabled but Kagi cookies not configured");
            return None;
        }
    };

    let json: Value = serde_json::from_slice(body).ok()?;
    let params = json.get("params")?;
    let objective = params
        .get("objective")
        .and_then(Value::as_str)
        .unwrap_or("");
    let search_queries = params
        .get("searchQueries")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let max_results = params
        .get("maxResults")
        .and_then(Value::as_u64)
        .unwrap_or(5) as usize;

    let query = if search_queries.is_empty() {
        objective.to_string()
    } else {
        search_queries[0].clone()
    };

    if query.is_empty() {
        let resp = serde_json::json!({
            "ok": true,
            "result": { "results": "No search query provided.", "showParallelAttribution": false }
        });
        return Some(Bytes::from(serde_json::to_vec(&resp).ok()?));
    }

    match kagi_search(
        &state.http,
        &query,
        &search_cookie,
        &session_cookie,
        max_results,
    )
    .await
    {
        Ok(results_text) => {
            let resp = serde_json::json!({
                "ok": true,
                "result": { "results": results_text, "showParallelAttribution": false }
            });
            Some(Bytes::from(serde_json::to_vec(&resp).ok()?))
        }
        Err(e) => {
            tracing::error!(error = %e, "local web search failed");
            let resp = serde_json::json!({
                "ok": false,
                "error": { "code": "web_search_error", "message": e.to_string() }
            });
            Some(Bytes::from(serde_json::to_vec(&resp).ok()?))
        }
    }
}

/// Perform a Kagi search and return formatted results text.
async fn kagi_search(
    http: &rquest::Client,
    query: &str,
    search_cookie: &str,
    session_cookie: &str,
    max_results: usize,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = rquest::Url::parse_with_params("https://kagi.com/html/search", &[("q", query)])?;

    let cookie_header = format!("_kagi_search_={search_cookie}; kagi_session={session_cookie}");

    let resp = http
        .get(url)
        .header("User-Agent", KAGI_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7")
        .header("Cookie", cookie_header)
        .send()
        .await?;

    let status = resp.status();
    let html = resp.text().await?;

    if status.as_u16() == 401
        || html.contains("sign in to kagi")
        || html.contains("log in to continue")
    {
        return Err("Kagi authentication failed — check cookies".into());
    }
    if status.as_u16() == 429 {
        return Err("Kagi rate limited".into());
    }
    if !status.is_success() {
        return Err(format!("Kagi returned HTTP {status}").into());
    }

    let all_results = parse_serp_html(&html);
    if all_results.is_empty() {
        return Ok("No results found.".to_string());
    }

    let mut output = String::new();
    for (i, r) in all_results.iter().take(max_results).enumerate() {
        output.push_str(&format!("{}. {}\n", i + 1, r.title));
        output.push_str(&format!("   URL: {}\n", r.url));
        if let Some(ref snippet) = r.snippet {
            if !snippet.is_empty() {
                output.push_str(&format!("   {snippet}\n"));
            }
        }
        output.push('\n');
    }
    Ok(output)
}

// ── extractWebPageContent ───────────────────────────────────────────────

/// Handle an `extractWebPageContent` request locally via `agent-browser` (Lightpanda).
pub async fn handle_extract_web_page(state: &Arc<AppState>, body: &Bytes) -> Option<Bytes> {
    let config = state.config.load();
    if !config.amp.web_tools.enabled {
        return None;
    }

    let json: Value = serde_json::from_slice(body).ok()?;
    let params = json.get("params")?;
    let url = params.get("url").and_then(Value::as_str)?;

    if url.is_empty() {
        let resp = serde_json::json!({
            "ok": false,
            "error": { "code": "invalid_url", "message": "URL is empty" }
        });
        return Some(Bytes::from(serde_json::to_vec(&resp).ok()?));
    }

    match extract_page_content(url).await {
        Ok(content) => {
            let resp = serde_json::json!({
                "ok": true,
                "result": { "excerpts": [content] }
            });
            Some(Bytes::from(serde_json::to_vec(&resp).ok()?))
        }
        Err(e) => {
            tracing::error!(error = %e, %url, "local page extraction failed");
            let resp = serde_json::json!({
                "ok": false,
                "error": { "code": "extract_error", "message": e.to_string() }
            });
            Some(Bytes::from(serde_json::to_vec(&resp).ok()?))
        }
    }
}

/// Extract page content using `agent-browser` with Lightpanda engine.
///
/// Opens the URL, waits for network idle (page fully loaded), takes a text
/// snapshot, then closes the browser.
async fn extract_page_content(
    url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::process::Command;

    // Use Lightpanda for speed (10x faster, 10x less memory than Chrome).
    // `open` already waits for page load; no extra `wait` needed.
    // `get text body` returns clean plaintext (innerText).
    let output = Command::new("agent-browser")
        .args([
            "--engine",
            "lightpanda",
            "batch",
            &format!("open {url}"),
            "get text body",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("agent-browser failed: {stderr}").into());
    }

    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        return Err("agent-browser returned empty content".into());
    }

    // Truncate to 256 KB (matching Amp's limit).
    let max_len = 256 * 1024;
    if text.len() > max_len {
        Ok(text[..max_len].to_string())
    } else {
        Ok(text)
    }
}
