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

fn redact(text: &mut String, replaced: &mut usize) {
    let (clean, count) = wi_lifecycle::redact_secrets(text);
    if count > 0 {
        *text = clean;
        *replaced += count;
    }
}

/// An id that holds a secret is replaced by a stable digest of itself, so two
/// different secret ids stay different and a re-import finds the same note.
fn redact_id(id: &mut String, replaced: &mut usize) {
    let (_, count) = wi_lifecycle::redact_secrets(id);
    if count > 0 {
        let digest = hash_bytes(id.as_bytes());
        let hex = digest.trim_start_matches("sha256:");
        *id = format!("redacted-{}", hex.get(..16).unwrap_or(hex));
        *replaced += count;
    }
}

/// Replace secret-like strings in ids, titles, messages and attachment names.
fn redact_conversation(conv: &mut super::Conversation) -> usize {
    let mut replaced = 0;
    redact_id(&mut conv.id, &mut replaced);
    redact(&mut conv.title, &mut replaced);
    for message in &mut conv.messages {
        redact(&mut message.text, &mut replaced);
        for name in &mut message.attachments {
            redact(name, &mut replaced);
        }
    }
    replaced
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
            let Some(mut conv) = parse_element(kind, value) else {
                applier.counts.skipped += 1;
                return Ok(true);
            };
            if conv.messages.is_empty() {
                applier.counts.skipped += 1;
                return Ok(true);
            }
            applier.counts.redacted += redact_conversation(&mut conv);
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
        for mut doc in docs {
            let replaced = &mut applier.counts.redacted;
            redact(&mut doc.content, replaced);
            redact(&mut doc.project, replaced);
            redact(&mut doc.filename, replaced);
            redact_id(&mut doc.project_id, replaced);
            redact_id(&mut doc.id, replaced);
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
            let id = render::document_key(&doc);
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
    if kind == Kind::Claude
        && let Some(projects) = &src.projects
    {
        import_projects(projects, &mut applier)?;
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
        let noun = if counts.skipped == 1 { "item" } else { "items" };
        diagnostics.push(format!(
            "skipped {} unreadable or empty {noun}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use wi_store::OpenOptions;

    fn vault() -> (tempfile::TempDir, Vault) {
        let tmp = tempfile::tempdir().unwrap();
        let (vault, _) = Vault::init_with(
            tmp.path().join("vault"),
            OpenOptions {
                index_path: Some(tmp.path().join("cache.sqlite3")),
            },
        )
        .unwrap();
        (tmp, vault)
    }

    const CONVERSATIONS: &[u8] =
        br#"[{"uuid":"1","name":"A long enough name","chat_messages":[{"sender":"human","text":"hello there"}]}]"#;

    #[test]
    fn json_over_the_limit_is_refused_with_a_next_step() {
        let (_tmp, vault) = vault();
        let limits = Limits { max_json_bytes: 40 };
        std::fs::write(vault.root().join("c.json"), CONVERSATIONS).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "conversations.json",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(CONVERSATIONS).unwrap();
        std::fs::write(
            vault.root().join("c.zip"),
            writer.finish().unwrap().into_inner(),
        )
        .unwrap();
        std::fs::create_dir(vault.root().join("folder")).unwrap();
        std::fs::write(
            vault.root().join("folder/conversations.json"),
            CONVERSATIONS,
        )
        .unwrap();
        for source in ["c.json", "c.zip", "folder"] {
            let error = try_import(&vault, source, &limits).unwrap_err();
            let sentence = super::super::owner_sentence(&error).expect(source);
            assert!(sentence.contains("smaller date range"), "{sentence}");
            assert!(sentence.contains("conversations-000.json"), "{sentence}");
        }
        assert!(!vault.root().join("AI").exists());
        // Within the limit the same sources import.
        let roomy = Limits {
            max_json_bytes: 4096,
        };
        assert!(try_import(&vault, "c.zip", &roomy).unwrap().is_some());
    }

    fn archive(entries: &[(&str, &[u8], zip::CompressionMethod)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes, method) in entries {
            let options = zip::write::SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn put(vault: &Vault, name: &str, bytes: &[u8]) {
        std::fs::write(vault.root().join(name), bytes).unwrap();
    }

    fn set_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn find(bytes: &[u8], signature: &[u8]) -> usize {
        bytes
            .windows(signature.len())
            .position(|w| w == signature)
            .unwrap()
    }

    #[test]
    fn a_large_entry_that_is_not_json_does_not_count_toward_the_limit() {
        let (_tmp, vault) = vault();
        let image = vec![7u8; 20_000];
        let stored = zip::CompressionMethod::Stored;
        put(
            &vault,
            "c.zip",
            &archive(&[
                ("conversations.json", CONVERSATIONS, stored),
                ("photo.png", &image, stored),
            ]),
        );
        let limits = Limits {
            max_json_bytes: 4096,
        };
        assert!(
            std::fs::metadata(vault.root().join("c.zip")).unwrap().len() > limits.max_json_bytes
        );
        assert!(try_import(&vault, "c.zip", &limits).unwrap().is_some());
        assert!(vault.root().join("AI/Claude").exists());
    }

    #[test]
    fn json_entries_over_the_limit_are_refused_with_the_unpack_step() {
        let (_tmp, vault) = vault();
        let stored = zip::CompressionMethod::Stored;
        put(
            &vault,
            "c.zip",
            &archive(&[
                ("conversations-000.json", CONVERSATIONS, stored),
                ("conversations-001.json", CONVERSATIONS, stored),
            ]),
        );
        let limits = Limits {
            max_json_bytes: 150,
        };
        let error = try_import(&vault, "c.zip", &limits).unwrap_err();
        let sentence = super::super::owner_sentence(&error).unwrap();
        assert!(sentence.contains("smaller date range"), "{sentence}");
        assert!(sentence.contains("unpack the archive"), "{sentence}");
        assert!(!vault.root().join("AI").exists());
    }

    #[test]
    fn an_entry_that_inflates_past_the_limit_is_refused_whatever_it_declares() {
        let (_tmp, vault) = vault();
        let bomb = vec![b' '; 1 << 20];
        let mut bytes = archive(&[(
            "conversations.json",
            &bomb,
            zip::CompressionMethod::Deflated,
        )]);
        // Declare 10 bytes in the local and the central header.
        let local = find(&bytes, b"PK\x03\x04");
        set_u32(&mut bytes, local + 22, 10);
        let central = find(&bytes, b"PK\x01\x02");
        set_u32(&mut bytes, central + 24, 10);
        put(&vault, "c.zip", &bytes);
        let limits = Limits {
            max_json_bytes: 4096,
        };
        let error = try_import(&vault, "c.zip", &limits).unwrap_err();
        let sentence = super::super::owner_sentence(&error).expect("a plain sentence");
        assert!(sentence.contains("smaller date range"), "{sentence}");
        assert!(!vault.root().join("AI").exists());
    }

    #[test]
    fn a_zip64_archive_is_refused_with_the_unpack_step() {
        let (_tmp, vault) = vault();
        let mut bytes = archive(&[(
            "conversations.json",
            CONVERSATIONS,
            zip::CompressionMethod::Stored,
        )]);
        let end = find(&bytes, b"PK\x05\x06");
        set_u32(&mut bytes, end + 16, u32::MAX);
        put(&vault, "big.zip", &bytes);
        let error = try_import(&vault, "big.zip", &Limits::default()).unwrap_err();
        let sentence = super::super::owner_sentence(&error).expect("a plain sentence");
        assert_eq!(
            sentence,
            "This archive is too big to be opened directly. Unpack it, then import the unpacked folder."
        );
    }
}
