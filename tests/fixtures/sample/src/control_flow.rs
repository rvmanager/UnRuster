//! Exercises every cyclomatic-complexity decision point and nesting depth
//! so metrics.rs's ComplexityVisitor branches are all hit.

// ── if / else if / nested ────────────────────────────────────────────────
pub fn ifs(a: bool, b: bool, c: bool) -> u8 {
    if a {
        if b {
            if c { 1 } else { 2 }
        } else if a && c {
            3
        } else {
            4
        }
    } else if b || c {
        5
    } else {
        6
    }
}

// ── match with guard + Or pattern + multiple arms ────────────────────────
pub fn matches(x: i32) -> u8 {
    match x {
        0 => 0,
        1 | 2 | 3 => 1,
        n if n > 10 => 2,
        n if n < -10 => 3,
        _ => 4,
    }
}

// ── while + while let + for + loop + ? ───────────────────────────────────
pub fn loopy(xs: &[Option<u8>]) -> Result<u8, ()> {
    let mut total: u8 = 0;
    for x in xs {
        let mut y = x.ok_or(())?;
        while y > 0 {
            total = total.wrapping_add(1);
            y -= 1;
        }
    }
    let mut it = xs.iter();
    while let Some(Some(_)) = it.next() {
        total = total.wrapping_add(1);
    }
    let mut counter = 0u8;
    loop {
        counter = counter.wrapping_add(1);
        if counter > 3 {
            break;
        }
    }
    Ok(total)
}

// ── short-circuit && / || ────────────────────────────────────────────────
pub fn short_circuit(a: bool, b: bool, c: bool, d: bool) -> bool {
    (a && b && c) || (b && d) || (a || c)
}
