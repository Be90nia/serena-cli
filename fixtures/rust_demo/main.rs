// rust fixture (M3 verify) — single function `add` callable from main.
fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let s = add(1, 2);
    println!("{s}");
}
