//! MySQL's reading of dates and times from strings and numbers, shared by
//! the parser, which types some results by it, and the frontend functions.

pub mod calendar;
pub mod parse;
pub mod temporal;
