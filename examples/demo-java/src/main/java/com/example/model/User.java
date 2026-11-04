package com.example.model;

/**
 * Java 16+ record. Record components show up as fields in the outline.
 */
public record User(String name, int age) {
    public User {
        if (age < 0) {
            throw new IllegalArgumentException("age must be positive");
        }
    }

    public User(String name) {
        this(name, 0);
    }

    public boolean adult() {
        return age >= 18;
    }
}
