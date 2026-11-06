use rmcp::model::CallToolResult;
use serde_json::Value;
pub trait ToolResultText {
    fn text_value(&self) -> Option<Value>;
}
impl ToolResultText for CallToolResult {
    fn text_value(&self) -> Option<Value> {
        self.content
            .first()?
            .as_text()
            .and_then(|t| serde_json::from_str(&t.text).ok())
    }
}
