package main

import "fmt"

// gopls fixture (M3 verify).
func add(a, b int) int {
	return a + b
}

func main() {
	s := add(1, 2)
	fmt.Println(s)
}
