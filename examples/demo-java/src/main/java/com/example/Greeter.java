package com.example;

import com.example.model.User;

public interface Greeter {
    String greet(User user);

    default String punctuation() {
        return "!";
    }
}
