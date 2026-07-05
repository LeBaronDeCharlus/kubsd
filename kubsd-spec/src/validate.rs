use crate::error::SpecError;
use ipnet::IpNet;

pub fn validate_name(name: &str) -> Result<(), SpecError> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(SpecError::InvalidName(name.to_string()))
    }
}

pub fn validate_address(address: &str) -> Result<(), SpecError> {
    address
        .parse::<IpNet>()
        .map(|_| ())
        .map_err(|e| SpecError::InvalidAddress(address.to_string(), e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_names() {
        assert!(validate_name("web-1").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn rejects_malformed_names() {
        assert_eq!(validate_name(""), Err(SpecError::InvalidName("".to_string())));
        assert_eq!(
            validate_name(&"a".repeat(64)),
            Err(SpecError::InvalidName("a".repeat(64)))
        );
        assert_eq!(
            validate_name("-leading-hyphen"),
            Err(SpecError::InvalidName("-leading-hyphen".to_string()))
        );
        assert_eq!(
            validate_name("trailing-hyphen-"),
            Err(SpecError::InvalidName("trailing-hyphen-".to_string()))
        );
        assert_eq!(
            validate_name("Has_Upper_And_Underscore"),
            Err(SpecError::InvalidName("Has_Upper_And_Underscore".to_string()))
        );
    }

    #[test]
    fn accepts_well_formed_cidr_addresses() {
        assert!(validate_address("10.0.0.5/24").is_ok());
        assert!(validate_address("192.168.1.1/32").is_ok());
    }

    #[test]
    fn rejects_malformed_addresses() {
        assert!(validate_address("not-an-address").is_err());
        assert!(validate_address("10.0.0.5").is_err()); // missing prefix length
        assert!(validate_address("10.0.0.5/33").is_err()); // prefix out of range
    }
}
