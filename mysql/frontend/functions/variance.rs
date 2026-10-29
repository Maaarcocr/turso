//! `VARIANCE`, `VAR_SAMP`, `STD` and `STDDEV_SAMP` and their synonyms, which
//! MySQL computes with Welford's recurrence over the arguments read as
//! DOUBLE. Lowering reads the argument as a DOUBLE before it reaches them.
use crate::types::unsupported_value;
use turso_core::native_ext::{Aggregate, AggregateFunction};
use turso_core::{ExternalFunc, IOResult, LimboError, Numeric, OpenOptions, Result, Value};

pub(crate) fn register(options: OpenOptions) -> OpenOptions {
    [
        ("__mysql_var_pop", false, false),
        ("__mysql_var_samp", true, false),
        ("__mysql_stddev_pop", false, true),
        ("__mysql_stddev_samp", true, true),
    ]
    .into_iter()
    .fold(options, |options, (name, sample, root)| {
        let function = ExternalFunc::new_native_aggregate(
            name.to_string(),
            1,
            Variance { name, sample, root },
        )
        .expect("one argument is a valid aggregate arity");
        options.extension_function(function)
    })
}

#[derive(Debug, Clone, Copy)]
struct Variance {
    name: &'static str,
    sample: bool,
    root: bool,
}

impl AggregateFunction for Variance {
    type Accumulator = Recurrence;

    fn create_accumulator(&self) -> Result<Recurrence> {
        Ok(Recurrence {
            variance: *self,
            count: 0,
            mean: 0.0,
            squares: 0.0,
        })
    }
}

#[derive(Debug)]
struct Recurrence {
    variance: Variance,
    count: u64,
    mean: f64,
    squares: f64,
}

impl Aggregate for Recurrence {
    fn step(&mut self, args: &[Value]) -> std::result::Result<IOResult<()>, Box<LimboError>> {
        let value = match &args[0] {
            Value::Null => return Ok(IOResult::Done(())),
            Value::Numeric(Numeric::Float(value)) => f64::from(*value),
            other => {
                return Err(Box::new(LimboError::InternalError(format!(
                    "{} of {other}, which lowering reads as a DOUBLE",
                    self.variance.name
                ))))
            }
        };
        self.count += 1;
        if self.count == 1 {
            self.mean = value;
        } else {
            let previous = self.mean;
            let difference = value - previous;
            self.mean = previous + difference / self.count as f64;
            self.squares += difference * (value - self.mean);
        }
        Ok(IOResult::Done(()))
    }

    fn finalize(&mut self) -> std::result::Result<IOResult<Value>, Box<LimboError>> {
        let sample = u64::from(self.variance.sample);
        if self.count <= sample {
            return Ok(IOResult::Done(Value::Null));
        }
        let variance = if self.count == 1 {
            0.0
        } else {
            self.squares / (self.count - sample) as f64
        };
        if !variance.is_finite() {
            return Err(Box::new(unsupported_value(format!(
                "{} that overflows a DOUBLE",
                self.variance.name
            ))));
        }
        let result = if self.variance.root {
            variance.sqrt()
        } else {
            variance
        };
        Ok(IOResult::Done(Value::from_f64(result)))
    }
}
