// typescript-language-server fixture (M3 verify).
export function add(a: number, b: number): number {
    if (!Number.isFinite(a) || !Number.isFinite(b)) {
        throw new Error("add: arguments must be finite numbers");
    }
    return a + b;
}

export function main(): void {
    const s = add(1, 2);
    console.log(s);
}
