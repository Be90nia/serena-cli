pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// edit_context 测试的真实调用者：定义点过滤（bd nl2w/A3b #1）生效后，
/// callers 至少含本函数内的 add 调用而非声明自身。
pub fn demo() -> i32 {
    add(2, 3)
}

pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}