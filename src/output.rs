//! Versioned JSON envelope, field selection, pagination and table output.
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format {
    Json,
    Ndjson,
    Table,
}

pub struct Out {
    pub schema: String,
    pub data: Value,
    pub dry_run: bool,
    pub page: Option<Value>,
    pub extra: Map<String, Value>,
}

impl Out {
    pub fn new(schema: &str, data: Value) -> Self {
        Out {
            schema: format!("icloud.{schema}/v1"),
            data,
            dry_run: false,
            page: None,
            extra: Map::new(),
        }
    }
    pub fn dry(mut self, d: bool) -> Self {
        self.dry_run = d;
        self
    }
    pub fn page(mut self, p: Value) -> Self {
        self.page = Some(p);
        self
    }
}

/// Keep only dotted `fields` in each object (array elements individually).
pub fn select_fields(v: &Value, fields: &[String]) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(|x| select_fields(x, fields)).collect()),
        Value::Object(_) => {
            let mut out = Map::new();
            for f in fields {
                let parts: Vec<&str> = f.split('.').collect();
                if let Some(x) = get_path(v, &parts) {
                    insert_path(&mut out, &parts, x.clone());
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn get_path<'a>(v: &'a Value, parts: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for p in parts {
        cur = cur.get(*p)?;
    }
    Some(cur)
}

fn insert_path(m: &mut Map<String, Value>, parts: &[&str], val: Value) {
    if parts.len() == 1 {
        m.insert(parts[0].to_string(), val);
        return;
    }
    let e = m
        .entry(parts[0].to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(inner) = e {
        insert_path(inner, &parts[1..], val);
    }
}

/// Offset-based cursor pagination over an already sorted list.
pub fn paginate(items: Vec<Value>, limit: usize, cursor: Option<&str>) -> (Vec<Value>, Value) {
    let off: usize = cursor
        .and_then(|c| c.strip_prefix("o:"))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(off).take(limit).collect();
    let next = if off + page.len() < total {
        Some(format!("o:{}", off + page.len()))
    } else {
        None
    };
    let meta = json!({ "returned": page.len(), "total": total, "next_cursor": next });
    (page, meta)
}

pub fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| {
                    !v.is_null() && !(v.is_array() && v.as_array().unwrap().is_empty())
                })
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        o => o,
    }
}

fn cell(v: &Value) -> String {
    let s = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > 40 {
        format!("{}…", s.chars().take(39).collect::<String>())
    } else {
        s
    }
}

pub fn table(v: &Value) -> String {
    let rows: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut cols: Vec<String> = Vec::new();
    for r in &rows {
        if let Value::Object(m) = r {
            for (k, val) in m {
                if !cols.contains(k) && !val.is_object() {
                    cols.push(k.clone());
                }
            }
        }
    }
    if cols.is_empty() {
        return serde_json::to_string_pretty(v).unwrap_or_default();
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            cols.iter()
                .map(|c| cell(r.get(c).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    let w: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            cells
                .iter()
                .map(|r| r[i].chars().count())
                .max()
                .unwrap_or(0)
                .max(c.len())
        })
        .collect();
    let mut out = String::new();
    let line = |vals: &[String]| -> String {
        vals.iter()
            .enumerate()
            .map(|(i, s)| format!("{s:<width$}", width = w[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    out.push_str(&line(&cols));
    out.push('\n');
    for r in cells {
        out.push_str(&line(&r));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fields_and_pages() {
        let v = json!([{"a":1,"b":{"c":2,"d":3}},{"a":4}]);
        let s = select_fields(&v, &["a".into(), "b.c".into()]);
        assert_eq!(s, json!([{"a":1,"b":{"c":2}},{"a":4}]));
        let (p, m) = paginate(vec![json!(1), json!(2), json!(3)], 2, None);
        assert_eq!(p.len(), 2);
        assert_eq!(m["next_cursor"], "o:2");
        let (p, m) = paginate(vec![json!(1), json!(2), json!(3)], 2, Some("o:2"));
        assert_eq!(p, vec![json!(3)]);
        assert!(m["next_cursor"].is_null());
    }
}
