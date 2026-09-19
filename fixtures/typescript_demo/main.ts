export function multiply(a: number, b: number): number {
  return a * b;
}

export class Calculator {
  private history: number[] = [];

  compute(a: number, b: number): number {
    const result = multiply(a, b);
    this.history.push(result);
    return result;
  }
}