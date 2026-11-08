use crate::{Error, Result, mapper::text};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub(crate) const VERSION: &str = "2026-03-11";
const MAX_BODY: usize = 450_000;

pub(crate) fn validate_id(id: &str) -> Result<()> {
    let valid = match id.len() {
        32 => id.bytes().all(|c| c.is_ascii_hexdigit()),
        36 => id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };
    if !valid {
        return Err(Error::Options("page and block ids must be UUIDs"));
    }
    Ok(())
}

pub(crate) struct Client {
    agent: ureq::Agent,
    token: String,
    base: String,
    interval: Duration,
    last: Option<Instant>,
    max_retries: u32,
}

impl Client {
    pub(crate) fn new(token: String, options: &crate::PublishOptions) -> Result<Self> {
        if token.is_empty()
            || ureq::http::HeaderValue::from_str(&format!("Bearer {token}")).is_err()
        {
            return Err(Error::Token);
        }
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(options.timeout))
            .build();
        Ok(Self {
            agent: config.into(),
            token,
            base: options.api_base_url.trim_end_matches('/').to_owned(),
            interval: Duration::try_from_secs_f64(1.0 / options.rate_limit.min(3.0))
                .map_err(|_| Error::Options("request rate is too small"))?,
            last: None,
            max_retries: options.max_retries,
        })
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        repeatable: bool,
    ) -> Result<Value> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        if body.len() > MAX_BODY {
            return Err(Error::Limit("request body is too large"));
        }
        for attempt in 0..=self.max_retries {
            if let Some(last) = self.last
                && let Some(wait) = self.interval.checked_sub(last.elapsed())
            {
                std::thread::sleep(wait);
            }
            let request = ureq::http::Request::builder()
                .method(method)
                .uri(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {}", self.token))
                .header("Notion-Version", VERSION)
                .header("Content-Type", "application/json")
                .body(body.as_slice())
                .map_err(|_| Error::Transport)?;
            self.last = Some(Instant::now());
            let mut response = self.agent.run(request).map_err(|_| Error::Transport)?;
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                return response
                    .body_mut()
                    .with_config()
                    .limit(4 * 1024 * 1024)
                    .read_json::<Value>()
                    .map_err(|_| Error::Response);
            }
            // Never expose remote bodies: they may echo credentials or note bytes.
            if attempt < self.max_retries
                && (status == 429 || (repeatable && (500..600).contains(&status)))
            {
                let delay = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1u64 << attempt.min(5));
                // Long throttles end this bounded call rather than retrying too early.
                if delay > 60 {
                    return Err(Error::Http(status));
                }
                std::thread::sleep(Duration::from_secs(delay));
                continue;
            }
            return Err(Error::Http(status));
        }
        Err(Error::Response)
    }

    pub(crate) fn create(&mut self, parent: &str, title: &str) -> Result<String> {
        let result = self.request(
            "POST",
            "/pages",
            Some(&json!({
                "parent":{"type":"page_id", "page_id":parent},
                "properties":{"title":{"type":"title", "title":title_text(title)?}}
            })),
            false,
        )?;
        let id = result["id"].as_str().ok_or(Error::Response)?;
        validate_id(id).map_err(|_| Error::Response)?;
        Ok(id.to_owned())
    }

    pub(crate) fn page(&mut self, id: &str, title: Option<&str>, archived: bool) -> Result<()> {
        validate_id(id)?;
        let mut value = json!({"in_trash":archived});
        if let Some(title) = title {
            value["properties"] = json!({"title":{"type":"title", "title":title_text(title)?}});
        }
        self.request("PATCH", &format!("/pages/{id}"), Some(&value), true)?;
        Ok(())
    }

    pub(crate) fn clear(&mut self, page: &str) -> Result<()> {
        let mut cursor = None;
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        loop {
            let mut path = format!("/blocks/{page}/children?page_size=100");
            if let Some(cursor) = &cursor {
                path.push_str(&format!("&start_cursor={cursor}"));
            }
            let response = self.request("GET", &path, None, true)?;
            for block in response["results"].as_array().ok_or(Error::Response)? {
                let id = block["id"].as_str().ok_or(Error::Response)?;
                validate_id(id).map_err(|_| Error::Response)?;
                if !ids.iter().any(|v| v == id) {
                    ids.push(id.to_owned());
                }
            }
            if !response["has_more"].as_bool().ok_or(Error::Response)? {
                break;
            }
            let next = response["next_cursor"].as_str().ok_or(Error::Response)?;
            // API cursors are opaque; percent encode before using them in a URL.
            let next: String = next
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            if !seen.insert(next.clone()) {
                return Err(Error::Response);
            }
            cursor = Some(next);
        }
        for id in ids {
            self.request("DELETE", &format!("/blocks/{id}"), None, true)?;
        }
        Ok(())
    }

    /// Send shallow blocks and then their children so arbitrary note nesting
    /// never exceeds the API's two-level per-request nesting limit.
    pub(crate) fn append(&mut self, parent: &str, blocks: &[Value]) -> Result<()> {
        let mut offset = 0;
        while offset < blocks.len() {
            let mut shallow = Vec::new();
            let mut pending = Vec::new();
            let mut size = 100;
            for value in &blocks[offset..] {
                let (value, children) = shallow_block(value)?;
                let len = serde_json::to_vec(&value)?.len() + 1;
                if shallow.len() >= 100 || size + len > MAX_BODY {
                    if shallow.is_empty() {
                        return Err(Error::Limit("one block is too large"));
                    }
                    break;
                }
                size += len;
                shallow.push(value);
                pending.push(children);
            }
            let response = self.request(
                "PATCH",
                &format!("/blocks/{parent}/children"),
                Some(&json!({"children":shallow})),
                false,
            )?;
            let results = response["results"].as_array().ok_or(Error::Response)?;
            if results.len() != shallow.len() {
                return Err(Error::Response);
            }
            for (result, children) in results.iter().zip(&pending) {
                if !children.is_empty() {
                    let id = result["id"].as_str().ok_or(Error::Response)?;
                    validate_id(id).map_err(|_| Error::Response)?;
                    self.append(id, children)?;
                }
            }
            offset += shallow.len();
        }
        Ok(())
    }
}

fn shallow_block(block: &Value) -> Result<(Value, Vec<Value>)> {
    let mut block = block.clone();
    let kind = block["type"].as_str().ok_or(Error::Response)?.to_owned();
    let mut children = block[&kind]
        .as_object_mut()
        .ok_or(Error::Response)?
        .remove("children")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    // A table must be created with at least one row.
    if kind == "table" && !children.is_empty() {
        block[&kind]["children"] = json!([children.remove(0)]);
    }
    Ok((block, children))
}

pub(crate) fn validate_blocks(blocks: &[Value]) -> Result<()> {
    for block in blocks {
        let (shallow, children) = shallow_block(block)?;
        if serde_json::to_vec(&shallow)?.len() + 100 > MAX_BODY {
            return Err(Error::Limit("one block is too large"));
        }
        validate_blocks(&children)?;
    }
    Ok(())
}

pub(crate) fn title_text(title: &str) -> Result<Vec<Value>> {
    let rich = text(title);
    if rich.len() > 100 {
        return Err(Error::Limit("title has over 100 rich-text elements"));
    }
    Ok(rich)
}
