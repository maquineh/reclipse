package com.example;

import com.example.model.User;
import com.example.service.MessageService;

/**
 * Small demo project used to exercise Reclipse's native syntax index.
 *
 * <p>It demonstrates <b>nested types</b>, fields, methods and a simple
 * {@link GreetingJob}.</p>
 *
 * @see GreetingJob
 */
public class App {
    private final MessageService service = new MessageService();

    public static void main(String[] args) {
        App app = new App();
        User user = new User("Ada", 36);
        System.out.println(app.service.greet(user));
        app.service.printModes();
        System.out.println(new GreetingJob("world").render());
    }

    /** Demonstrates a nested type in the outline. */
    static final class GreetingJob {
        private final String target;

        GreetingJob(String target) {
            this.target = target;
        }

        String render() {
            return "Hello, " + target + "!";
        }
    }
}
