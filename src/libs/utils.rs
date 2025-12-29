use eyre::Result;

pub fn get_log_id() -> u64 {
    chrono::Utc::now().timestamp_micros() as _
}

pub fn get_conn_id() -> u32 {
    chrono::Utc::now().timestamp_micros() as _
}

pub fn get_time_milliseconds() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub fn get_time_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}
pub fn hex_decode(s: &[u8]) -> Result<Vec<u8>> {
    if s.starts_with(b"0x") {
        Ok(hex::decode(&s[2..])?)
    } else {
        Ok(hex::decode(s)?)
    }
}

/// format decimal by specific significant figures
pub fn decimal_sf(num: rust_decimal::Decimal, sig_figs: usize) -> rust_decimal::Decimal {
    num.round_sf(sig_figs as _).unwrap()
}

/// Aligns the precision of one `f64` value to match another `f64` value.
pub fn align_precision(a: f64, b: f64) -> f64 {
    let precision_b = count_dp(b);
    let precision_a = format!("{:.0$}", { precision_b });
    let aligned_a = format!("{:.*}", precision_a.parse::<usize>().unwrap(), a);
    aligned_a.parse().unwrap()
}

pub fn count_dp(num: f64) -> usize {
    // Convert the f64 to a string representation
    let num_str = format!("{num}");

    // Find the position of the decimal point
    if let Some(decimal_index) = num_str.find('.') {
        // Count the number of characters after the decimal point
        num_str.len() - decimal_index - 1
    } else {
        // If there is no decimal point, return 0
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_dp() {
        assert_eq!(count_dp(1.5), 1);
        assert_eq!(count_dp(1.1), 1);
    }

    #[test]
    fn test_align_precision() {
        let a = 123.456789;
        let b = 78.9;
        let aligned_a = align_precision(a, b);
        assert_eq!(aligned_a, 123.5);

        let a = 2.37;
        let b = 631.3;
        let aligned_a = align_precision(a, b);
        assert_eq!(aligned_a, 2.4);
    }
}
