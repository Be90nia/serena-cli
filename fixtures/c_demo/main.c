// clangd C fixture (M3 verify) — same simple `add` shape as cpp_demo/main.cpp.
int add(int a, int b) {
    return a + b;
}

int main(void) {
    int s = add(1, 2);
    return s;
}
