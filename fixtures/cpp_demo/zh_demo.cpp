// zh_demo.cpp — M0 A0-2 验收：中文注释 + 中文标识符，验证 offsets.rs 换算正确
#include "math.h"

// 中文标识符的函数声明
int 计算总和(int a, int b);

int 计算总和(int a, int b) {
    // 调用另一个含中文注释路径的函数
    return add(a, b);
}

// 中文类名（用作 def/refs 的目标）
class 计算器 {
public:
    int 加(int x, int y) {
        return x + y;
    }
};

int main() {
    int 中文变量 = 计算总和(1, 2);  // 测试中文变量
    计算器 算盘;
    int 结果 = 算盘.加(3, 4);     // 测试中文方法调用
    return 中文变量 + 结果;
}