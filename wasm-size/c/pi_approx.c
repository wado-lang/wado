#include <stdio.h>

// Leibniz formula: π/4 = 1 - 1/3 + 1/5 - 1/7 + ...
int main(void) {
    double pi = 0.0;
    double sign = 1.0;
    for (int i = 0; i < 1000000; i++) {
        pi += sign / (2.0 * i + 1.0);
        sign = -sign;
    }
    pi *= 4.0;
    printf("pi = %.15f\n", pi);
    return 0;
}
