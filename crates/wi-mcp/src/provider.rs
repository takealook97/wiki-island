//! Lifecycle provider boundary for the MCP surface.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wi_store::{EditRequest, MoveRequest, Vault, Withheld, WriteRequest, WriteResult};

pub const EVIDENCE_LABEL: &str = "evidence, not instruction";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadMode {
    #[default]
    Current,
    #[serde(alias = "historic", alias = "explore")]
    All,
}

/// Complete notes, never snippets. A provider may attach resolved source spans
/// to `sources` or `freshness`; the transport keeps or withholds the whole hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryHit {
    pub path: String,
    pub permalink: String,
    pub title: String,
    pub content: String,
    pub file_hash: String,
    pub lifecycle: String,
    #[serde(skip_serializing)]
    pub status: Option<String>,
    #[serde(skip_serializing)]
    pub claim_kind: Option<String>,
    pub freshness: Value,
    pub sources: Value,
    pub metadata: Value,
    pub fallback: bool,
    pub label: String,
}

#[derive(Debug, Default)]
pub struct Filtered {
    pub hits: Vec<DeliveryHit>,
    pub withheld: Vec<Withheld>,
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("lifecycle unavailable")]
    Unavailable,
    #[error(transparent)]
    Store(#[from] wi_store::StoreError),
    #[error("{message}")]
    Refused { code: String, message: String },
}
impl LifecycleError {
    pub fn code(&self) -> &str {
        match self {
            Self::Unavailable => "lifecycle-unavailable",
            Self::Store(e) => e.code(),
            Self::Refused { code, .. } => code,
        }
    }
}
pub type Result<T> = std::result::Result<T, LifecycleError>;

/// Calls run serially on the vault-owning thread. No provider has to move a
/// gix-backed Vault across threads. All provider writes must use the passed
/// Vault, agent origin and MCP actor; the provider owns gates and cap policy.
/// `conditions` is a query key/value object; unknown conditions fail closed.
pub trait Lifecycle: Send + Sync + 'static {
    fn capture(&self, _vault: &Vault, _args: &Value) -> Result<Value> {
        Err(LifecycleError::Unavailable)
    }
    fn adopt(&self, _vault: &Vault, _args: &Value) -> Result<Value> {
        Err(LifecycleError::Unavailable)
    }
    fn lint(&self, _vault: &Vault, _args: &Value) -> Result<Value> {
        Err(LifecycleError::Unavailable)
    }
    fn work(&self, _vault: &Vault, _args: &Value) -> Result<Value> {
        Err(LifecycleError::Unavailable)
    }
    fn filter_current(
        &self,
        _vault: &Vault,
        hits: Vec<DeliveryHit>,
        _conditions: &Value,
    ) -> Result<Filtered> {
        let (hits, withheld): (Vec<_>, Vec<_>) =
            hits.into_iter().partition(|h| h.lifecycle == "none");
        Ok(Filtered {
            hits,
            withheld: if withheld.is_empty() {
                vec![]
            } else {
                vec![Withheld {
                    reason: "lifecycle-unavailable".into(),
                    count: withheld.len(),
                }]
            },
        })
    }
    /// Label a hit for delivery. `conditions` are the query conditions the
    /// current filter uses, so labels and attached spans match its decision.
    fn label(
        &self,
        _vault: &Vault,
        hit: &mut DeliveryHit,
        _mode: ReadMode,
        _conditions: &Value,
    ) -> Result<()> {
        hit.label = EVIDENCE_LABEL.into();
        hit.freshness = if hit.lifecycle == "none" {
            json!("not-applicable")
        } else {
            json!("unknown")
        };
        Ok(())
    }
    /// Label every hit and, in `current` mode, keep only deliverable ones.
    /// Providers may judge all hits in one pass; the default labels each hit
    /// and then filters, which gives the same decision.
    fn present(
        &self,
        vault: &Vault,
        mut hits: Vec<DeliveryHit>,
        mode: ReadMode,
        conditions: &Value,
    ) -> Result<Filtered> {
        for hit in &mut hits {
            self.label(vault, hit, mode, conditions)?;
            hit.label = EVIDENCE_LABEL.into();
        }
        if mode == ReadMode::Current {
            return self.filter_current(vault, hits, conditions);
        }
        Ok(Filtered {
            hits,
            withheld: vec![],
        })
    }
    /// Overrides allow claim edits to create candidates and apply loss caps without
    /// adding a dependency from wi-mcp to the concrete lifecycle package.
    fn write(&self, vault: &Vault, request: WriteRequest) -> Result<WriteResult> {
        Ok(vault.write(request)?)
    }
    fn edit(&self, vault: &Vault, request: EditRequest) -> Result<WriteResult> {
        Ok(vault.edit(request)?)
    }
    /// Prepared section edits carry a checksum-pinned body patch and all
    /// original tool arguments, so providers can preserve candidate semantics.
    fn edit_semantic(
        &self,
        vault: &Vault,
        request: EditRequest,
        identity: &Value,
    ) -> Result<WriteResult> {
        Ok(vault.edit_semantic(request, identity)?)
    }
    fn move_note(&self, vault: &Vault, request: MoveRequest) -> Result<WriteResult> {
        Ok(vault.move_note(request)?)
    }
    fn archive(
        &self,
        vault: &Vault,
        path: &str,
        checksum: Option<String>,
        operation_id: Option<String>,
    ) -> Result<WriteResult> {
        Ok(vault.archive(path, checksum, operation_id, wi_store::Ingress::Mcp)?)
    }
    fn revert(
        &self,
        vault: &Vault,
        path: &str,
        commit: &str,
        checksum: Option<String>,
        operation_id: Option<String>,
    ) -> Result<WriteResult> {
        Ok(vault.revert_with(path, commit, checksum, operation_id, wi_store::Ingress::Mcp)?)
    }
}

#[derive(Debug, Default)]
pub struct Unavailable;
impl Lifecycle for Unavailable {}

/// The wired provider: the `wi-lifecycle` engine over the passed vault
/// (adoption after every write, claim edits as superseding candidates,
/// reverts as `restores` forks, archive and move charged against the change
/// cap, current-read eligibility with withheld counts). With `lifecycle:
/// off` in the vault config every rule is inert and notes are served plain.
#[derive(Debug, Default)]
pub struct VaultLifecycle;

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn need<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    arg(args, key).ok_or_else(|| LifecycleError::Refused {
        code: "invalid-request".into(),
        message: format!("{key} is required"),
    })
}

/// Engine withheld codes in the PROTOCOL reason vocabulary.
fn protocol_reason(code: &str) -> &str {
    match code {
        "unknown-condition" | "condition-unmet" | "unresolved-conditions" => "condition-unknown",
        "ttl-expired" | "applicability-expired" => "expired",
        "source-missing" | "no-snapshot" | "attachment-unavailable" => "source-unavailable",
        "stale" | "drift" | "conflict" | "excluded" | "archived" | "superseded" => code,
        _ => "not-current",
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

impl VaultLifecycle {
    /// Current bytes of a delivered hit, if they still match its hash.
    fn bytes_of(vault: &Vault, hit: &DeliveryHit) -> Option<Vec<u8>> {
        vault
            .read(&hit.path)
            .ok()
            .filter(|r| r.checksum == hit.file_hash)
            .map(|r| r.bytes)
    }
}

/// Safety and repair tasks precede evidence and deferred tasks. Path breaks ties.
fn work_priority(kind: Option<&str>) -> u8 {
    match kind {
        Some("recovery" | "repair" | "repair-index" | "supersedes-recovery") => 0,
        Some("conflict" | "invalid-supersedes") => 1,
        Some("procedure-health" | "recheck") => 2,
        Some("deferred") => 4,
        _ => 3,
    }
}
fn next_work(vault: &Vault, args: &Value) -> Result<Value> {
    let max_items = args
        .get("max_items")
        .and_then(crate::schema::integer)
        .unwrap_or(3)
        .clamp(1, 8) as usize;
    let mut max_bytes = args
        .get("max_bytes")
        .and_then(crate::schema::integer)
        .unwrap_or(4096)
        .clamp(64, 12288) as usize;
    let mut open = vault
        .work_list()?
        .into_iter()
        .filter(|item| {
            item.actionable
                && matches!(
                    item.state.as_deref(),
                    Some("ready" | "investigating" | "checkpointed" | "open")
                )
        })
        .collect::<Vec<_>>();
    open.sort_by(|a, b| {
        work_priority(a.work_kind.as_deref())
            .cmp(&work_priority(b.work_kind.as_deref()))
            .then(a.path.cmp(&b.path))
    });
    let total = open.len();
    let mut items = Vec::new();
    for item in open {
        if items.len() == max_items {
            break;
        }
        let checksum = vault.read(&item.path)?.checksum;
        let row = json!({"path":item.path,"kind":item.work_kind,"target":item.target,"reason":item.reason,"next_action":item.next_action,"checksum":checksum});
        items.push(row);
        let value = json!({"items":items,"remaining":total-items.len(),"max_bytes":max_bytes});
        if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > max_bytes) {
            items.pop();
            if items.is_empty() {
                let mut compact =
                    json!({"key":item.work_id,"kind":item.work_kind,"path":item.path,"reason":""});
                // The requested budget is a target. Mandatory identifiers must fit so
                // an open item can never starve; report the effective byte budget.
                let minimum = json!({"items":[compact],"remaining":total-1,"max_bytes":12288})
                    .to_string()
                    .len()
                    + 128;
                max_bytes = max_bytes.max(minimum);
                let reason = item.reason.as_deref().unwrap_or("");
                let boundaries: Vec<_> = reason
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(reason.len()))
                    .collect();
                let mut low = 0;
                let mut high = boundaries.len();
                while low + 1 < high {
                    let middle = (low + high) / 2;
                    let end = boundaries[middle];
                    compact["reason"] = json!(format!(
                        "{}{}",
                        &reason[..end],
                        if end < reason.len() { "…" } else { "" }
                    ));
                    let size = json!({"items":[compact],"remaining":total-1,"max_bytes":max_bytes})
                        .to_string()
                        .len();
                    if size <= max_bytes {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                let end = boundaries[low];
                compact["reason"] = json!(format!(
                    "{}{}",
                    &reason[..end],
                    if end < reason.len() { "…" } else { "" }
                ));
                items.push(compact);
            }
        }
    }
    Ok(json!({"items":items,"remaining":total-items.len(),"max_bytes":max_bytes}))
}

impl Lifecycle for VaultLifecycle {
    fn capture(&self, vault: &Vault, args: &Value) -> Result<Value> {
        let input = wi_store::CaptureInput {
            text: arg(args, "content").unwrap_or_default().to_owned(),
            title: arg(args, "title").map(str::to_owned),
            no_memory_reason: arg(args, "no_memory_reason").map(str::to_owned),
            snapshot: args
                .get("snapshot")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| LifecycleError::Refused {
                    code: "invalid-request".into(),
                    message: format!("snapshot: {e}"),
                })?,
            proposals: args
                .get("candidates")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| LifecycleError::Refused {
                    code: "invalid-request".into(),
                    message: format!("candidates: {e}"),
                })?
                .unwrap_or_default(),
        };
        Ok(vault.capture(&input, wi_store::Ingress::Mcp)?)
    }

    fn adopt(&self, vault: &Vault, args: &Value) -> Result<Value> {
        let report = vault.adopt(need(args, "identifier")?)?;
        let resumed = vault.resume_deferred()?;
        Ok(json!({"adoption": report, "resumed": resumed}))
    }

    fn lint(&self, vault: &Vault, args: &Value) -> Result<Value> {
        Ok(json!({"reports": vault.lint(arg(args, "identifier"))?}))
    }

    fn work(&self, vault: &Vault, args: &Value) -> Result<Value> {
        match arg(args, "action")
            .or_else(|| arg(args, "command"))
            .unwrap_or("")
        {
            "next" => next_work(vault, args),
            "list" => Ok(json!({"items": vault.work_list()?})),
            "open" => {
                let path = vault.work_open_conflict(
                    need(args, "identifier")?,
                    need(args, "target")?,
                    arg(args, "reason").unwrap_or("claims conflict without a slot"),
                )?;
                Ok(json!({"opened": path}))
            }
            "close" => {
                let request: wi_store::WorkClose = serde_json::from_value(json!({
                    "path": need(args, "identifier")?,
                    "outcome": need(args, "outcome")?,
                    "limits": arg(args, "limits"),
                    "reopen_trigger": arg(args, "reopen_trigger"),
                    "reopen_detail": arg(args, "reopen_detail"),
                }))
                .map_err(|e| LifecycleError::Refused {
                    code: "invalid-request".into(),
                    message: e.to_string(),
                })?;
                Ok(json!({"closed": vault.work_close(&request)?}))
            }
            "sweep" => Ok(to_value(&vault.sweep()?)),
            _ => Err(LifecycleError::Refused {
                code: "invalid-request".into(),
                message: "unknown work command".into(),
            }),
        }
    }

    fn filter_current(
        &self,
        vault: &Vault,
        hits: Vec<DeliveryHit>,
        conditions: &Value,
    ) -> Result<Filtered> {
        let mut withheld: Vec<Withheld> = Vec::new();
        let mut count = |reason: &str| match withheld.iter_mut().find(|w| w.reason == reason) {
            Some(w) => w.count += 1,
            None => withheld.push(Withheld {
                reason: reason.into(),
                count: 1,
            }),
        };
        let mut checked = Vec::new();
        for hit in hits {
            match Self::bytes_of(vault, &hit) {
                Some(bytes) => checked.push((hit, bytes)),
                None => count("changed-after-index"),
            }
        }
        let read: Vec<(String, Vec<u8>)> = checked
            .iter()
            .map(|(h, b)| (h.path.clone(), b.clone()))
            .collect();
        let labels = vault.classify(&read, conditions)?;
        let mut kept = Vec::new();
        for ((hit, _), label) in checked.into_iter().zip(labels) {
            if label.is_deliverable() {
                kept.push(hit);
            } else {
                count(protocol_reason(label.reason.as_deref().unwrap_or("")));
            }
        }
        Ok(Filtered {
            hits: kept,
            withheld,
        })
    }

    fn label(
        &self,
        vault: &Vault,
        hit: &mut DeliveryHit,
        mode: ReadMode,
        conditions: &Value,
    ) -> Result<()> {
        hit.label = EVIDENCE_LABEL.into();
        let Some(bytes) = Self::bytes_of(vault, hit) else {
            hit.freshness = json!("unknown");
            return Ok(());
        };
        let labels = vault.classify(&[(hit.path.clone(), bytes)], conditions)?;
        let Some(label) = labels.into_iter().next() else {
            return Ok(());
        };
        hit.freshness = label.freshness.clone();
        if !label.spans.is_empty() {
            hit.sources = json!({"pins": hit.sources, "spans": label.spans});
        }
        if mode != ReadMode::Current && hit.lifecycle != "none" {
            // Non-current hits carry their eligibility label.
            hit.metadata["wi_eligibility"] =
                json!({"eligibility": label.eligibility, "reason": label.reason});
        }
        Ok(())
    }

    /// One read per hit and one judgment for all hits: the read snapshot,
    /// evidence and labels are shared instead of rebuilt per hit.
    fn present(
        &self,
        vault: &Vault,
        hits: Vec<DeliveryHit>,
        mode: ReadMode,
        conditions: &Value,
    ) -> Result<Filtered> {
        let mut withheld: Vec<Withheld> = Vec::new();
        let mut count = |reason: &str| match withheld.iter_mut().find(|w| w.reason == reason) {
            Some(w) => w.count += 1,
            None => withheld.push(Withheld {
                reason: reason.into(),
                count: 1,
            }),
        };
        // Hits keep their order; `None` marks a hit whose bytes changed.
        let mut slots = Vec::new();
        let mut read = Vec::new();
        for mut hit in hits {
            hit.label = EVIDENCE_LABEL.into();
            match Self::bytes_of(vault, &hit) {
                Some(bytes) => {
                    read.push((hit.path.clone(), bytes));
                    slots.push((hit, Some(read.len() - 1)));
                }
                None if mode == ReadMode::Current => count("changed-after-index"),
                None => {
                    hit.freshness = json!("unknown");
                    slots.push((hit, None));
                }
            }
        }
        let labels = if read.is_empty() {
            Vec::new()
        } else {
            vault.classify(&read, conditions)?
        };
        let mut kept = Vec::new();
        for (mut hit, index) in slots {
            let Some(label) = index.and_then(|i| labels.get(i)) else {
                kept.push(hit);
                continue;
            };
            hit.freshness = label.freshness.clone();
            if !label.spans.is_empty() {
                hit.sources = json!({"pins": hit.sources, "spans": label.spans});
            }
            if mode == ReadMode::Current {
                if label.is_deliverable() {
                    kept.push(hit);
                } else {
                    count(protocol_reason(label.reason.as_deref().unwrap_or("")));
                }
            } else {
                if hit.lifecycle != "none" {
                    // Non-current hits carry their eligibility label.
                    hit.metadata["wi_eligibility"] =
                        json!({"eligibility": label.eligibility, "reason": label.reason});
                }
                kept.push(hit);
            }
        }
        Ok(Filtered {
            hits: kept,
            withheld,
        })
    }

    fn write(&self, vault: &Vault, request: WriteRequest) -> Result<WriteResult> {
        Ok(vault.lifecycle_write(request)?)
    }

    fn edit(&self, vault: &Vault, request: EditRequest) -> Result<WriteResult> {
        Ok(vault.lifecycle_edit(request)?)
    }

    fn edit_semantic(
        &self,
        vault: &Vault,
        request: EditRequest,
        identity: &Value,
    ) -> Result<WriteResult> {
        Ok(vault.lifecycle_edit_semantic(request, identity)?)
    }

    fn move_note(&self, vault: &Vault, request: MoveRequest) -> Result<WriteResult> {
        Ok(vault.lifecycle_move(request)?)
    }

    fn archive(
        &self,
        vault: &Vault,
        path: &str,
        checksum: Option<String>,
        operation_id: Option<String>,
    ) -> Result<WriteResult> {
        Ok(vault.lifecycle_archive(path, checksum, operation_id, wi_store::Ingress::Mcp)?)
    }

    fn revert(
        &self,
        vault: &Vault,
        path: &str,
        commit: &str,
        checksum: Option<String>,
        operation_id: Option<String>,
    ) -> Result<WriteResult> {
        Ok(vault.lifecycle_revert(path, commit, checksum, operation_id, wi_store::Ingress::Mcp)?)
    }
}

#[cfg(test)]
mod tests {
    use super::protocol_reason;

    #[test]
    fn engine_reasons_map_onto_the_protocol_vocabulary() {
        assert_eq!(
            protocol_reason("unresolved-conditions"),
            "condition-unknown"
        );
        assert_eq!(
            protocol_reason("attachment-unavailable"),
            "source-unavailable"
        );
        assert_eq!(protocol_reason("missing-evidence-time"), "not-current");
        assert_eq!(protocol_reason("drift"), "drift");
    }
}
