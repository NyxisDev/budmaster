//! Проверка значений из операций. Количества и деньги приходят строками («2.5», «185.00»)
//! и в базу передаются строками: через числа с плавающей точкой они не проходят.

use serde::Deserialize;

/// Единица учёта остатка: штуки (базовая единица товара) или пачки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, sqlx::Type)]
#[sqlx(type_name = "stock_unit", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum StockUnit {
    Unit,
    Pack,
}

impl StockUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unit => "unit",
            Self::Pack => "pack",
        }
    }
}

const MAX_WHOLE_DIGITS: usize = 8;

/// Положительное количество: до 8 цифр до точки и до 3 после; без дробной части,
/// если `allow_fraction = false`. Возвращает нормализованную строку.
pub fn parse_qty(raw: &str, allow_fraction: bool) -> Option<String> {
    let value = parse_decimal(raw, 3)?;
    if !allow_fraction && value.contains('.') {
        return None;
    }
    if is_zero(&value) { None } else { Some(value) }
}

/// Изменение количества со знаком (корректировка): «-2», «+1.5». Ноль не допускается.
pub fn parse_delta(raw: &str, allow_fraction: bool) -> Option<String> {
    let raw = raw.trim();
    let (sign, digits) = match raw.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", raw.strip_prefix('+').unwrap_or(raw)),
    };
    parse_qty(digits, allow_fraction).map(|value| format!("{sign}{value}"))
}

/// Цена за единицу (закупочная) — до 4 знаков после точки, ноль допустим.
pub fn parse_unit_cost(raw: &str) -> Option<String> {
    parse_decimal(raw, 4)
}

/// Неотрицательное десятичное число; запятая принимается как точка. Без ведущих нулей
/// и без лишних нулей в дробной части.
fn parse_decimal(raw: &str, max_fraction: usize) -> Option<String> {
    let normalized = raw.trim().replace(',', ".");
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty()
        || whole.len() > MAX_WHOLE_DIGITS
        || fraction.len() > max_fraction
        || !digits(whole)
        || !digits(fraction)
        || normalized.ends_with('.')
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let fraction = fraction.trim_end_matches('0');
    Some(if fraction.is_empty() { whole.to_owned() } else { format!("{whole}.{fraction}") })
}

fn is_zero(value: &str) -> bool {
    value.chars().all(|c| c == '0' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(parse_qty("2", false).as_deref(), Some("2"));
        assert_eq!(parse_qty("2.500", true).as_deref(), Some("2.5"));
        assert_eq!(parse_qty("0,125", true).as_deref(), Some("0.125"));
        assert_eq!(parse_qty("007", false).as_deref(), Some("7"));
        for bad in ["0", "0.000", "-1", "1.5", "", "abc", "1e3", "1.2345", "123456789"] {
            assert_eq!(parse_qty(bad, bad == "1.5" && false), None, "{bad}");
        }
        assert_eq!(parse_qty("1.5", false), None, "дробное количество для штук");
    }

    #[test]
    fn deltas() {
        assert_eq!(parse_delta("-2", false).as_deref(), Some("-2"));
        assert_eq!(parse_delta("+1.5", true).as_deref(), Some("1.5"));
        assert_eq!(parse_delta("-0", false), None);
        assert_eq!(parse_delta("--1", false), None);
    }

    #[test]
    fn unit_costs() {
        assert_eq!(parse_unit_cost("0").as_deref(), Some("0"));
        assert_eq!(parse_unit_cost("12.3456").as_deref(), Some("12.3456"));
        assert_eq!(parse_unit_cost("12.34567"), None);
        assert_eq!(parse_unit_cost("-1"), None);
    }
}
