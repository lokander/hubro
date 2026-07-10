//! Backend-neutral schema metadata. Captures primary keys, unique indexes,
//! and foreign keys up front — editing and FK navigation depend on them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Declared type as written in the schema (may be empty in SQLite).
    pub type_name: String,
    pub nullable: bool,
    /// 1-based position within the primary key; `None` when not part of it.
    pub primary_key_position: Option<u32>,
    /// Default value expression as written in the schema, if any.
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMeta {
    /// Referencing columns in this table, in key order.
    pub columns: Vec<String>,
    pub referenced_table: String,
    /// Referenced columns, parallel to `columns`. An entry is `None` when the
    /// FK references the target table's implicit primary key.
    pub referenced_columns: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMeta {
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColumnMeta>,
    pub indexes: Vec<IndexMeta>,
    pub foreign_keys: Vec<ForeignKeyMeta>,
}

impl TableMeta {
    /// Columns making up the primary key, in key order.
    pub fn primary_key(&self) -> Vec<&ColumnMeta> {
        let mut pk: Vec<&ColumnMeta> = self
            .columns
            .iter()
            .filter(|c| c.primary_key_position.is_some())
            .collect();
        pk.sort_by_key(|c| c.primary_key_position);
        pk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, pk: Option<u32>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "TEXT".into(),
            nullable: true,
            primary_key_position: pk,
            default: None,
        }
    }

    #[test]
    fn primary_key_is_ordered_by_key_position() {
        let table = TableMeta {
            name: "t".into(),
            kind: TableKind::Table,
            columns: vec![col("a", Some(2)), col("b", None), col("c", Some(1))],
            indexes: vec![],
            foreign_keys: vec![],
        };
        let pk: Vec<&str> = table
            .primary_key()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(pk, ["c", "a"]);
    }
}
