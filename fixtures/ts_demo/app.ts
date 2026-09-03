// typescript-language-server fixture (M3 verify).
export function add(a: number, b: number): number {
    return a + b;
}

export function main(): void {
    const s = add(1, 2);
    console.log(s);
}
