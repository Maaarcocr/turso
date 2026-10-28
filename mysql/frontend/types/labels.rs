use turso_core::dialect::{DialectType, StorageType};

macro_rules! labels {
    ($name:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: "",
            behaviour: "ASSIGN __mysql_assign_text(value, NULL) \
                CAST __mysql_sqlite_cast(value, 'TEXT')",
        }
    };
}

pub(super) const TYPES: [DialectType; 2] = [labels!("enum"), labels!("set")];
