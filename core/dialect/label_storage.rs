use crate::numeric::Numeric;
use crate::{LimboError, Result, Value};

const MAX_SET_LABELS: usize = 64;

pub(crate) fn enumeration_encode(value: &Value, labels: &Value) -> Result<Value> {
    let labels = parse_labels(labels)?;
    let text = expect_text(value, "enumeration")?;
    match labels.iter().position(|label| label == text) {
        Some(index) => Ok(Value::from_i64(index as i64 + 1)),
        None => Err(invalid_value("enumeration", text)),
    }
}

pub(crate) fn enumeration_decode(value: &Value, labels: &Value) -> Result<Value> {
    let labels = parse_labels(labels)?;
    let position = match value {
        Value::Numeric(Numeric::Integer(position)) => *position,
        other => {
            return Err(LimboError::Corrupt(format!(
                "stored enumeration value is not an integer: {other}"
            )))
        }
    };
    usize::try_from(position)
        .ok()
        .and_then(|position| position.checked_sub(1))
        .and_then(|index| labels.get(index))
        .map(|label| Value::build_text(label.clone()))
        .ok_or_else(|| {
            LimboError::Corrupt(format!(
                "stored enumeration position {position} has no label"
            ))
        })
}

pub(crate) fn label_set_encode(value: &Value, labels: &Value) -> Result<Value> {
    let labels = parse_labels(labels)?;
    if labels.len() > MAX_SET_LABELS {
        return Err(LimboError::Constraint(format!(
            "a label set has at most {MAX_SET_LABELS} labels, got {}",
            labels.len()
        )));
    }
    let text = expect_text(value, "label set")?;
    let mut mask = 0_u64;
    if !text.is_empty() {
        for member in text.split(',') {
            let Some(index) = labels.iter().position(|label| label == member) else {
                return Err(invalid_value("label set", text));
            };
            mask |= 1 << index;
        }
    }
    Ok(match i64::try_from(mask) {
        Ok(small) => Value::from_i64(small),
        Err(_) => Value::from_blob(mask.to_be_bytes().to_vec()),
    })
}

pub(crate) fn label_set_decode(value: &Value, labels: &Value) -> Result<Value> {
    let labels = parse_labels(labels)?;
    let mask = match value {
        Value::Numeric(Numeric::Integer(mask)) if *mask >= 0 => *mask as u64,
        Value::Blob(bytes) if bytes.len() == 8 => {
            u64::from_be_bytes(bytes.as_slice().try_into().expect("checked length"))
        }
        other => {
            return Err(LimboError::Corrupt(format!(
                "stored label set is not a bit mask: {other}"
            )))
        }
    };
    if labels.len() < MAX_SET_LABELS && mask >> labels.len() != 0 {
        return Err(LimboError::Corrupt(format!(
            "stored label set {mask} has bits without a label"
        )));
    }
    let members: Vec<&str> = labels
        .iter()
        .enumerate()
        .filter(|(index, _)| mask & (1 << index) != 0)
        .map(|(_, label)| label.as_str())
        .collect();
    Ok(Value::build_text(members.join(",")))
}

fn parse_labels(labels: &Value) -> Result<Vec<String>> {
    let Value::Text(list) = labels else {
        return Err(bad_label_list(&labels.to_string()));
    };
    let list = list.as_str();
    let mut labels = Vec::new();
    let mut chars = list.chars().peekable();
    loop {
        if chars.next() != Some('\'') {
            return Err(bad_label_list(list));
        }
        let mut label = String::new();
        loop {
            match chars.next() {
                Some('\'') if chars.peek() == Some(&'\'') => {
                    chars.next();
                    label.push('\'');
                }
                Some('\'') => break,
                Some(c) => label.push(c),
                None => return Err(bad_label_list(list)),
            }
        }
        labels.push(label);
        match chars.next() {
            None => return Ok(labels),
            Some(',') => {}
            Some(_) => return Err(bad_label_list(list)),
        }
    }
}

fn expect_text<'a>(value: &'a Value, type_name: &str) -> Result<&'a str> {
    match value {
        Value::Text(text) => Ok(text.as_str()),
        other => Err(invalid_value(type_name, &other.to_string())),
    }
}

fn invalid_value(type_name: &str, text: &str) -> LimboError {
    LimboError::Constraint(format!("invalid value for {type_name}: {text}"))
}

fn bad_label_list(list: &str) -> LimboError {
    LimboError::InternalError(format!("labels must be a list of text literals: {list}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::build_text(value.to_string())
    }

    #[test]
    fn enumerations_store_the_position_of_the_label() {
        let labels = text("'small','large','it''s'");
        assert_eq!(
            enumeration_encode(&text("large"), &labels).unwrap(),
            Value::from_i64(2)
        );
        assert_eq!(
            enumeration_decode(&Value::from_i64(3), &labels).unwrap(),
            text("it's")
        );
        assert!(enumeration_encode(&text("medium"), &labels).is_err());
        assert!(enumeration_decode(&Value::from_i64(0), &labels).is_err());
        assert!(enumeration_decode(&Value::from_i64(4), &labels).is_err());
    }

    #[test]
    fn label_sets_store_a_bit_mask_in_declared_order() {
        let labels = text("'a','b','c'");
        assert_eq!(
            label_set_encode(&text("c,a"), &labels).unwrap(),
            Value::from_i64(0b101)
        );
        assert_eq!(
            label_set_decode(&Value::from_i64(0b101), &labels).unwrap(),
            text("a,c")
        );
        assert_eq!(
            label_set_encode(&text(""), &labels).unwrap(),
            Value::from_i64(0)
        );
        assert!(label_set_encode(&text("a,d"), &labels).is_err());
        assert!(label_set_decode(&Value::from_i64(0b1000), &labels).is_err());

        let sixty_four: Vec<String> = (0..64).map(|i| format!("'l{i}'")).collect();
        let sixty_four = text(&sixty_four.join(","));
        let last = label_set_encode(&text("l63"), &sixty_four).unwrap();
        assert_eq!(last, Value::from_blob(vec![0x80, 0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(label_set_decode(&last, &sixty_four).unwrap(), text("l63"));
    }
}
