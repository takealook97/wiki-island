//! `wi brief`: the standing brief (pinned notes, rules, preferences) that MCP
//! hosts read as `wiki-island://brief`.

use anyhow::Result;
use wi_store::Vault;

use super::Output;

pub(crate) fn brief(vault: &Vault) -> Result<Output> {
    let json = wi_mcp::brief(vault)?;
    let mut human = String::new();
    let items = json["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        human.push_str("Nothing pinned and no current rules or preferences.\n");
    }
    for item in &items {
        let kind = item["kind"].as_str().unwrap_or("note");
        let summary = item["summary"].as_str().unwrap_or("");
        let path = item["path"].as_str().unwrap_or("");
        human.push_str(&format!("[{kind}] {summary} ({path})\n"));
    }
    if let Some(noted) = json["noted_by_ai"]["items"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        human.push_str("Noted by your AI\n");
        for item in noted {
            human.push_str(&format!(
                "{} ({})\n",
                item["summary"].as_str().unwrap_or(""),
                item["path"].as_str().unwrap_or("")
            ));
        }
    }
    let omitted = json["omitted"].as_u64().unwrap_or(0);
    if omitted > 0 {
        human.push_str(&format!(
            "{omitted} more entr{} did not fit the 2 KiB brief.\n",
            if omitted == 1 { "y" } else { "ies" }
        ));
    }
    Ok(Output { json, human })
}
