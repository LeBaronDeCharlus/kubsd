use crate::error::SpecError;

pub fn parse_cpu_cores(s: &str) -> Result<f64, SpecError> {
    let cores: f64 = s.parse().map_err(|_| SpecError::InvalidCpu(s.to_string()))?;
    if cores > 0.0 && cores.is_finite() {
        Ok(cores)
    } else {
        Err(SpecError::InvalidCpu(s.to_string()))
    }
}

pub fn cores_to_pcpu_percent(cores: f64) -> u32 {
    (cores * 100.0).round() as u32
}

pub fn parse_memory_bytes(s: &str) -> Result<u64, SpecError> {
    let invalid = || SpecError::InvalidMemory(s.to_string());
    let upper = s.to_ascii_uppercase();
    let (num_part, multiplier): (&str, u64) = if let Some(n) = upper.strip_suffix('K') {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('M') {
        (n, 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix('G') {
        (n, 1024 * 1024 * 1024)
    } else {
        (upper.as_str(), 1)
    };
    let value: u64 = num_part.parse().map_err(|_| invalid())?;
    if value == 0 {
        return Err(invalid());
    }
    value.checked_mul(multiplier).ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_cpu_values() {
        assert_eq!(parse_cpu_cores("2"), Ok(2.0));
        assert_eq!(parse_cpu_cores("0.5"), Ok(0.5));
    }

    #[test]
    fn rejects_invalid_cpu_values() {
        assert_eq!(parse_cpu_cores("0"), Err(SpecError::InvalidCpu("0".to_string())));
        assert_eq!(parse_cpu_cores("-1"), Err(SpecError::InvalidCpu("-1".to_string())));
        assert_eq!(parse_cpu_cores("abc"), Err(SpecError::InvalidCpu("abc".to_string())));
    }

    #[test]
    fn converts_cores_to_pcpu_percent() {
        assert_eq!(cores_to_pcpu_percent(2.0), 200);
        assert_eq!(cores_to_pcpu_percent(0.5), 50);
    }

    #[test]
    fn parses_valid_memory_values() {
        assert_eq!(parse_memory_bytes("512M"), Ok(512 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("1G"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("2048K"), Ok(2048 * 1024));
        assert_eq!(parse_memory_bytes("100"), Ok(100));
    }

    #[test]
    fn rejects_invalid_memory_values() {
        assert!(parse_memory_bytes("0M").is_err());
        assert!(parse_memory_bytes("").is_err());
        assert!(parse_memory_bytes("abc").is_err());
        assert!(parse_memory_bytes("-5M").is_err());
        assert!(parse_memory_bytes("999999999999G").is_err());
    }
}
