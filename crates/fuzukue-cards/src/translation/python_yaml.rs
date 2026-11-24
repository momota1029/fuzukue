use std::sync::LazyLock;

use anyhow::{Context, Result};
use num_bigint::BigInt;
use regex::Regex;
use serde_yaml::Value;

use super::yaml;

pub(super) const REPR_TAG: &str = "!fuzukue-python-repr";

/// Preserve PyYAML's YAML 1.1 scalar types when serde_yaml constructs the mapping.
pub(super) fn load(front: &str) -> Result<Value> {
    let mut replacements = Vec::new();
    yaml::visit(front, |event, span| {
        let yaml::Event::Scalar(scalar) = event else {
            return Ok(());
        };
        if span.is_empty() || scalar.value == "<<" {
            return Ok(());
        }
        let Some(kind) = scalar
            .tag
            .and_then(|tag| tag.strip_prefix("tag:yaml.org,2002:"))
            .or_else(|| scalar.plain.then(|| implicit_type(scalar.value)))
        else {
            return Ok(());
        };
        let value = scalar.value;
        let replacement = match kind {
            "str" => serde_json::to_string(value)?,
            "null" => "null".into(),
            "bool" => match value.to_ascii_lowercase().as_str() {
                "yes" | "true" | "on" => "true".into(),
                "no" | "false" | "off" => "false".into(),
                _ => anyhow::bail!("invalid YAML boolean"),
            },
            "int" => tagged(&integer(value)?.to_string())?,
            "float" => tagged(&float(value)?)?,
            "timestamp" => tagged(&timestamp(value)?)?,
            _ => return Ok(()),
        };
        let anchor = scalar
            .anchor
            .map_or_else(String::new, |name| format!("&{name} "));
        let start = if front[..span.start].ends_with(':') {
            " "
        } else {
            ""
        };
        let end = if front[span.clone()].ends_with('\n') {
            "\n"
        } else {
            ""
        };
        replacements.push((span, format!("{start}{anchor}{replacement}{end}")));
        Ok(())
    })?;
    let mut adjusted = front.to_owned();
    for (span, replacement) in replacements.into_iter().rev() {
        adjusted.replace_range(span, &replacement);
    }
    Ok(serde_yaml::from_str(&adjusted)?)
}

fn tagged(repr: &str) -> Result<String> {
    Ok(format!("{REPR_TAG} {}", serde_json::to_string(repr)?))
}

fn implicit_type(value: &str) -> &'static str {
    static INT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[+-]?(?:0b[01_]+|0[0-7_]+|0|[1-9][0-9_]*|0x[0-9a-fA-F_]+|[1-9][0-9_]*(?::[0-5]?[0-9])+)$").unwrap()
    });
    static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:[+-]?[0-9][0-9_]*\.[0-9_]*(?:[eE][+-][0-9]+)?|\.[0-9][0-9_]*(?:[eE][+-][0-9]+)?|[+-]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[+-]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$").unwrap()
    });
    match value {
        "" | "~" | "null" | "Null" | "NULL" => "null",
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" | "no" | "No"
        | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => "bool",
        _ if INT.is_match(value) => "int",
        _ if FLOAT.is_match(value) => "float",
        _ if TIMESTAMP
            .captures(value)
            .is_some_and(|m| m.get(4).is_some() || (m[2].len() == 2 && m[3].len() == 2)) =>
        {
            "timestamp"
        }
        _ => "str",
    }
}

fn integer(value: &str) -> Result<BigInt> {
    let value = value.replace('_', "");
    let digits = value.strip_prefix(['+', '-']).unwrap_or(&value);
    let number = if digits.contains(':') {
        digits.split(':').try_fold(BigInt::from(0), |acc, part| {
            Ok::<BigInt, anyhow::Error>(acc * 60 + part.parse::<BigInt>()?)
        })?
    } else {
        let (digits, radix) = if let Some(digits) = digits.strip_prefix("0b") {
            (digits, 2)
        } else if let Some(digits) = digits.strip_prefix("0x") {
            (digits, 16)
        } else if digits.starts_with('0') {
            (digits, 8)
        } else {
            (digits, 10)
        };
        BigInt::parse_bytes(digits.as_bytes(), radix).context("invalid YAML integer")?
    };
    Ok(if value.starts_with('-') {
        -number
    } else {
        number
    })
}

fn float(value: &str) -> Result<String> {
    let value = value.replace('_', "").to_ascii_lowercase();
    let digits = value.strip_prefix(['+', '-']).unwrap_or(&value);
    let n = match digits {
        ".inf" => f64::INFINITY,
        ".nan" => return Ok("nan".into()),
        _ if digits.contains(':') => {
            let mut number = 0.0;
            let mut base = 1.0;
            for part in digits.rsplit(':') {
                number += part.parse::<f64>()? * base;
                base *= 60.0;
            }
            number
        }
        _ => digits.parse()?,
    };
    let n = if value.starts_with('-') { -n } else { n };
    let repr = format!("{n:?}");
    if let Some((mantissa, exp)) = repr.split_once('e') {
        Ok(format!("{mantissa}e{:+03}", exp.parse::<i32>()?))
    } else {
        Ok(repr)
    }
}

static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})",
        r"(?:(?:[Tt]|[ \t]+)([0-9]{1,2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]*))?",
        r"(?:[ \t]*(Z|[+-]([0-9]{1,2})(?::([0-9]{2}))?))?)?$",
    ))
    .unwrap()
});

fn timestamp(value: &str) -> Result<String> {
    let m = TIMESTAMP
        .captures(value)
        .context("invalid YAML timestamp")?;
    let part = |i| -> i32 { m.get(i).map_or(0, |s| s.as_str().parse().unwrap()) };
    let (year, month, day) = (part(1), part(2), part(3));
    if m.get(4).is_none() {
        return Ok(format!("datetime.date({year}, {month}, {day})"));
    }
    let (hour, minute, second) = (part(4), part(5), part(6));
    let mut repr = format!("datetime.datetime({year}, {month}, {day}, {hour}, {minute}");
    let fraction = m.get(7).map_or("", |s| s.as_str());
    let micros: i32 = format!("{:0<6}", &fraction[..fraction.len().min(6)]).parse()?;
    if second != 0 || micros != 0 {
        repr.push_str(&format!(", {second}"));
    }
    if micros != 0 {
        repr.push_str(&format!(", {micros}"));
    }
    if let Some(zone) = m.get(8).map(|s| s.as_str()) {
        let seconds = (part(9) * 60 + part(10)) * 60;
        let seconds = if zone.starts_with('-') {
            -seconds
        } else {
            seconds
        };
        match seconds {
            0 => repr.push_str(", tzinfo=datetime.timezone.utc"),
            1.. => repr.push_str(&format!(
                ", tzinfo=datetime.timezone(datetime.timedelta(seconds={seconds}))"
            )),
            _ => repr.push_str(&format!(
                ", tzinfo=datetime.timezone(datetime.timedelta(days=-1, seconds={}))",
                seconds + 86400
            )),
        }
    }
    repr.push(')');
    Ok(repr)
}
