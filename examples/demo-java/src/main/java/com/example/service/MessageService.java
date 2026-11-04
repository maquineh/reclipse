package com.example.service;

import com.example.Greeter;
import com.example.model.User;

import java.util.List;
import java.util.stream.Collectors;

public class MessageService implements Greeter {
    public enum Mode {
        FORMAL,
        CASUAL
    }

    private Mode mode = Mode.FORMAL;

    @Override
    public String greet(User user) {
        String message = switch (mode) {
            case FORMAL -> "Bom dia, " + user.name() + ".";
            case CASUAL -> "E aí, " + user.name() + punctuation();
        };
        return message;
    }

    public void printModes() {
        List<String> modes = List.of(Mode.values()).stream()
                .map(Enum::name)
                .collect(Collectors.toList());
        System.out.println(modes);

        String textBlock = """
                Reclipse demo
                mode = %s
                """.formatted(mode);
        System.out.print(textBlock);
    }
}
