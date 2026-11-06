//! End-to-end JDWP smoke test against a real JVM.
//!
//! The test is skipped when `javac`/`java` are unavailable, but fails when the
//! tools exist and the debugger cannot stop or inspect the target.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reclipse_debug::{
    BreakpointSpec, DebugCommand, DebugEvent, DebugSession, LaunchSpec, StepKind, StopReason,
    WatchValue,
};

/// Serializes the JDWP integration tests: each test reserves an ephemeral
/// localhost port for its debuggee, and parallel tests can steal each
/// other's port between reserve and bind.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn debugs_a_real_java_program() {
    let _serial = serial();
    let source = r#"
public class DebugTarget {
    public static void main(String[] args) {
        int total = 0;
        for (int i = 0; i < 20; i++) {
            total += i;
        }
        System.out.println("total=" + total);
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("total += i"))
        .map(|line| line + 1)
        .expect("breakpoint line");

    let directory = unique_temp_dir();
    fs::create_dir_all(directory.join("classes")).unwrap();
    let source_file = directory.join("DebugTarget.java");
    fs::write(&source_file, source.trim_start()).unwrap();
    let compile = Command::new("javac")
        .arg("-g")
        .arg("-d")
        .arg(directory.join("classes"))
        .arg(&source_file)
        .output();
    let Ok(compile) = compile else {
        let _ = fs::remove_dir_all(&directory);
        eprintln!("javac unavailable; skipping JDWP integration test");
        return;
    };
    if !compile.status.success() {
        panic!("javac failed: {}", String::from_utf8_lossy(&compile.stderr));
    }

    let spec = LaunchSpec {
        java: PathBuf::from("java"),
        classpath: directory.join("classes").display().to_string(),
        main_class: "DebugTarget".to_owned(),
        jvm_args: Vec::new(),
        program_args: Vec::new(),
        env: Default::default(),
        workdir: directory.clone(),
    };
    let breakpoint_path = source_file.clone();
    let breakpoints = vec![BreakpointSpec {
        path: source_file,
        line: breakpoint_line,
        class_name: "DebugTarget".to_owned(),
        condition: None,
    }];
    let session = DebugSession::launch(spec, breakpoints, false).expect("session");
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut stopped = None;
    let mut resolved = false;
    let mut started = false;
    while Instant::now() < deadline && stopped.is_none() {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Started { .. } => started = true,
                DebugEvent::BreakpointResolved { .. } => resolved = true,
                DebugEvent::Stopped(info) => stopped = Some(info),
                DebugEvent::Error(error) => panic!("debugger error: {error}"),
                DebugEvent::Exited { message } => {
                    panic!("JVM exited early: {message} (started={started}, resolved={resolved})")
                }
                DebugEvent::Output(line) => eprintln!("[jvm] {line}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(started, "session did not start");
    assert!(resolved, "breakpoint was not resolved");
    let stopped = stopped.expect("breakpoint did not stop the JVM");
    assert_eq!(stopped.reason, StopReason::Breakpoint);
    assert!(!stopped.frames.is_empty(), "no stack frames");
    assert!(
        stopped
            .variables
            .iter()
            .any(|variable| variable.name == "total"),
        "locals: {:?}",
        stopped.variables
    );
    assert!(
        stopped
            .variables
            .iter()
            .any(|variable| variable.name == "i"),
        "locals: {:?}",
        stopped.variables
    );

    session.send(DebugCommand::Step(StepKind::Over));
    let mut stepped = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !stepped {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Stopped(info) if info.reason == StopReason::Step => stepped = true,
                DebugEvent::Error(error) => panic!("step error: {error}"),
                DebugEvent::Exited { message } => panic!("JVM exited during step: {message}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(stepped, "step did not stop");

    session.send(DebugCommand::ClearBreakpoint {
        path: breakpoint_path,
        line: breakpoint_line,
    });
    session.send(DebugCommand::Resume);
    let mut exited = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline && !exited {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Exited { .. } => exited = true,
                DebugEvent::Error(error) => panic!("resume error: {error}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(exited, "JVM did not exit after resume");
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(&directory);
}

#[test]
fn inspects_instance_fields() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping instance-field test");
        return;
    }
    let source = r#"
public class InstanceTarget {
    private final String name = "Ada";
    private int count = 3;

    public static void main(String[] args) {
        new InstanceTarget().run();
    }

    void run() {
        int local = count + 1;
        System.out.println(name + local);
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("int local"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "InstanceTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "InstanceTarget"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            class_name: "InstanceTarget".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    let stopped = wait_for_stop(&session, StopReason::Breakpoint);
    assert!(
        stopped
            .variables
            .iter()
            .any(|variable| variable.name == "this.name" && variable.value.contains("Ada")),
        "fields: {:?}",
        stopped.variables
    );
    assert!(
        stopped
            .variables
            .iter()
            .any(|variable| variable.name == "this.count" && variable.value == "3"),
        "fields: {:?}",
        stopped.variables
    );
    session.send(DebugCommand::Disconnect);
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn pauses_on_uncaught_exception() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping exception test");
        return;
    }
    let source = r#"
public class ExceptionTarget {
    public static void main(String[] args) {
        throw new IllegalStateException("boom");
    }
}
"#;
    let (directory, _source_file) = compile_target(source.trim_start(), "ExceptionTarget");
    let session =
        DebugSession::launch(launch_spec(&directory, "ExceptionTarget"), Vec::new(), true)
            .expect("session");
    let stopped = wait_for_stop(&session, StopReason::Exception);
    assert!(
        stopped
            .exception
            .as_deref()
            .is_some_and(|exception| exception.contains("IllegalStateException")),
        "exception: {:?}",
        stopped.exception
    );
    session.send(DebugCommand::Resume);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut exited = false;
    while Instant::now() < deadline && !exited {
        while let Some(event) = session.poll() {
            if matches!(event, DebugEvent::Exited { .. }) {
                exited = true;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(exited, "JVM did not exit after exception resume");
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn filters_exceptions_by_type_and_caught_flag() {
    let _serial = serial();
    use reclipse_debug::ExceptionConfig;
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping exception filter test");
        return;
    }
    let source = r#"
public class FilterTarget {
    public static void main(String[] args) throws Exception {
        Thread.sleep(500);
        try {
            throw new IllegalArgumentException("bad");
        } catch (IllegalArgumentException expected) {
            System.out.println("caught " + expected.getMessage());
        }
        String value = null;
        System.out.println(value.length());
    }
}
"#;
    let (directory, _source_file) = compile_target(source.trim_start(), "FilterTarget");

    // 1. Matching type, uncaught: NPE stops the JVM.
    let session = DebugSession::launch(launch_spec(&directory, "FilterTarget"), Vec::new(), false)
        .expect("session");
    session.send(DebugCommand::SetExceptionConfig(ExceptionConfig {
        enabled: true,
        caught: false,
        uncaught: true,
        types: vec!["NullPointerException".to_owned()],
    }));
    let stopped = wait_for_stop(&session, StopReason::Exception);
    assert!(
        stopped
            .exception
            .as_deref()
            .is_some_and(|exception| exception.contains("NullPointerException")),
        "exception: {:?}",
        stopped.exception
    );
    drop(session);

    // 2. Non-matching type: the same NPE is ignored and the JVM exits.
    let session = DebugSession::launch(launch_spec(&directory, "FilterTarget"), Vec::new(), false)
        .expect("session");
    session.send(DebugCommand::SetExceptionConfig(ExceptionConfig {
        enabled: true,
        caught: false,
        uncaught: true,
        types: vec!["IllegalStateException".to_owned()],
    }));
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut stopped_exception = false;
    let mut exited = false;
    while Instant::now() < deadline && !exited {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Stopped(info) if info.reason == StopReason::Exception => {
                    stopped_exception = true;
                }
                DebugEvent::Exited { .. } => exited = true,
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(exited, "JVM did not exit with non-matching filter");
    assert!(
        !stopped_exception,
        "exception filter should have ignored the NullPointerException"
    );
    drop(session);

    // 3. Caught exceptions only stop when `caught` is set: IAE is caught.
    let session = DebugSession::launch(launch_spec(&directory, "FilterTarget"), Vec::new(), false)
        .expect("session");
    session.send(DebugCommand::SetExceptionConfig(ExceptionConfig {
        enabled: true,
        caught: true,
        uncaught: false,
        types: vec!["java.lang.IllegalArgumentException".to_owned()],
    }));
    let stopped = wait_for_stop(&session, StopReason::Exception);
    assert!(
        stopped
            .exception
            .as_deref()
            .is_some_and(|exception| exception.contains("IllegalArgumentException")),
        "exception: {:?}",
        stopped.exception
    );
    drop(session);

    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn steps_across_maven_modules() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping multi-module test");
        return;
    }
    // Mirrors `examples/multi-module-demo` (parent + core + app): the library
    // and the app are compiled to separate output roots, like Maven's
    // `core/target/classes` and `app/target/classes`.
    let directory = unique_temp_dir();
    let core_src = directory.join("core-src/com/example/core/Greeter.java");
    let app_src = directory.join("app-src/com/example/app/Main.java");
    fs::create_dir_all(core_src.parent().unwrap()).unwrap();
    fs::create_dir_all(app_src.parent().unwrap()).unwrap();
    fs::write(
        &core_src,
        "package com.example.core;\npublic class Greeter {\n    public String greet(String name) {\n        return \"hello, \" + name + \"!\";\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        &app_src,
        "package com.example.app;\nimport com.example.core.Greeter;\npublic class Main {\n    public static void main(String[] args) {\n        Greeter greeter = new Greeter();\n        System.out.println(greeter.greet(\"world\"));\n    }\n}\n",
    )
    .unwrap();
    let core_classes = directory.join("core-classes");
    let app_classes = directory.join("app-classes");
    fs::create_dir_all(&core_classes).unwrap();
    fs::create_dir_all(&app_classes).unwrap();
    let compile_core = Command::new("javac")
        .arg("-g")
        .arg("-d")
        .arg(&core_classes)
        .arg(&core_src)
        .output()
        .unwrap_or_else(|error| panic!("javac unavailable: {error}"));
    assert!(
        compile_core.status.success(),
        "javac core failed: {}",
        String::from_utf8_lossy(&compile_core.stderr)
    );
    let compile_app = Command::new("javac")
        .arg("-g")
        .arg("-cp")
        .arg(&core_classes)
        .arg("-d")
        .arg(&app_classes)
        .arg(&app_src)
        .output()
        .unwrap_or_else(|error| panic!("javac unavailable: {error}"));
    assert!(
        compile_app.status.success(),
        "javac app failed: {}",
        String::from_utf8_lossy(&compile_app.stderr)
    );

    let separator = if cfg!(windows) { ";" } else { ":" };
    let classpath = format!(
        "{}{}{}",
        app_classes.display(),
        separator,
        core_classes.display()
    );
    let session = DebugSession::launch(
        LaunchSpec {
            java: PathBuf::from("java"),
            classpath,
            main_class: "com.example.app.Main".to_owned(),
            jvm_args: Vec::new(),
            program_args: Vec::new(),
            env: Default::default(),
            workdir: directory.clone(),
        },
        vec![BreakpointSpec {
            path: app_src.clone(),
            line: 6,
            class_name: "com.example.app.Main".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    wait_for_stop(&session, StopReason::Breakpoint);

    session.send(DebugCommand::Step(StepKind::Into));
    let stepped = wait_for_stop(&session, StopReason::Step);
    let top = stepped.frames.first().expect("step must land on a frame");
    assert_eq!(top.location.method, "greet", "frames: {:?}", stepped.frames);
    assert!(
        top.location.signature.contains("Greeter"),
        "signature: {}",
        top.location.signature
    );

    session.send(DebugCommand::Evaluate {
        expression: "name".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "name");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("\"world\""));

    session.send(DebugCommand::Disconnect);
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn attaches_to_a_suspended_jvm() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping attach test");
        return;
    }
    let source = r#"
public class AttachTarget {
    public static void main(String[] args) {
        int value = 7;
        System.out.println("value=" + value);
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("int value"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "AttachTarget");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut child = Command::new("java")
        .arg(format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:{port}"
        ))
        .arg("-cp")
        .arg(directory.join("classes"))
        .arg("AttachTarget")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("jvm");
    thread::sleep(Duration::from_millis(600));
    let session = DebugSession::attach(
        format!("127.0.0.1:{port}"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            class_name: "AttachTarget".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("attach session");
    session.send(DebugCommand::Resume);
    let stopped = wait_for_stop(&session, StopReason::Breakpoint);
    assert!(
        stopped
            .variables
            .iter()
            .any(|variable| variable.name == "args"),
        "locals: {:?}",
        stopped.variables
    );
    session.send(DebugCommand::Disconnect);
    drop(session);
    let _ = child.kill();
    let _ = child.wait();
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn steps_into_and_out() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping step test");
        return;
    }
    let source = r#"
public class StepTarget {
    public static void main(String[] args) {
        int result = helper(3);
        System.out.println(result);
    }

    static int helper(int value) {
        int doubled = value * 2;
        return doubled;
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("helper(3)"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "StepTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "StepTarget"),
        vec![BreakpointSpec {
            path: source_file.clone(),
            line: breakpoint_line,
            class_name: "StepTarget".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    wait_for_stop(&session, StopReason::Breakpoint);
    session.send(DebugCommand::Step(StepKind::Into));
    let into = wait_for_stop(&session, StopReason::Step);
    assert_eq!(
        into.frames
            .first()
            .map(|frame| frame.location.method.as_str()),
        Some("helper"),
        "frames: {:?}",
        into.frames
    );
    session.send(DebugCommand::Step(StepKind::Out));
    let out = wait_for_stop(&session, StopReason::Step);
    assert_eq!(
        out.frames
            .first()
            .map(|frame| frame.location.method.as_str()),
        Some("main"),
        "frames: {:?}",
        out.frames
    );
    session.send(DebugCommand::ClearBreakpoint {
        path: source_file,
        line: breakpoint_line,
    });
    session.send(DebugCommand::Resume);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut exited = false;
    while Instant::now() < deadline && !exited {
        while let Some(event) = session.poll() {
            if matches!(event, DebugEvent::Exited { .. }) {
                exited = true;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(exited, "JVM did not exit after step out");
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn conditional_breakpoint_filters_hits() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping conditional breakpoint test");
        return;
    }
    let source = r#"
public class ConditionalTarget {
    public static void main(String[] args) {
        int total = 0;
        for (int i = 0; i < 10; i++) {
            total += i;
        }
        System.out.println("total=" + total);
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("total += i"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "ConditionalTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "ConditionalTarget"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            class_name: "ConditionalTarget".to_owned(),
            condition: Some("i == 3".to_owned()),
        }],
        false,
    )
    .expect("session");
    let stopped = wait_for_stop(&session, StopReason::Breakpoint);
    let i = stopped
        .variables
        .iter()
        .find(|variable| variable.name == "i")
        .map(|variable| variable.value.as_str());
    assert_eq!(i, Some("3"), "locals: {:?}", stopped.variables);
    session.send(DebugCommand::Disconnect);
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn evaluates_expressions_and_watches() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping expression test");
        return;
    }
    let source = r#"
public class ExprTarget {
    static class User {
        final String name;
        User(String name) { this.name = name; }
        String greet() { return "hi " + name; }
    }

    public static void main(String[] args) {
        User user = new User("Ada");
        int[] nums = {10, 20, 30};
        int total = 0;
        for (int i = 0; i < 5; i++) {
            total += i;
        }
        System.out.println(user.name + total);
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.trim() == "total += i;")
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "ExprTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "ExprTarget"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            class_name: "ExprTarget".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    wait_for_stop(&session, StopReason::Breakpoint);

    session.send(DebugCommand::Evaluate {
        expression: "1 + 2 * 3".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "1 + 2 * 3");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("7"));

    session.send(DebugCommand::Evaluate {
        expression: "user.name".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "user.name");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("\"Ada\""));

    session.send(DebugCommand::Evaluate {
        expression: "i == 0 && user.name == \"Ada\"".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "i == 0 && user.name == \"Ada\"");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("true"));

    session.send(DebugCommand::EvaluateWatches {
        expressions: vec![
            "i".to_owned(),
            "total + 10".to_owned(),
            "user.name".to_owned(),
        ],
    });
    let watches = wait_for_watches(&session, 3);
    assert_eq!(watches[0].value.as_deref(), Some("0"));
    assert_eq!(watches[1].value.as_deref(), Some("10"));
    assert_eq!(watches[2].value.as_deref(), Some("\"Ada\""));

    // Array indexing.
    session.send(DebugCommand::Evaluate {
        expression: "nums[1]".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "nums[1]");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("20"));

    session.send(DebugCommand::Evaluate {
        expression: "nums[i + 1]".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "nums[i + 1]");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("20"));

    // Out of bounds is a clean error, not a crash.
    session.send(DebugCommand::Evaluate {
        expression: "nums[9]".to_owned(),
    });
    let (_, error) = wait_for_evaluate(&session, "nums[9]");
    assert!(error.is_some(), "expected out-of-bounds error");

    // Instance method invocation via JDWP.
    session.send(DebugCommand::Evaluate {
        expression: "user.greet()".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "user.greet()");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("\"hi Ada\""));

    // Chained call on a field.
    session.send(DebugCommand::Evaluate {
        expression: "user.name.length()".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "user.name.length()");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("3"));

    // Static call on a JDK class.
    session.send(DebugCommand::Evaluate {
        expression: "Math.max(3, 7)".to_owned(),
    });
    let (value, error) = wait_for_evaluate(&session, "Math.max(3, 7)");
    assert_eq!(error, None);
    assert_eq!(value.as_deref(), Some("7"));

    session.send(DebugCommand::Disconnect);
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn hot_swaps_a_loaded_class() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping hot swap test");
        return;
    }
    let source_v1 = r#"
public class HotSwapTarget {
    public static void main(String[] args) {
        System.out.println(new HotSwapTarget().version());
    }

    String version() { return "v1"; }
}
"#;
    let source_v2 = source_v1.replace("\"v1\"", "\"v2\"");
    let source_v1 = source_v1.trim_start();
    let breakpoint_line = source_v1
        .lines()
        .position(|line| line.contains("System.out.println"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source_v1, "HotSwapTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "HotSwapTarget"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            class_name: "HotSwapTarget".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    wait_for_stop(&session, StopReason::Breakpoint);
    let (v2_directory, _) = compile_target(source_v2.trim_start(), "HotSwapTarget");
    let bytes = fs::read(v2_directory.join("classes/HotSwapTarget.class")).unwrap();
    session.send(DebugCommand::RedefineClasses {
        classes: vec![("HotSwapTarget".to_owned(), bytes)],
    });
    wait_for_status(&session, "hot swap");
    session.send(DebugCommand::ClearBreakpoint {
        path: directory.join("HotSwapTarget.java"),
        line: breakpoint_line,
    });
    session.send(DebugCommand::Resume);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut output = String::new();
    let mut exited = false;
    while Instant::now() < deadline && !(exited && output.contains("v2")) {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Output(line) => {
                    output.push_str(&line);
                    output.push('\n');
                }
                DebugEvent::Exited { .. } => exited = true,
                DebugEvent::Error(error) => panic!("hot swap error: {error}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(output.contains("v2"), "output after hot swap: {output:?}");
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
    let _ = fs::remove_dir_all(v2_directory);
}

fn wait_for_status(session: &DebugSession, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Status(text) if text.contains(needle) => return,
                DebugEvent::Error(error) => panic!("debugger error: {error}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("status containing {needle} did not arrive");
}

fn wait_for_evaluate(session: &DebugSession, expression: &str) -> (Option<String>, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        while let Some(event) = session.poll() {
            if let DebugEvent::EvaluateResult {
                expression: result_expression,
                value,
                error,
            } = event
            {
                if result_expression == expression {
                    return (value, error);
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("evaluate did not return for {expression}");
}

fn wait_for_watches(session: &DebugSession, count: usize) -> Vec<WatchValue> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        while let Some(event) = session.poll() {
            if let DebugEvent::WatchValues(values) = event {
                if values.len() == count {
                    return values;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("watches did not return");
}

#[test]
fn breakpoints_inside_anonymous_class() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping anonymous class test");
        return;
    }
    let source = r#"
public class AnonymousTarget {
    public static void main(String[] args) {
        Runnable runnable = new Runnable() {
            @Override
            public void run() {
                int value = 7;
                System.out.println("value=" + value);
            }
        };
        runnable.run();
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("int value = 7"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "AnonymousTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "AnonymousTarget"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            class_name: "AnonymousTarget".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    let stopped = wait_for_stop(&session, StopReason::Breakpoint);
    assert!(
        stopped.location.as_ref().is_some_and(|location| {
            location.line == breakpoint_line && location.signature.contains("AnonymousTarget$")
        }),
        "location: {:?}",
        stopped.location
    );
    session.send(DebugCommand::Disconnect);
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn breakpoints_after_nested_type_still_resolve() {
    let _serial = serial();
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("javac unavailable; skipping nested order test");
        return;
    }
    let source = r#"
public class NestedOrderTarget {
    public static void main(String[] args) {
        int before = 1;
        class Inner {
            void run() { System.out.println("inner"); }
        }
        new Inner().run();
        int after = before + 1;
        System.out.println(after);
    }
}
"#;
    let source = source.trim_start();
    let breakpoint_line = source
        .lines()
        .position(|line| line.contains("int after"))
        .map(|line| line + 1)
        .expect("breakpoint line");
    let (directory, source_file) = compile_target(source, "NestedOrderTarget");
    let session = DebugSession::launch(
        launch_spec(&directory, "NestedOrderTarget"),
        vec![BreakpointSpec {
            path: source_file,
            line: breakpoint_line,
            // Simulates the old heuristic choosing the inner local type.
            class_name: "NestedOrderTarget$1Inner".to_owned(),
            condition: None,
        }],
        false,
    )
    .expect("session");
    let stopped = wait_for_stop(&session, StopReason::Breakpoint);
    assert_eq!(
        stopped.location.as_ref().map(|location| location.line),
        Some(breakpoint_line),
        "location: {:?}",
        stopped.location
    );
    session.send(DebugCommand::Disconnect);
    drop(session);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_dir_all(directory);
}

fn compile_target(source: &str, class_name: &str) -> (PathBuf, PathBuf) {
    let directory = unique_temp_dir();
    fs::create_dir_all(directory.join("classes")).unwrap();
    let source_file = directory.join(format!("{class_name}.java"));
    fs::write(&source_file, source).unwrap();
    let compile = Command::new("javac")
        .arg("-g")
        .arg("-d")
        .arg(directory.join("classes"))
        .arg(&source_file)
        .output()
        .unwrap_or_else(|error| panic!("javac unavailable: {error}"));
    assert!(
        compile.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    (directory, source_file)
}

fn launch_spec(directory: &Path, main_class: &str) -> LaunchSpec {
    LaunchSpec {
        java: PathBuf::from("java"),
        classpath: directory.join("classes").display().to_string(),
        main_class: main_class.to_owned(),
        jvm_args: Vec::new(),
        program_args: Vec::new(),
        env: Default::default(),
        workdir: directory.to_path_buf(),
    }
}

fn wait_for_stop(session: &DebugSession, reason: StopReason) -> Box<reclipse_debug::StopInfo> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        while let Some(event) = session.poll() {
            match event {
                DebugEvent::Stopped(info) if info.reason == reason => return info,
                DebugEvent::Error(error) => panic!("debugger error: {error}"),
                DebugEvent::Exited { message } => panic!("JVM exited early: {message}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("debugger did not stop with {reason:?}");
}

fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "reclipse-debug-test-{}-{nanos}",
        std::process::id()
    ))
}
