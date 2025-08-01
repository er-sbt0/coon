#include <stdio.h>

// Test function in another file
void test_function() {
    printf("Test function called.\n");
}

// Function that uses the test function
void call_test() {
    test_function();
}
