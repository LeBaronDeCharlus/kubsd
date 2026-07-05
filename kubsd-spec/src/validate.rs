use crate::error::SpecError;

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
}
