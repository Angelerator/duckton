//! Portable SQL value & result-set model.
//!
//! This is the engine-independent representation of query output. It is used by:
//!  * the mock query engine (deterministic test execution),
//!  * the real DuckDB engine adapter (maps DuckDB rows into this form),
//!  * the canonical result hasher in `p2p-trust`.

use serde::{Deserialize, Serialize};

/// A single SQL cell value.
///
/// Floats are stored as their raw bits is *not* done here; canonicalization
/// (numeric/NULL normalization) happens in the trust layer's hasher so that the
/// wire form stays human-readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Arbitrary-precision-ish decimal kept as text to stay deterministic.
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// A short tag byte used to make the canonical hash type-aware so that e.g.
    /// the integer `1` and the text `"1"` never collide.
    pub fn type_tag(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Text(_) => 4,
            Value::Blob(_) => 5,
        }
    }
}

/// A materialized result set: column names + rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl ResultSet {
    pub fn new(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Self {
        Self { columns, rows }
    }

    pub fn empty() -> Self {
        Self {
            columns: vec![],
            rows: vec![],
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_tags_disambiguate() {
        assert_ne!(Value::Int(1).type_tag(), Value::Text("1".into()).type_tag());
    }

    #[test]
    fn result_set_roundtrips_json() {
        let rs = ResultSet::new(
            vec!["region".into(), "n".into()],
            vec![
                vec![Value::Text("us".into()), Value::Int(3)],
                vec![Value::Text("eu".into()), Value::Null],
            ],
        );
        let bytes = crate::to_bytes(&rs).unwrap();
        let back: ResultSet = crate::from_bytes(&bytes).unwrap();
        assert_eq!(rs, back);
    }
}
