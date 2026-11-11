pub mod approval;
pub mod duration;
pub mod fs;
pub mod json;
pub(crate) mod print_ranges;
pub mod run;
pub mod time;

#[cfg(test)]
mod tests {
    #[test]
    fn util_submodules_are_reachable() {
        assert_eq!(crate::util::json::truncate_runes("abcd", 3), "abc…");
    }
}
