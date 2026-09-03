# pyright fixture (M3 verify) — single function `add` callable from main.
def add(a: int, b: int) -> int:
    return a + b

def main() -> None:
    s = add(1, 2)
    print(s)

if __name__ == "__main__":
    main()
