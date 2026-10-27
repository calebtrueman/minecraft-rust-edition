#![allow(clippy::result_large_err)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ristretto_vm::Error as VmError;
use rust_edition_java::{
    HostClassInitializer, HostClassInitializerFuture, HostMethod, JavaVm, MethodExecutionPolicy,
    NativeMethod, Parameters, Thread, Value,
};

const CALLER: &str = "hostfixture.Caller";

const BASE_SOURCE: &str = r#"
package hostfixture.blocked;

public class Base {
    public int inheritedField;
    public int inheritedMethod() { return 19; }
}
"#;

const TARGET_SOURCE: &str = r#"
package hostfixture.blocked;

public final class Target extends Base {
    public static int fieldValue;

    static { failIfInitialized(); }

    private static void failIfInitialized() {
        if (System.nanoTime() != Long.MIN_VALUE) {
            throw new AssertionError("host should replace this initializer");
        }
    }

    public static int staticCall() { return -1; }
    public int virtualCall() { return -2; }
    public static int unregisteredCall() { return -3; }

    public interface Contract { int interfaceCall(); }
    public static final class Implementation implements Contract {
        public int interfaceCall() { return -4; }
    }
}
"#;

const FAILED_SOURCE: &str = r#"
package hostfixture.blocked;

public final class FailedTarget {
    static { failIfInitialized(); }

    private static void failIfInitialized() {
        if (System.nanoTime() != Long.MIN_VALUE) {
            throw new AssertionError("host should replace this initializer");
        }
    }
    public static int value() { return -5; }
}
"#;

const CALLER_SOURCE: &str = r#"
package hostfixture;

import hostfixture.blocked.Target;
import hostfixture.blocked.Base;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public final class Caller {
    public static int staticCall() { return Target.staticCall(); }
    public static int virtualCall(Target target) { return target.virtualCall(); }
    public static int interfaceCall(Target.Contract target) { return target.interfaceCall(); }

    public static int reflectiveCall() throws Exception {
        return (Integer) Target.class.getMethod("staticCall").invoke(null);
    }

    public static int methodHandleCall() throws Throwable {
        var handle = MethodHandles.lookup().findStatic(
            Target.class, "staticCall", MethodType.methodType(int.class));
        return (int) handle.invokeExact();
    }

    public static int catchesBlockedMethod() {
        try { return Target.unregisteredCall(); }
        catch (UnsupportedOperationException expected) { return 71; }
    }

    public static int failedValue() {
        return hostfixture.blocked.FailedTarget.value();
    }

    public static int hookOnlyValue() {
        return HookOnly.value();
    }

    public static int catchesBlockedField() {
        try { return Target.fieldValue; }
        catch (UnsupportedOperationException expected) { return 73; }
    }

    public static int inheritedMethod(Target target) { return target.inheritedMethod(); }

    public static int catchesInheritedMethod(Target target) {
        try { return target.inheritedMethod(); }
        catch (UnsupportedOperationException expected) { return 74; }
    }

    public static int inheritedField(Base target) {
        try { return target.inheritedField; }
        catch (UnsupportedOperationException expected) { return 75; }
    }
}

final class HookOnly {
    public static int value() { return -6; }
}
"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rust-edition-host-method-hooks-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("src/hostfixture/blocked"))?;
        fs::create_dir_all(path.join("src/hostfixture"))?;
        fs::create_dir(path.join("classes"))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tool(home: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    let name = format!("{name}.exe");
    home.join("bin").join(name)
}

fn static_answer(_: Arc<Thread>, _: Parameters) -> ristretto_vm::Result<Option<Value>> {
    Ok(Some(Value::Int(101)))
}

fn interface_answer(_: Arc<Thread>, _: Parameters) -> ristretto_vm::Result<Option<Value>> {
    Ok(Some(Value::Int(103)))
}

fn failed_value_answer(_: Arc<Thread>, _: Parameters) -> ristretto_vm::Result<Option<Value>> {
    Ok(Some(Value::Int(106)))
}

fn hook_only_answer(_: Arc<Thread>, _: Parameters) -> ristretto_vm::Result<Option<Value>> {
    Ok(Some(Value::Int(107)))
}

fn async_virtual_answer(
    _: Arc<Thread>,
    _: Parameters,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ristretto_vm::Result<Option<Value>>> + Send>>
{
    Box::pin(async { Ok(Some(Value::Int(102))) })
}

fn failure_initializer(_: Arc<Thread>, _: Arc<ristretto_vm::Class>) -> HostClassInitializerFuture {
    Box::pin(async {
        Err(VmError::InternalError(
            "fixture host initializer failure".to_owned(),
        ))
    })
}

async fn call(
    vm: &JavaVm,
    method: &str,
    args: &[Value],
) -> Result<i32, Box<dyn std::error::Error>> {
    Ok(vm
        .invoke(CALLER, method, args)
        .await?
        .ok_or("fixture method should return a value")?
        .as_i32()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_host_methods_replace_calls_and_initializers_keep_java_failure_rules()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() || env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!("skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for the Java 25 fixture");
        return Ok(());
    }
    let jdk = PathBuf::from(env::var_os("JAVA_HOME").ok_or("JAVA_HOME is required")?);
    let library_home = PathBuf::from(
        env::var_os("RUST_EDITION_JAVA25_HOME").ok_or("RUST_EDITION_JAVA25_HOME is required")?,
    );
    let javac = tool(&jdk, "javac");
    let scratch = Scratch::new()?;
    let sources = [
        ("hostfixture/blocked/Base.java", BASE_SOURCE),
        ("hostfixture/blocked/Target.java", TARGET_SOURCE),
        ("hostfixture/blocked/FailedTarget.java", FAILED_SOURCE),
        ("hostfixture/Caller.java", CALLER_SOURCE),
    ];
    let mut source_paths = Vec::new();
    for (relative, source) in sources {
        let path = scratch.0.join("src").join(relative);
        fs::write(&path, source)?;
        source_paths.push(path);
    }
    let compiled = Command::new(&javac)
        .args(["--release", "25", "-d"])
        .arg(scratch.0.join("classes"))
        .args(&source_paths)
        .output()?;
    assert!(
        compiled.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let initializations = Arc::new(AtomicUsize::new(0));
    let initializer_count = initializations.clone();
    let success_initializer = HostClassInitializer::new(
        "hostfixture/blocked/Target",
        Arc::new(move |_, class| {
            let count = initializer_count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                class.set_static_value("fieldValue", Value::Int(81))?;
                Ok(())
            })
        }),
    );
    let failing_initializations = Arc::new(AtomicUsize::new(0));
    let failure_count = failing_initializations.clone();
    let failed_initializer = HostClassInitializer::new(
        "hostfixture/blocked/FailedTarget",
        Arc::new(move |thread, class| {
            let count = failure_count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                failure_initializer(thread, class).await
            })
        }),
    );
    let hook_only_initializations = Arc::new(AtomicUsize::new(0));
    let hook_only_count = hook_only_initializations.clone();
    let hook_only_initializer = HostClassInitializer::new(
        "hostfixture/HookOnly",
        Arc::new(move |_, _| {
            let count = hook_only_count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }),
    );
    let vm = JavaVm::new_with_host_hooks(
        &library_home,
        &[scratch.0.join("classes")],
        std::iter::empty::<NativeMethod>(),
        [
            HostMethod::sync("hostfixture/blocked/Target.staticCall()I", static_answer),
            HostMethod::asynchronous(
                "hostfixture/blocked/Target.virtualCall()I",
                async_virtual_answer,
            ),
            HostMethod::sync(
                "hostfixture/blocked/Target$Implementation.interfaceCall()I",
                interface_answer,
            ),
            HostMethod::sync(
                "hostfixture/blocked/FailedTarget.value()I",
                failed_value_answer,
            ),
            HostMethod::sync("hostfixture/HookOnly.value()I", hook_only_answer),
        ],
        [
            success_initializer,
            failed_initializer,
            hook_only_initializer,
        ],
        Arc::new(()),
        MethodExecutionPolicy::deny_class_prefixes(["hostfixture/blocked/"]),
    )
    .await?;

    assert_eq!(call(&vm, "staticCall()I", &[]).await?, 101);
    assert_eq!(initializations.load(Ordering::SeqCst), 1);

    let target = vm
        .allocate_object_without_initialization("hostfixture/blocked/Target")
        .await?;
    assert_eq!(
        call(
            &vm,
            "virtualCall(Lhostfixture/blocked/Target;)I",
            std::slice::from_ref(&target),
        )
        .await?,
        102
    );

    let implementation = vm
        .allocate_object_without_initialization("hostfixture/blocked/Target$Implementation")
        .await?;
    assert_eq!(
        call(
            &vm,
            "interfaceCall(Lhostfixture/blocked/Target$Contract;)I",
            &[implementation],
        )
        .await?,
        103
    );

    let second_target = vm
        .allocate_object_without_initialization("hostfixture/blocked/Target")
        .await?;
    vm.set_host_object_field(&target, "inheritedField", Value::Int(81))
        .await?;
    vm.allow_inherited_method_on_host_object(
        &target,
        "hostfixture/blocked/Base.inheritedMethod()I",
    )?;
    vm.allow_inherited_field_read_on_host_object(
        &target,
        "hostfixture/blocked/Base.inheritedField:I",
    )?;
    assert!(
        vm.allow_inherited_method_on_host_object(&target, "java/lang/String.length()I",)
            .is_err()
    );
    assert_eq!(
        call(
            &vm,
            "inheritedMethod(Lhostfixture/blocked/Target;)I",
            std::slice::from_ref(&target),
        )
        .await?,
        19
    );
    assert_eq!(
        call(
            &vm,
            "inheritedField(Lhostfixture/blocked/Base;)I",
            std::slice::from_ref(&target),
        )
        .await?,
        81
    );
    assert_eq!(
        call(
            &vm,
            "catchesInheritedMethod(Lhostfixture/blocked/Target;)I",
            std::slice::from_ref(&second_target),
        )
        .await?,
        74
    );
    assert_eq!(
        call(
            &vm,
            "inheritedField(Lhostfixture/blocked/Base;)I",
            std::slice::from_ref(&second_target),
        )
        .await?,
        75
    );
    assert_eq!(call(&vm, "reflectiveCall()I", &[]).await?, 101);
    assert_eq!(call(&vm, "methodHandleCall()I", &[]).await?, 101);
    assert_eq!(call(&vm, "catchesBlockedMethod()I", &[]).await?, 71);
    assert_eq!(call(&vm, "catchesBlockedField()I", &[]).await?, 73);
    assert_eq!(call(&vm, "hookOnlyValue()I", &[]).await?, 107);
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    assert_eq!(hook_only_initializations.load(Ordering::SeqCst), 1);

    let first = vm.invoke(CALLER, "failedValue()I", &[] as &[Value]).await;
    assert!(
        first.is_err(),
        "the host initializer failure must escape on first use"
    );
    let first_error = first.unwrap_err().to_string();
    assert!(
        first_error.contains("ExceptionInInitializerError"),
        "{first_error}"
    );
    let second = vm.invoke(CALLER, "failedValue()I", &[] as &[Value]).await;
    assert!(second.is_err(), "a failed class must stay failed");
    let second_error = second.unwrap_err().to_string();
    assert!(
        second_error.contains("NoClassDefFoundError"),
        "{second_error}"
    );
    assert_eq!(failing_initializations.load(Ordering::SeqCst), 1);
    Ok(())
}
