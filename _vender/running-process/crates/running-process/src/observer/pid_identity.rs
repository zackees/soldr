pub(super) fn matches<T: Eq>(expected: &T, observed: Option<&T>) -> bool {
    observed == Some(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_present_with_different_identity_is_not_the_spawned_process() {
        assert!(!matches(&100_u64, Some(&200_u64)));
    }

    #[test]
    fn matching_identity_is_the_spawned_process() {
        assert!(matches(&100_u64, Some(&100_u64)));
    }
}
