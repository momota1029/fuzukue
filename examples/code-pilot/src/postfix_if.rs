pub fn postfix_if(guard: bool, value: impl FnOnce() -> i64) -> Option<i64> {
    guard.then(value)
}
