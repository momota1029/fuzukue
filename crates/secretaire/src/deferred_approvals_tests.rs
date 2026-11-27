use super::*;

thread_local! {
    static CRASH: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
}

/// Simulate the server stopping at a named point on this test thread.
pub(crate) fn crash_point(name: &str) -> Result<()> {
    if CRASH.with(|crash| crash.borrow().is_some_and(|point| point == name)) {
        anyhow::bail!("simulated crash at {name}");
    }
    Ok(())
}

pub(crate) fn crash_at(point: Option<&'static str>) {
    CRASH.with(|crash| *crash.borrow_mut() = point);
}
