use turso_core::dialect::{DialectType, StorageType};

macro_rules! temporal {
    ($name:literal, $parameters:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: $parameters,
            behaviour: concat!(
                "ASSIGN __mysql_assign_text(value, NULL) ",
                "CAST __mysql_sqlite_cast(value, 'TEXT') ",
                "FUNCTION __mysql_cast_as_signed(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported ",
                "FUNCTION __mysql_cast_as_unsigned(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 4] = [
    temporal!("date", ""),
    temporal!("datetime", "precision integer DEFAULT 0"),
    temporal!("timestamp", "precision integer DEFAULT 0"),
    temporal!("time", "precision integer DEFAULT 0"),
];
