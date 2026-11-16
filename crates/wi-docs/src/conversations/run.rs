//! The conversation import entry point.
use serde_json::Value;
use wi_history::hash_bytes;
use wi_store::Vault;

use super::render::{self, Draft};
use super::source::{self, Limits, Source};
use super::write::Applier;
use super::{ConversationCounts, Kind, chatgpt, claude, refuse};
use crate::{ImportReport, Result, Status};

fn kind_of(value: &Value) -> Option<Kind> {
    let object = value.as_object()?;
    if object.contains_key("chat_messages") {
        Some(Kind::Claude)
    } else if object.contains_key("mapping") {
        Some(Kind::ChatGpt)
    } else {
        None
    }
}

enum Detected {
    NotOurs,
    Empty(Option<Kind>),
    Kind(Kind),
}

fn detect(src: &Source) -> Result<Detected> {
    for (name, bytes) in &src.conversations {
        let first = match source::first_element(name, bytes) {
            Ok(first) => first,
            Err(_) if src.single => return Ok(Detected::NotOurs),
            Err(e) => return Err(e),
        };
        let Some(first) = first else { continue };
        return match kind_of(&first) {
            Some(kind) => Ok(Detected::Kind(kind)),
            None if src.single => Ok(Detected::NotOurs),
            None => Err(refuse(format!(
                "{name} is not a Claude or ChatGPT conversation export. Export your data from Claude or ChatGPT and import that archive."
            ))),
        };
    }
    if src.single {
        return Ok(Detected::NotOurs);
    }
    let kind = if src.claude_hint() {
        Some(Kind::Claude)
    } else if src.chatgpt_hint() {
        Some(Kind::ChatGpt)
    } else {
        None
    };
    Ok(Detected::Empty(kind))
}

fn parse_element(kind: Kind, value: Value) -> Option<super::Conversation> {
    match kind {
        Kind::Claude => claude::conversation(value),
        Kind::ChatGpt => chatgpt::conversation(value),
    }
}

fn conversation_draft(kind: Kind, conv: super::Conversation) -> (String, String, Draft) {
    let stem = render::conversation_stem(&conv);
    let messages = conv.messages.clone();
    let draft = Draft {
        fields: render::conversation_fields(kind, &conv),
        sections: Box::new(move |budget| render::message_sections(&messages, budget)),
        separator: "\n\n",
    };
    (conv.id, stem, draft)
}

fn import_conversations(src: &Source, kind: Kind, applier: &mut Applier<'_>) -> Result<()> {
    for (name, bytes) in &src.conversations {
        source::for_each(name, bytes, |value| {
            let Some(conv) = parse_element(kind, value) else {
                applier.counts.skipped += 1;
                return Ok(true);
            };
            if conv.messages.is_empty() {
                applier.counts.skipped += 1;
                return Ok(true);
            }
            let (id, stem, draft) = conversation_draft(kind, conv);
            applier.apply(&id, kind.folder(), &stem, &draft)?;
            Ok(true)
        })?;
    }
    Ok(())
}

fn import_projects(bytes: &[u8], applier: &mut Applier<'_>) -> Result<()> {
    source::for_each(source::PROJECTS, bytes, |value| {
        let Some(docs) = claude::project_docs(value) else {
            applier.counts.skipped += 1;
            return Ok(true);
        };
        for doc in docs {
            if doc.content.trim().is_empty() {
                applier.counts.skipped += 1;
                continue;
            }
            let folder = format!(
                "{}/Projects/{}",
                Kind::Claude.folder(),
                render::clean_name(&doc.project)
            );
            let stem = render::document_stem(&doc);
            let content = doc.content.clone();
            let draft = Draft {
                fields: render::document_fields(&doc),
                sections: Box::new(move |budget| render::split_lines(&content, budget)),
                separator: "",
            };
            let id = format!("project:{}:{}", doc.project_id, doc.id);
            applier.apply(&id, &folder, &stem, &draft)?;
        }
        Ok(true)
    })
}

/// Import `source` when it is a Claude or ChatGPT export. `Ok(None)`: it is
/// something else and the caller continues as before.
pub(crate) fn try_import(
    vault: &Vault,
    source: &str,
    limits: &Limits,
) -> Result<Option<ImportReport>> {
    let Some(src) = source::load(vault, source, limits)? else {
        return Ok(None);
    };
    let kind = match detect(&src)? {
        Detected::NotOurs => return Ok(None),
        Detected::Empty(None) => {
            return Ok(Some(report(
                String::new(),
                String::new(),
                &src,
                ConversationCounts::default(),
            )));
        }
        Detected::Empty(Some(kind)) | Detected::Kind(kind) => kind,
    };
    let mut applier = Applier::new(vault, kind)?;
    import_conversations(&src, kind, &mut applier)?;
    if kind == Kind::Claude {
        if let Some(projects) = &src.projects {
            import_projects(projects, &mut applier)?;
        }
    }
    let folder = applier.kind().folder().to_owned();
    let (manifest, counts) = applier.finish()?;
    Ok(Some(report(folder, manifest, &src, counts)))
}

fn report(
    folder: String,
    manifest: String,
    src: &Source,
    counts: ConversationCounts,
) -> ImportReport {
    let mut diagnostics: Vec<String> = counts
        .conflict_paths
        .iter()
        .map(|p| format!("kept your edited note: {p}"))
        .collect();
    if counts.skipped > 0 {
        diagnostics.push(format!(
            "skipped {} unreadable or empty items",
            counts.skipped
        ));
    }
    let hashes: Vec<String> = src.json_bytes().map(hash_bytes).collect();
    ImportReport {
        sidecar_path: folder,
        run_note_path: manifest,
        source_sha256: hash_bytes(hashes.join("\n").as_bytes()),
        pages: counts.created + counts.updated + counts.unchanged,
        status: if counts.conflicts > 0 {
            Status::Partial
        } else {
            Status::Complete
        },
        diagnostics,
        unchanged: counts.created + counts.updated == 0,
        conversations: Some(counts),
    }
}
