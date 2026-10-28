use turso_core::dialect::{DialectType, StorageType};

macro_rules! temporal {
    ($name:literal, $parameters:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: $parameters,
            behaviour: "ASSIGN __mysql_assign_text(value, NULL) \
                CAST __mysql_sqlite_cast(value, 'TEXT')",
        }
    };
}

pub(super) const TYPES: [DialectType; 4] = [
    temporal!("date", ""),
    temporal!("datetime", "precision integer DEFAULT 0"),
    temporal!("timestamp", "precision integer DEFAULT 0"),
    temporal!("time", "precision integer DEFAULT 0"),
];
