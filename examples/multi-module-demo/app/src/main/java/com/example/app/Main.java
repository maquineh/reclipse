package com.example.app;

import com.example.core.Greeter;

public class Main {
    public static void main(String[] args) {
        Greeter greeter = new Greeter("hello");
        String message = greeter.greet("world");
        System.out.println(message);
    }
}
