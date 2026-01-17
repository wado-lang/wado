package main

import "fmt"

// Leibniz formula: π/4 = 1 - 1/3 + 1/5 - 1/7 + ...
func main() {
	pi := 0.0
	sign := 1.0
	for i := 0; i < 1000000; i++ {
		pi += sign / (2.0*float64(i) + 1.0)
		sign = -sign
	}
	pi *= 4.0
	fmt.Printf("pi = %.15f\n", pi)
}
