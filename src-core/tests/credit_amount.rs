use aiwork_core::CreditAmount;

#[test]
fn credit_amount_round_trips_six_decimal_credits_as_string() {
    let amount = CreditAmount::parse("12.34", "credits").unwrap();

    assert_eq!(amount.as_microcredits(), 12_340_000);
    assert_eq!(amount.to_string(), "12.340000");
    assert_eq!(serde_json::to_string(&amount).unwrap(), "\"12.340000\"");

    let one_microcredit: CreditAmount = serde_json::from_str("\"0.000001\"").unwrap();
    assert_eq!(one_microcredit.as_microcredits(), 1);
}

#[test]
fn credit_amount_rejects_invalid_precision_and_non_credit_units() {
    for value in ["", "-1", "+1", "1e-6", "0.0000001", "NaN", "1x"] {
        assert!(CreditAmount::parse(value, "credits").is_err(), "accepted {value:?}");
    }

    assert!(CreditAmount::parse("1", "tokens").is_err());
}

#[test]
fn credit_amount_checked_arithmetic_rejects_overflow_and_underflow() {
    let maximum = CreditAmount::parse("9223372036854.775807", "credits").unwrap();
    let one = CreditAmount::parse("0.000001", "credits").unwrap();

    assert!(maximum.checked_add(one).is_none());
    assert!(CreditAmount::default().checked_sub(one).is_none());
    assert_eq!(
        CreditAmount::parse("0.999999", "credits")
            .unwrap()
            .checked_add(one)
            .unwrap()
            .as_microcredits(),
        1_000_000
    );
}
