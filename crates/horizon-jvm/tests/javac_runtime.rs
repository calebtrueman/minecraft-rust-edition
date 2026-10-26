use horizon_jvm::Runtime;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn matches_a_small_javac_program_when_a_jdk_is_available() {
    let Some((javac, java)) = java_tools() else {
        eprintln!("skipping: set JAVA_HOME to run the javac comparison");
        return;
    };
    let dir = TempDir::new();
    let source = dir.0.join("IntProgram.java");
    let helper = dir.0.join("Helper.java");
    fs::write(
        &source,
        r#"
public class IntProgram {
    public static int score(int x, int y) {
        int total = Helper.scale(x, y);
        if (total < 0) return 0;
        return total;
    }

    public static void main(String[] args) {
        System.out.println(score(Integer.parseInt(args[0]), Integer.parseInt(args[1])));
    }
}

"#,
    )
    .unwrap();
    fs::write(
        &helper,
        r#"
public class Helper {
    public static int scale(int x, int y) {
        return (x + y) * 3;
    }
}
"#,
    )
    .unwrap();

    let compiled = Command::new(&javac)
        .arg("-g:none")
        .arg("-d")
        .arg(&dir.0)
        .arg(&source)
        .arg(&helper)
        .output()
        .expect("run javac");
    assert!(
        compiled.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let caller_bytes = fs::read(dir.0.join("IntProgram.class")).unwrap();
    let helper_bytes = fs::read(dir.0.join("Helper.class")).unwrap();
    let mut runtime = Runtime::new();
    runtime.load_class(&caller_bytes).unwrap();
    runtime.load_class(&helper_bytes).unwrap();
    for (x, y) in [(2, 6), (-9, 2), (i32::MAX, 1)] {
        let expected = Command::new(&java)
            .arg("-cp")
            .arg(&dir.0)
            .arg("IntProgram")
            .arg(x.to_string())
            .arg(y.to_string())
            .output()
            .expect("run Java fixture");
        assert!(
            expected.status.success(),
            "Java fixture failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let expected: i32 = String::from_utf8(expected.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let actual = runtime
            .run_static_i32("IntProgram", "score", "(II)I", &[x, y], 100)
            .unwrap();
        assert_eq!(actual, expected, "score({x}, {y})");
    }

    let mut missing_class = Runtime::new();
    missing_class.load_class(&caller_bytes).unwrap();
    assert_eq!(
        missing_class
            .run_static_i32("IntProgram", "score", "(II)I", &[2, 6], 100)
            .unwrap_err()
            .to_string(),
        "class not loaded: Helper"
    );

    let alternate = TempDir::new();
    let alternate_helper = alternate.0.join("Helper.java");
    fs::write(
        &alternate_helper,
        "public class Helper { public static int different(int x) { return x; } }",
    )
    .unwrap();
    let compiled = Command::new(&javac)
        .arg("-g:none")
        .arg("-d")
        .arg(&alternate.0)
        .arg(&alternate_helper)
        .output()
        .expect("compile alternate helper");
    assert!(
        compiled.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let mut missing_method = Runtime::new();
    missing_method.load_class(&caller_bytes).unwrap();
    missing_method
        .load_class(&fs::read(alternate.0.join("Helper.class")).unwrap())
        .unwrap();
    assert_eq!(
        missing_method
            .run_static_i32("IntProgram", "score", "(II)I", &[2, 6], 100)
            .unwrap_err()
            .to_string(),
        "method not found: Helper.scale(II)I"
    );
}

#[test]
fn matches_javac_objects_fields_and_instance_calls_when_a_jdk_is_available() {
    let Some((javac, java)) = java_tools() else {
        eprintln!("skipping: set JAVA_HOME to run the javac comparison");
        return;
    };
    let dir = TempDir::new();
    let box_source = dir.0.join("Box.java");
    let program_source = dir.0.join("ObjectProgram.java");
    fs::write(
        &box_source,
        r#"
public class Box {
    public int value;

    public Box() {
    }

    public Box(int initial) {
        this.value = initial;
    }

    public int add(int amount) {
        return this.value + amount;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        &program_source,
        r#"
public class ObjectProgram {
    public static int exercise(int initial, int amount) {
        Box box = new Box(initial);
        box.value += 3;
        return box.add(amount);
    }

    public static int nullCall() {
        Box box = null;
        return box.add(1);
    }

    public static int nullField() {
        Box box = null;
        return box.value;
    }

    public static int defaultValue() {
        Box box = new Box();
        return box.value;
    }

    public static void main(String[] args) {
        if (args.length == 0) {
            System.out.println(defaultValue());
        } else {
            System.out.println(exercise(Integer.parseInt(args[0]), Integer.parseInt(args[1])));
        }
    }
}
"#,
    )
    .unwrap();

    let compiled = Command::new(&javac)
        .arg("-g:none")
        .arg("-d")
        .arg(&dir.0)
        .arg(&box_source)
        .arg(&program_source)
        .output()
        .expect("run javac");
    assert!(
        compiled.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut runtime = Runtime::new();
    runtime
        .load_class(&fs::read(dir.0.join("ObjectProgram.class")).unwrap())
        .unwrap();
    runtime
        .load_class(&fs::read(dir.0.join("Box.class")).unwrap())
        .unwrap();
    for (initial, amount) in [(2, 6), (-9, 2), (i32::MAX, 1)] {
        let expected = Command::new(&java)
            .arg("-cp")
            .arg(&dir.0)
            .arg("ObjectProgram")
            .arg(initial.to_string())
            .arg(amount.to_string())
            .output()
            .expect("run Java object fixture");
        assert!(
            expected.status.success(),
            "Java fixture failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let expected: i32 = String::from_utf8(expected.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let actual = runtime
            .run_static_i32(
                "ObjectProgram",
                "exercise",
                "(II)I",
                &[initial, amount],
                100,
            )
            .unwrap();
        assert_eq!(actual, expected, "exercise({initial}, {amount})");
    }

    let expected = Command::new(&java)
        .arg("-cp")
        .arg(&dir.0)
        .arg("ObjectProgram")
        .output()
        .expect("run Java default-field fixture");
    assert!(
        expected.status.success(),
        "Java fixture failed: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let expected: i32 = String::from_utf8(expected.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        runtime
            .run_static_i32("ObjectProgram", "defaultValue", "()I", &[], 100)
            .unwrap(),
        expected
    );

    assert_eq!(
        runtime
            .run_static_i32("ObjectProgram", "nullCall", "()I", &[], 100)
            .unwrap_err()
            .to_string(),
        "null object reference at method call"
    );
    assert_eq!(
        runtime
            .run_static_i32("ObjectProgram", "nullField", "()I", &[], 100)
            .unwrap_err()
            .to_string(),
        "null object reference at field access"
    );
}

#[test]
fn matches_javac_superclass_and_interface_dispatch_when_a_jdk_is_available() {
    let Some((javac, java)) = java_tools() else {
        eprintln!("skipping: set JAVA_HOME to run the javac comparison");
        return;
    };
    let dir = TempDir::new();
    let base_source = dir.0.join("ValueBase.java");
    let interface_source = dir.0.join("IntAction.java");
    let child_source = dir.0.join("ValueChild.java");
    let default_action_source = dir.0.join("DefaultAction.java");
    let program_source = dir.0.join("DispatchProgram.java");
    fs::write(
        &base_source,
        r#"
public class ValueBase {
    protected int value;

    public ValueBase(int initial) {
        this.value = initial;
    }

    public int apply(int amount) {
        return this.value + amount;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        &interface_source,
        r#"
public interface IntAction {
    default int apply(int amount) {
        return amount + 2;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        &child_source,
        r#"
public class ValueChild extends ValueBase implements IntAction {
    public int extra;

    public ValueChild(int initial) {
        super(initial);
        this.extra = 3;
    }

    @Override
    public int apply(int amount) {
        return super.apply(amount) + this.extra + 5;
    }
}
"#,
    )
    .unwrap();
    fs::write(
        &default_action_source,
        r#"
public class DefaultAction implements IntAction {
}
"#,
    )
    .unwrap();
    fs::write(
        &program_source,
        r#"
public class DispatchProgram {
    public static int exercise(int initial, int amount) {
        ValueChild child = new ValueChild(initial);
        ValueBase base = child;
        IntAction action = child;
        return base.apply(amount) + action.apply(amount);
    }

    public static int defaultCall(int amount) {
        IntAction action = new DefaultAction();
        return action.apply(amount);
    }

    public static void main(String[] args) {
        if (args.length == 1) {
            System.out.println(defaultCall(Integer.parseInt(args[0])));
        } else {
            System.out.println(exercise(Integer.parseInt(args[0]), Integer.parseInt(args[1])));
        }
    }
}
"#,
    )
    .unwrap();

    let compiled = Command::new(&javac)
        .arg("-g:none")
        .arg("-d")
        .arg(&dir.0)
        .arg(&base_source)
        .arg(&interface_source)
        .arg(&child_source)
        .arg(&default_action_source)
        .arg(&program_source)
        .output()
        .expect("run javac");
    assert!(
        compiled.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut runtime = Runtime::new();
    for class in [
        "DispatchProgram",
        "ValueChild",
        "IntAction",
        "ValueBase",
        "DefaultAction",
    ] {
        runtime
            .load_class(&fs::read(dir.0.join(format!("{class}.class"))).unwrap())
            .unwrap();
    }
    for (initial, amount) in [(2, 6), (-9, 2), (i32::MAX, 1)] {
        let expected = Command::new(&java)
            .arg("-cp")
            .arg(&dir.0)
            .arg("DispatchProgram")
            .arg(initial.to_string())
            .arg(amount.to_string())
            .output()
            .expect("run Java dispatch fixture");
        assert!(
            expected.status.success(),
            "Java fixture failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let expected: i32 = String::from_utf8(expected.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let actual = runtime
            .run_static_i32(
                "DispatchProgram",
                "exercise",
                "(II)I",
                &[initial, amount],
                200,
            )
            .unwrap();
        assert_eq!(actual, expected, "exercise({initial}, {amount})");
    }
    for amount in [-3, 0, i32::MAX] {
        let expected = Command::new(&java)
            .arg("-cp")
            .arg(&dir.0)
            .arg("DispatchProgram")
            .arg(amount.to_string())
            .output()
            .expect("run Java default-method fixture");
        assert!(
            expected.status.success(),
            "Java fixture failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let expected: i32 = String::from_utf8(expected.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let actual = runtime
            .run_static_i32("DispatchProgram", "defaultCall", "(I)I", &[amount], 200)
            .unwrap();
        assert_eq!(actual, expected, "defaultCall({amount})");
    }
}

fn java_tools() -> Option<(PathBuf, PathBuf)> {
    if let Some(home) = env::var_os("JAVA_HOME") {
        let bin = Path::new(&home).join("bin");
        let javac = bin.join(if cfg!(windows) { "javac.exe" } else { "javac" });
        let java = bin.join(if cfg!(windows) { "java.exe" } else { "java" });
        return working_tools(javac, java);
    }
    let path = env::var_os("PATH")?;
    let path_dirs: Vec<_> = env::split_paths(&path).collect();
    let executable = |name: &str| {
        path_dirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    };
    working_tools(executable("javac")?, executable("java")?)
}

fn working_tools(javac: PathBuf, java: PathBuf) -> Option<(PathBuf, PathBuf)> {
    let works = |tool: &Path| {
        tool.is_file()
            && Command::new(tool)
                .arg("-version")
                .output()
                .is_ok_and(|result| result.status.success())
    };
    (works(&javac) && works(&java)).then_some((javac, java))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("horizon-jvm-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
