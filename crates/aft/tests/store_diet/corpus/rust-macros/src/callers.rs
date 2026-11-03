use crate::targets::{select_target, select_branch_target, matches_target, custom_target, template_target, dsl_target};

macro_rules! run_template {
    ($x:expr) => {
        template_target();
        $x
    };
}

pub async fn select_caller(rx: Receiver) {
    tokio::select! {
        biased;
        res = async { select_target() } => { select_branch_target(); }
        _ = rx.recv() => {}
    }
}

pub fn matches_caller(x: Option<u32>) -> bool {
    matches!(x, Some(v) if matches_target(v))
}

pub fn custom_caller() {
    let _ = my_custom!(first: custom_target(), second => 3);
}

pub fn template_caller_one() {
    run_template!(1);
}

pub fn template_caller_two() {
    run_template!(2);
}

pub fn dsl_caller() {
    my_dsl! { when ready then dsl_target @ 3 }
}
