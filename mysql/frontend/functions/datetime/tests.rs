use std::sync::Arc;

use turso_core::{Connection, Value};

#[test]
fn date_add_moves_dates_by_calendar_units() {
    let connection = crate::open_memory().expect("in-memory database opens");
    assert_eq!(
        select(&connection, "DATE_ADD('2018-05-02', 1, 'DAY')"),
        Value::build_text("2018-05-03")
    );
    assert_eq!(
        select(&connection, "DATE_ADD('2000-01-31', 1, 'MONTH')"),
        Value::build_text("2000-02-29")
    );
    assert_eq!(
        select(&connection, "DATE_ADD('2000-02-29', 1, 'YEAR')"),
        Value::build_text("2001-02-28")
    );
    assert_eq!(
        select(&connection, "DATE_ADD('2000-01-01', 1, 'QUARTER')"),
        Value::build_text("2000-04-01")
    );
    assert_eq!(
        select(&connection, "DATE_SUB('2018-05-01', 1, 'YEAR')"),
        Value::build_text("2017-05-01")
    );
    assert_eq!(
        select(&connection, "DATE_SUB('2018-05-01', '1 1:1:1', 'DAY')"),
        Value::build_text("2018-04-30")
    );
    assert_eq!(
        select(&connection, "ADDDATE('2000-01-01', 31)"),
        Value::build_text("2000-02-01")
    );
}

#[test]
fn date_add_with_clock_units_returns_a_datetime() {
    let connection = crate::open_memory().expect("in-memory database opens");
    assert_eq!(
        select(&connection, "DATE_ADD('2018-05-02', 1, 'HOUR')"),
        Value::build_text("2018-05-02 01:00:00")
    );
    assert_eq!(
        select(&connection, "DATE_ADD('2000-01-01', '1 1', 'DAY_HOUR')"),
        Value::build_text("2000-01-02 01:00:00")
    );
    assert_eq!(
        select(&connection, "DATE_ADD('2000-01-01', '-1:2', 'HOUR_SECOND')"),
        Value::build_text("1999-12-31 23:58:58")
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('2000-01-01 00:00:00.5', 1, 'SECOND')"
        ),
        Value::build_text("2000-01-01 00:00:01.500000")
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('2000-01-01 10:00:00', '1.5', 'SECOND')"
        ),
        Value::build_text("2000-01-01 10:00:01.500000")
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('2000-01-01', '5', 'SECOND_MICROSECOND')"
        ),
        Value::build_text("2000-01-01 00:00:00.500000")
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('1992-12-31 23:59:59.000002', '1.999999', 'SECOND_MICROSECOND')"
        ),
        Value::build_text("1993-01-01 00:00:01.000001")
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('1900-01-01 00:00:00', '-1 10', 'DAY_HOUR')"
        ),
        Value::build_text("1899-12-30 14:00:00")
    );
}

#[test]
fn date_add_out_of_range_or_malformed_is_null() {
    let connection = crate::open_memory().expect("in-memory database opens");
    assert_eq!(
        select(&connection, "DATE_ADD('9999-12-31', 1, 'DAY')"),
        Value::Null
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('2000-01-01', '1:2:3:4', 'HOUR_SECOND')"
        ),
        Value::Null
    );
    assert_eq!(
        select(&connection, "DATE_ADD('10:00:00', 1, 'HOUR')"),
        Value::Null
    );
    assert_eq!(
        select(&connection, "DATE_ADD('0000-00-00', 1, 'DAY')"),
        Value::Null
    );
    assert_eq!(
        select(&connection, "DATE_ADD('2000-01-01', NULL, 'DAY')"),
        Value::Null
    );
    assert_eq!(
        select(
            &connection,
            "DATE_ADD('2000-01-01', -9223372036854775808, 'DAY')"
        ),
        Value::Null
    );
}

#[test]
fn timestamp_add_and_diff_take_the_unit_first() {
    let connection = crate::open_memory().expect("in-memory database opens");
    assert_eq!(
        select(&connection, "TIMESTAMPADD('WEEK', 1, '2003-01-02')"),
        Value::build_text("2003-01-09")
    );
    assert_eq!(
        select(&connection, "TIMESTAMPADD('DAY', '1:1', '2018-05-01')"),
        Value::build_text("2018-05-02")
    );
    assert_eq!(
        select(&connection, "TIMESTAMPADD('DAY', 1, '0000-00-00')"),
        Value::Null
    );
    assert_eq!(
        select(
            &connection,
            "TIMESTAMPDIFF('QUARTER', '0000-01-01', '9999-12-31 23:59:59.999999')"
        ),
        Value::from_i64(39999)
    );
    assert_eq!(
        select(
            &connection,
            "TIMESTAMPDIFF('MICROSECOND', '0000-03-01', '9999-12-31 23:59:59.999999')"
        ),
        Value::from_i64(315564335999999999)
    );
    assert_eq!(
        select(
            &connection,
            "TIMESTAMPDIFF('MONTH', '2000-01-31', '2000-02-29')"
        ),
        Value::from_i64(0)
    );
    assert_eq!(
        select(
            &connection,
            "TIMESTAMPDIFF('MONTH', '2000-02-01 09:00:00', '2000-01-01 10:00:00')"
        ),
        Value::from_i64(0)
    );
    assert_eq!(
        select(
            &connection,
            "TIMESTAMPDIFF('SECOND', '2000-01-01 00:00:01', '2000-01-01')"
        ),
        Value::from_i64(-1)
    );
}

#[test]
fn extract_reads_dates_or_times_by_unit() {
    let connection = crate::open_memory().expect("in-memory database opens");
    assert_eq!(
        select(&connection, "EXTRACT('DAY_HOUR', '25:00:00')"),
        Value::from_i64(25)
    );
    assert_eq!(
        select(&connection, "EXTRACT('DAY_HOUR', '2000-01-02 10:00:00')"),
        Value::from_i64(210)
    );
    assert_eq!(
        select(&connection, "EXTRACT('DAY_MICROSECOND', '-25:00:00.5')"),
        Value::from_i64(-250000500000)
    );
    assert_eq!(
        select(&connection, "EXTRACT('DAY', '25:00:00')"),
        Value::from_i64(0)
    );
    assert_eq!(
        select(&connection, "EXTRACT('WEEK', 0)"),
        Value::from_i64(613566757)
    );
    assert_eq!(
        select(&connection, "EXTRACT('YEAR_MONTH', '2000-01-02')"),
        Value::from_i64(200001)
    );
    assert_eq!(
        select(&connection, "EXTRACT('DAY', '0000-00-00')"),
        Value::Null
    );
}

#[test]
fn interval_functions_reject_unknown_units() {
    let connection = crate::open_memory().expect("in-memory database opens");
    for sql in [
        "SELECT DATE_ADD('2000-01-01', 1, 'FORTNIGHT')",
        "SELECT TIMESTAMPDIFF('DAY_HOUR', '2000-01-01', '2000-01-02')",
    ] {
        let error = connection
            .prepare(sql)
            .and_then(|mut statement| statement.run_collect_rows())
            .expect_err("unknown units must fail");
        assert!(error.to_string().contains("unit"), "{error}");
    }
}

fn select(connection: &Arc<Connection>, expression: &str) -> Value {
    let mut statement = connection
        .prepare(format!("SELECT {expression}"))
        .expect("statement prepares");
    let mut rows = statement.run_collect_rows().expect("statement runs");
    assert_eq!(rows.len(), 1, "{expression}");
    rows.remove(0).remove(0)
}
