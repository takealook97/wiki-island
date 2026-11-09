//! Move and write receipts.
use super::*;

impl Engine {
    pub(super) fn move_files(&self, a: &Value, out: &mut Value) -> Result<()> {
        let destination = required(a, "destination")?;
        let into = |source: &str| -> Result<String> {
            Ok(if destination.ends_with('/') {
                let prefix = folder(destination)?;
                let name = Path::new(source)
                    .file_name()
                    .and_then(|p| p.to_str())
                    .ok_or(Error::Invalid("Source must have a basename."))?;
                if prefix.is_empty() {
                    name.into()
                } else {
                    format!("{prefix}/{name}")
                }
            } else {
                destination.into()
            })
        };
        if let Some(source) = s(a, "path") {
            if a.get("identifier").is_some()
                || a.get("expected_checksum").is_some()
                || a.get("operation_id").is_some()
            {
                return Err(Error::Invalid(
                    "Directory move uses path, not identifier or per-file write preconditions.",
                ));
            }
            let source = folder(source)?;
            if source.is_empty() {
                return Err(Error::Invalid("Cannot move the vault root."));
            }
            let destination = into(source)?;
            let destination = folder(&destination)?;
            if destination.is_empty()
                || destination == source
                || destination.starts_with(&format!("{source}/"))
            {
                return Err(Error::Invalid(
                    "Directory destination must be outside the source subtree.",
                ));
            }
            // Refuse obvious destination collisions before publishing any member.
            // Each member is still checked again by the store during publication.
            let mut members = Vec::new();
            for (path, dir) in self.paths(source, usize::MAX)? {
                if dir {
                    continue;
                }
                let suffix = path
                    .strip_prefix(&format!("{source}/"))
                    .ok_or(Error::Invalid("Invalid member path."))?;
                let to = format!("{destination}/{suffix}");
                match self.vault.read(&to) {
                    Ok(_) => {
                        return Err(Error::Invalid(
                            "A directory destination member already exists.",
                        ));
                    }
                    Err(StoreError::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
                let read = self.vault.read(&path)?;
                members.push((path, to, read.checksum));
            }
            if members.is_empty() {
                return Err(Error::NotFound);
            }
            let mut receipts = Vec::new();
            for (from, to, checksum) in members {
                let request = MoveRequest {
                    from,
                    to,
                    expected_checksum: Some(checksum),
                    op_id: None,
                    ingress: Ingress::Mcp,
                };
                let moved = if markdown(&request.from) {
                    self.lifecycle.move_note(&self.vault, request)
                } else {
                    self.vault.move_note(request).map_err(LifecycleError::from)
                };
                match moved {
                    Ok(receipt) => receipts.push(json!(receipt)),
                    Err(error) => {
                        out["data"] =
                            json!({"partial":!receipts.is_empty(),"moved":receipts.len()});
                        out["items"] = json!(receipts);
                        return Err(error.into());
                    }
                }
            }
            out["data"] = json!({"partial":false,"moved":receipts.len(),"source":source,"destination":destination});
            out["items"] = json!(receipts);
            return Ok(());
        }
        let path = required(a, "identifier")?;
        let r = self.lifecycle.move_note(
            &self.vault,
            MoveRequest {
                from: path.into(),
                to: into(path)?,
                expected_checksum: owned(a, "expected_checksum"),
                op_id: owned(a, "operation_id"),
                ingress: Ingress::Mcp,
            },
        )?;
        self.receipt(r, out)
    }

    pub(super) fn receipt(&self, receipt: WriteResult, out: &mut Value) -> Result<()> {
        out["index"] = json!(receipt.index);
        out["generation"] = json!(receipt.generation);
        out["data"] = json!(receipt);
        // Commit off the save path, with the next request or background flush.
        // Shutdown also flushes; an acknowledged done record survives a crash.
        Ok(())
    }
}
