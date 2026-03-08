use alloy::primitives::{Address, Bytes, FixedBytes};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};

/// Oracle response matching Rain's SignedContextV1 format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleResponse {
    pub signer: Address,
    pub context: Vec<FixedBytes<32>>,
    pub signature: Bytes,
}

/// Build the signed context array from a USD price and expiry.
///
/// The price is the relevant side of the NBBO:
/// - For buy orders (input=USDC, output=tStock): ask price (what you'd pay to buy)
/// - For sell orders (input=tStock, output=USDC): 1/bid price (inverted)
///
/// Context layout (all Rain DecimalFloats):
/// - [0]: price
/// - [1]: expiry timestamp
pub fn build_context(
    price: f64,
    expiry: u64,
    inverted: bool,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let price_str = format_price(price);
    let price_float = Float::parse(price_str.clone())
        .map_err(|e| anyhow::anyhow!("Failed to parse price '{}' as Rain float: {:?}", price_str, e))?;

    let final_price = if inverted {
        let one = Float::parse("1".to_string())
            .map_err(|e| anyhow::anyhow!("Failed to parse '1' as Rain float: {:?}", e))?;
        (one / price_float)
            .map_err(|e| anyhow::anyhow!("Failed to invert price: {:?}", e))?
    } else {
        price_float
    };

    let expiry_str = expiry.to_string();
    let expiry_float = Float::parse(expiry_str.clone())
        .map_err(|e| anyhow::anyhow!("Failed to parse expiry '{}' as Rain float: {:?}", expiry_str, e))?;

    let price_bytes: alloy::primitives::B256 = final_price.into();
    let expiry_bytes: alloy::primitives::B256 = expiry_float.into();

    Ok(vec![
        FixedBytes::from(price_bytes),
        FixedBytes::from(expiry_bytes),
    ])
}

/// Format an f64 price as a string suitable for Float::parse.
/// Avoids scientific notation which Float::parse may not handle.
fn format_price(price: f64) -> String {
    // Use enough decimal places to preserve precision
    let s = format!("{:.10}", price);
    // Trim trailing zeros after decimal point
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_price() {
        assert_eq!(format_price(185.42), "185.42");
        assert_eq!(format_price(1000.0), "1000");
        assert_eq!(format_price(0.0005), "0.0005");
        assert_eq!(format_price(99999.99), "99999.99");
    }

    #[test]
    fn test_build_context_as_is() {
        let ctx = build_context(185.42, 1700000000, false).unwrap();
        assert_eq!(ctx.len(), 2);

        let price_float = Float::from(alloy::primitives::B256::from(ctx[0]));
        let formatted = price_float.format().unwrap();
        assert_eq!(formatted, "185.42");
    }

    #[test]
    fn test_build_context_inverted() {
        let ctx = build_context(200.0, 1700000000, true).unwrap();
        assert_eq!(ctx.len(), 2);

        let price_float = Float::from(alloy::primitives::B256::from(ctx[0]));
        let formatted = price_float.format().unwrap();
        assert_eq!(formatted, "0.005"); // 1/200 = 0.005
    }
}
