pub fn shaped(flag: bool, n: i32) -> i32 {
    if flag && n > 0 {
        for value in 0..n {
            let _ = value;
        }
    }

    match n {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}
