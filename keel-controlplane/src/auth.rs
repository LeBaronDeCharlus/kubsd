use std::path::Path;

pub fn load_token(path: &Path) -> Result<String, String> {
    let token = std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read auth token file {}: {e}", path.display()))?;
    if token.is_empty() {
        return Err(format!("auth token file {} is empty", path.display()));
    }
    Ok(token)
}

pub fn check(provided: Option<&str>, expected: &str) -> bool {
    let Some(provided) = provided else { return false };
    let provided = provided.strip_prefix("Bearer ").unwrap_or(provided);
    constant_time_eq(provided.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_accepts_the_correct_token_with_bearer_prefix() {
        assert!(check(Some("Bearer secret123"), "secret123"));
    }

    #[test]
    fn check_accepts_the_correct_token_without_bearer_prefix() {
        assert!(check(Some("secret123"), "secret123"));
    }

    #[test]
    fn check_rejects_a_wrong_token() {
        assert!(!check(Some("Bearer wrong"), "secret123"));
    }

    #[test]
    fn check_rejects_a_missing_header() {
        assert!(!check(None, "secret123"));
    }

    #[test]
    fn load_token_trims_trailing_whitespace_and_newline() {
        let dir = std::env::temp_dir().join(format!("keel-controlplane-auth-test-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "secret123\n").unwrap();
        assert_eq!(load_token(&path).unwrap(), "secret123");
    }

    #[test]
    fn load_token_on_an_empty_file_returns_an_error() {
        let dir = std::env::temp_dir().join(format!("keel-controlplane-auth-test-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "").unwrap();
        assert!(load_token(&path).is_err());
    }

    #[test]
    fn load_token_on_a_whitespace_only_file_returns_an_error() {
        let dir = std::env::temp_dir().join(format!("keel-controlplane-auth-test-whitespace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "   \n\t\n").unwrap();
        assert!(load_token(&path).is_err());
    }

    #[test]
    fn load_token_on_a_missing_file_returns_an_error() {
        let path = std::env::temp_dir().join("keel-controlplane-auth-test-missing-file-does-not-exist");
        assert!(load_token(&path).is_err());
    }

    #[test]
    fn constant_time_eq_accepts_equal_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn constant_time_eq_rejects_same_length_different_content() {
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
