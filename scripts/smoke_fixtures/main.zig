const std = @import("std");

// 冒烟具名符号：具名函数（documentSymbol 底线断言目标）
fn smoke_double(x: u32) u32 {
    return x * 2;
}

pub fn main() !void {
    std.debug.print("{d}\n", .{smoke_double(21)});
}
