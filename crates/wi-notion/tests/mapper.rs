use std::collections::BTreeMap;
use wi_notion::map_markdown;

#[test]
fn maps_supported_blocks_and_inline_styles() {
    let note = b"---\ntitle: Example\n---\n# One\n\n## Two\n\n### Three\n\nText **bold** *italic* ~~strike~~ `code` [web](https://example.com).\n\n- item\n- [x] done\n\n1. numbered\n\n> quoted\n\n---\n\n```rust\nfn main() {}\n```\n\n| A | B |\n| - | - |\n| x | y |\n";
    let mapped = map_markdown(note, &BTreeMap::new()).unwrap();
    let kinds: Vec<_> = mapped
        .blocks
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "callout",
            "paragraph",
            "heading_1",
            "heading_2",
            "heading_3",
            "paragraph",
            "bulleted_list_item",
            "to_do",
            "numbered_list_item",
            "quote",
            "divider",
            "code",
            "table"
        ]
    );
    assert_eq!(mapped.blocks[7]["to_do"]["checked"], true);
    let inline = mapped.blocks[5]["paragraph"]["rich_text"]
        .as_array()
        .unwrap();
    assert!(inline.iter().any(|v| v["annotations"]["bold"] == true));
    assert!(inline.iter().any(|v| v["annotations"]["italic"] == true));
    assert!(
        inline
            .iter()
            .any(|v| v["annotations"]["strikethrough"] == true)
    );
    assert!(inline.iter().any(|v| v["annotations"]["code"] == true));
    assert!(
        inline
            .iter()
            .any(|v| v["text"]["link"]["url"] == "https://example.com")
    );
    assert_eq!(
        mapped.blocks[12]["table"]["children"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn nested_task_does_not_change_parent_list_kind() {
    let mapped = map_markdown(b"- parent\n  - [ ] child", &BTreeMap::new()).unwrap();
    assert_eq!(mapped.blocks[1]["type"], "bulleted_list_item");
    assert_eq!(
        mapped.blocks[1]["bulleted_list_item"]["children"][0]["type"],
        "to_do"
    );
}

#[test]
fn chunks_rich_text_by_utf16_units_and_array_limit() {
    let body = "😀".repeat(101_001);
    let mapped = map_markdown(body.as_bytes(), &BTreeMap::new()).unwrap();
    assert_eq!(mapped.blocks.len(), 3);
    let mut joined = String::new();
    for block in &mapped.blocks[1..] {
        let rich = block["paragraph"]["rich_text"].as_array().unwrap();
        assert!(rich.len() <= 100);
        for item in rich {
            let content = item["text"]["content"].as_str().unwrap();
            assert!(content.encode_utf16().count() <= 2000);
            joined.push_str(content);
        }
    }
    assert_eq!(joined, body);
}

#[test]
fn maps_known_wikilinks_and_reports_unsupported_syntax() {
    let known = BTreeMap::from([(
        "target".into(),
        "22222222-2222-2222-2222-222222222222".into(),
    )]);
    let mapped = map_markdown(
        b"[[target]] [[unknown]] [file](local.md) ![image](a.png) <b>html</b>",
        &known,
    )
    .unwrap();
    let rich = mapped.blocks[1]["paragraph"]["rich_text"]
        .as_array()
        .unwrap();
    assert!(
        rich.iter()
            .any(|v| v["mention"]["page"]["id"] == known["target"])
    );
    assert!(rich.iter().any(|v| v["text"]["content"] == "unknown"));
    assert!(
        mapped
            .warnings
            .iter()
            .any(|w| w.contains("unsupported link"))
    );
    assert!(mapped.warnings.iter().any(|w| w.contains("HTML")));
    assert!(mapped.warnings.iter().any(|w| w.contains("not uploaded")));
}

#[test]
fn rejects_invalid_utf8_and_tables_over_one_hundred_columns() {
    assert!(map_markdown(&[0xff], &BTreeMap::new()).is_err());
    let row = format!("|{}|\n", vec!["a"; 101].join("|"));
    let separator = format!("|{}|\n", vec!["---"; 101].join("|"));
    assert!(
        map_markdown(
            format!("{row}{separator}{row}").as_bytes(),
            &BTreeMap::new()
        )
        .is_err()
    );
}
