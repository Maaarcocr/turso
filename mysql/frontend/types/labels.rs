use turso_core::dialect::{DialectType, StorageType};

macro_rules! labels {
    ($name:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: "",
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

pub(super) const TYPES: [DialectType; 2] = [labels!("enum"), labels!("set")];
