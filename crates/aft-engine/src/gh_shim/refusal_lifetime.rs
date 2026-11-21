use super::*;

#[cfg(test)]
impl Drop for RefusalSinkGuard {
    fn drop(&mut self) {
        REFUSAL_SINK.with(|slot| slot.replace(None));
    }
}
