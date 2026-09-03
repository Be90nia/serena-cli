// csharp-ls fixture (M3 verify).
using System;

class Program {
    static int Add(int a, int b) => a + b;

    static void Main() {
        var s = Add(1, 2);
        Console.WriteLine(s);
    }
}
