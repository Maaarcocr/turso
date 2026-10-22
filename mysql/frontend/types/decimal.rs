use turso_core::dialect::{DialectType, StorageType};

pub(super) const TYPES: [DialectType; 2] = [
    DialectType {
        name: "decimal",
        storage: StorageType::Real,
        parameters: "precision integer DEFAULT 10, scale integer DEFAULT 0",
        behaviour: "ASSIGN __mysql_assign_double(value, NULL, NULL, 0) \
            CAST __mysql_sqlite_cast(value, 'NUMERIC')",
    },
    DialectType {
        name: "decimal unsigned",
        storage: StorageType::Real,
        parameters: "precision integer DEFAULT 10, scale integer DEFAULT 0",
        behaviour: "ASSIGN __mysql_assign_double(value, NULL, NULL, 1) \
            CAST __mysql_sqlite_cast(value, 'NUMERIC')",
    },
];
