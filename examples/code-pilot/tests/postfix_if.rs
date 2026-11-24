#[path = "../src/postfix_if.rs"]
mod evaluator;

#[test]
fn false_guard_skips_evaluation() {
    assert_eq!(evaluator::postfix_if(false, || panic!("skipped value")), None);
    assert_eq!(evaluator::postfix_if(true, || 7), Some(7));
}
