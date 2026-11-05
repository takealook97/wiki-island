use crate::{Error, Result};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub(crate) struct Predicate {
    parts: Vec<String>,
    op: String,
    values: Vec<Value>,
    numeric: bool,
}

fn scalar(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}
fn number(v: &Value) -> Option<f64> {
    if v.is_number() {
        return v.as_f64();
    }
    let text = v.as_str()?.trim();
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let parts: Vec<_> = unsigned.split('.').collect();
    if parts.len() > 2
        || parts
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    text.parse::<f64>().ok()
}

pub(crate) fn parse(filters: &Map<String, Value>) -> Result<Vec<Predicate>> {
    let mut result = Vec::new();
    for (path, raw) in filters {
        let path = path.trim();
        if path.split('.').any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        }) {
            return Err(Error::InvalidQuery(format!(
                "invalid metadata path: {path}"
            )));
        }
        let parts: Vec<_> = path.split('.').map(str::to_string).collect();
        let mut add = |op: &str, value: &Value| -> Result<()> {
            let op = op.strip_prefix('$').unwrap_or(op);
            if ![
                "eq", "gt", "gte", "lt", "lte", "in", "between", "contains", "exists",
            ]
            .contains(&op)
            {
                return Err(Error::InvalidQuery(format!(
                    "unsupported metadata operator: {op}"
                )));
            }
            let values = match op {
                "in" | "between" => value
                    .as_array()
                    .cloned()
                    .ok_or_else(|| Error::InvalidQuery(format!("{op} requires an array")))?,
                "contains" => value
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![value.clone()]),
                _ => vec![value.clone()],
            };
            if values.is_empty()
                || (op == "between" && values.len() != 2)
                || (op == "exists" && !value.is_boolean())
                || (op != "eq" && op != "exists" && values.iter().any(Value::is_null))
                || values.iter().any(|v| v.is_array() || v.is_object())
            {
                return Err(Error::InvalidQuery(format!("invalid operand for {op}")));
            }
            let numeric = ["gt", "gte", "lt", "lte", "between"].contains(&op)
                && values.iter().all(|v| number(v).is_some());
            if numeric
                && values
                    .iter()
                    .any(|v| number(v).is_none_or(|n| !n.is_finite()))
            {
                return Err(Error::InvalidQuery("numeric bound must be finite".into()));
            }
            result.push(Predicate {
                parts: parts.clone(),
                op: op.into(),
                values,
                numeric,
            });
            Ok(())
        };
        if let Some(ops) = raw.as_object() {
            if ops.is_empty() {
                return Err(Error::InvalidQuery("empty metadata operators".into()));
            }
            for (op, value) in ops {
                add(op, value)?;
            }
        } else if raw.is_array() {
            add("contains", raw)?;
        } else {
            add("eq", raw)?;
        }
    }
    Ok(result)
}

impl Predicate {
    pub(crate) fn sql(&self, params: &mut Vec<SqlValue>) -> String {
        let path = format!(
            "${}",
            self.parts
                .iter()
                .map(|p| format!(".\"{p}\""))
                .collect::<String>()
        );
        params.push(SqlValue::Text(path));
        let path_ref = format!("?{}", params.len());
        let raw = format!("json_extract(n.metadata, {path_ref})");
        let text = format!(
            "CASE json_type(n.metadata, {path_ref}) WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' ELSE CAST({raw} AS TEXT) END"
        );
        let expr = if self.numeric {
            format!("CAST({raw} AS REAL)")
        } else {
            text
        };
        if self.op == "exists" || (self.op == "eq" && self.values[0].is_null()) {
            let not = if self.op == "exists" && self.values[0] == true {
                "NOT "
            } else {
                ""
            };
            return format!("{raw} IS {not}NULL");
        }
        let mut binds = Vec::new();
        for v in &self.values {
            params.push(if self.numeric {
                SqlValue::Real(number(v).unwrap_or(0.0))
            } else {
                SqlValue::Text(scalar(v))
            });
            binds.push(format!("?{}", params.len()));
        }
        match self.op.as_str() {
            "eq" => format!("{expr} = {}", binds[0]),
            "in" => format!("{expr} IN ({})", binds.join(",")),
            "contains" => binds.iter().map(|b| format!("(json_type(n.metadata, {path_ref}) = 'array' AND EXISTS (SELECT 1 FROM json_each(n.metadata, {path_ref}) WHERE CASE type WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' ELSE CAST(value AS TEXT) END = {b}))")).collect::<Vec<_>>().join(" AND "),
            "between" => format!("{expr} BETWEEN {} AND {}", binds[0], binds[1]),
            op => format!("{expr} {} {}", match op { "gt" => ">", "gte" => ">=", "lt" => "<", _ => "<=" }, binds[0]),
        }
    }
}
