#include <stdio.h>

void call_test();

// A simple function to test call hierarchy
void hello_world() {
	call_test();
    printf("Hello, World!\n");
}

// Another function that calls hello_world
void main_function() {
    hello_world();
    printf("Main function completed.\n");
}

// Entry point
int main() {
    main_function();
    return 0;
}
