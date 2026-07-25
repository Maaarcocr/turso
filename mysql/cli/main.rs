use std::env;
use turso_core::Value;

const HELLO_WORLD_SQL: &str = "SELECT 'hello world' AS greeting";

fn main() -> anyhow::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let sql = if arguments.is_empty() {
        HELLO_WORLD_SQL.to_string()
    } else {
        arguments.join(" ")
    };

    let connection = turso_mysql::open_memory()?;
    let mut statement = connection.prepare(&sql)?;
    if statement.parameters_count() != 0 {
        anyhow::bail!("tursomysql CLI does not support binding parameters");
    }
    for row in statement.run_collect_rows()? {
        println!(
            "{}",
            row.iter().map(display_value).collect::<Vec<_>>().join("\t")
        );
    }
    connection.close()?;
    Ok(())
}

fn display_value(value: &Value) -> String {
    match value {
        Value::Text(text) => text.as_str().to_string(),
        Value::Null => "NULL".to_string(),
        Value::Numeric(_) | Value::Blob(_) => value.to_string(),
    }
}
