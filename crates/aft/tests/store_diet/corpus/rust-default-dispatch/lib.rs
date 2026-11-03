trait I { fn m(&self) {} }
struct T;
impl I for T {}
fn caller(x: &dyn I) { x.m(); }
