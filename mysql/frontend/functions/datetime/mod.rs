use turso_core::{Result, Value};

mod arg;
mod arith;
mod build;
mod calendar;
mod decimal;
mod epoch;
mod format;
mod parse;
mod parts;
mod str_to_date;
mod temporal;
#[cfg(test)]
mod tests;
mod zone;

use arg::Arg;

pub(super) fn accepts(name: &str, arg_count: usize) -> bool {
    find(name, arg_count).is_some()
}

pub(super) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    let function = find(name, args.len())?;
    let args: Vec<Arg> = args.iter().map(Arg::new).collect();
    Some((function.run)(&args))
}

fn find(name: &str, arg_count: usize) -> Option<&'static Function> {
    FUNCTIONS
        .iter()
        .find(|function| function.name == name && function.arities.contains(&arg_count))
}

type Implementation = fn(&[Arg]) -> Result<Value>;

struct Function {
    name: &'static str,
    arities: &'static [usize],
    run: Implementation,
}

const FUNCTIONS: &[Function] = &[
    function("YEAR", &[1], parts::year),
    function("MONTH", &[1], parts::month),
    function("DAY", &[1], parts::day_of_month),
    function("DAYOFMONTH", &[1], parts::day_of_month),
    function("QUARTER", &[1], parts::quarter),
    function("DAYOFYEAR", &[1], parts::day_of_year),
    function("WEEKDAY", &[1], parts::weekday),
    function("DAYOFWEEK", &[1], parts::day_of_week),
    function("WEEK", &[1, 2], parts::week),
    function("WEEKOFYEAR", &[1], parts::week_of_year),
    function("YEARWEEK", &[1, 2], parts::year_week),
    function("TO_DAYS", &[1], parts::to_days),
    function("TO_SECONDS", &[1], parts::to_seconds),
    function("MONTHNAME", &[1], parts::month_name),
    function("DAYNAME", &[1], parts::day_name),
    function("HOUR", &[1], parts::hour),
    function("MINUTE", &[1], parts::minute),
    function("SECOND", &[1], parts::second),
    function("MICROSECOND", &[1], parts::microsecond),
    function("TIME_TO_SEC", &[1], parts::time_to_sec),
    function("MAKEDATE", &[2], build::make_date),
    function("MAKETIME", &[3], build::make_time),
    function("SEC_TO_TIME", &[1], build::sec_to_time),
    function("FROM_DAYS", &[1], build::from_days),
    function("LAST_DAY", &[1], build::last_day),
    function("PERIOD_ADD", &[2], build::period_add),
    function("PERIOD_DIFF", &[2], build::period_diff),
    function("DATE", &[1], build::date),
    function("TIME", &[1], build::time),
    function("TIMESTAMP", &[1], build::timestamp),
    function("__MYSQL_CAST_AS_DATE", &[1], build::date),
    function("__MYSQL_CAST_AS_DATETIME", &[2], build::cast_as_datetime),
    function("__MYSQL_CAST_AS_TIME", &[2], build::cast_as_time),
    function("DATE_FORMAT", &[2], build::date_format),
    function("TIME_FORMAT", &[2], build::time_format),
    function("STR_TO_DATE", &[2], str_to_date::str_to_date),
    function("GET_FORMAT", &[2], format::get_format),
    function("CONVERT_TZ", &[3], epoch::convert_tz),
    function("FROM_UNIXTIME", &[1, 2], epoch::from_unixtime),
    function("UNIX_TIMESTAMP", &[0, 1], epoch::unix_timestamp),
    function("TIMESTAMP", &[2], arith::timestamp_with_time),
    function("DATE_ADD", &[3], arith::date_add),
    function("DATE_SUB", &[3], arith::date_sub),
    function("ADDDATE", &[2], arith::add_days),
    function("SUBDATE", &[2], arith::subtract_days),
    function("TIMESTAMPADD", &[3], arith::timestamp_add),
    function("TIMESTAMPDIFF", &[3], arith::timestamp_diff),
    function("EXTRACT", &[2], arith::extract),
    function("DATEDIFF", &[2], arith::date_diff),
    function("TIMEDIFF", &[2], arith::time_diff),
    function("ADDTIME", &[2], arith::add_time),
    function("SUBTIME", &[2], arith::subtract_time),
    function("NOW", &[0, 1], epoch::now_datetime),
    function("SYSDATE", &[0, 1], epoch::now_datetime),
    function("LOCALTIME", &[0, 1], epoch::now_datetime),
    function("LOCALTIMESTAMP", &[0, 1], epoch::now_datetime),
    function("CURRENT_TIMESTAMP", &[0, 1], epoch::now_datetime),
    function("UTC_TIMESTAMP", &[0, 1], epoch::now_datetime),
    function("CURDATE", &[0], epoch::now_date),
    function("CURRENT_DATE", &[0], epoch::now_date),
    function("UTC_DATE", &[0], epoch::now_date),
    function("CURTIME", &[0, 1], epoch::now_time),
    function("CURRENT_TIME", &[0, 1], epoch::now_time),
    function("UTC_TIME", &[0, 1], epoch::now_time),
];

const fn function(name: &'static str, arities: &'static [usize], run: Implementation) -> Function {
    Function { name, arities, run }
}
