//! Права работника — битовая маска `users.permissions` (docs/design.md, раздел 7).
//! У владельца все права считаются включёнными независимо от маски.

pub const PRODUCTS_EDIT: i64 = 1 << 0;
pub const PRICES_EDIT: i64 = 1 << 1;
pub const CATEGORIES_EDIT: i64 = 1 << 2;
pub const SUPPLIERS_EDIT: i64 = 1 << 3;
pub const WAREHOUSES_EDIT: i64 = 1 << 4;

/// Все биты, которые сейчас что-то значат. Остальные выставить нельзя.
pub const ALL: i64 = PRODUCTS_EDIT | PRICES_EDIT | CATEGORIES_EDIT | SUPPLIERS_EDIT | WAREHOUSES_EDIT;

/// Маска состоит только из известных битов.
pub fn is_valid_mask(mask: i64) -> bool {
    mask >= 0 && mask & !ALL == 0
}

/// В маске включены все запрошенные биты.
pub fn has(mask: i64, required: i64) -> bool {
    mask & required == required
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_accepts_only_known_bits() {
        assert!(is_valid_mask(0));
        assert!(is_valid_mask(ALL));
        assert!(is_valid_mask(PRICES_EDIT | WAREHOUSES_EDIT));
        assert!(!is_valid_mask(1 << 5));
        assert!(!is_valid_mask(ALL | (1 << 40)));
        assert!(!is_valid_mask(-1));
    }

    #[test]
    fn has_requires_every_bit() {
        let mask = PRODUCTS_EDIT | PRICES_EDIT;
        assert!(has(mask, PRODUCTS_EDIT));
        assert!(has(mask, PRODUCTS_EDIT | PRICES_EDIT));
        assert!(!has(mask, PRODUCTS_EDIT | SUPPLIERS_EDIT));
        assert!(!has(0, WAREHOUSES_EDIT));
    }
}
