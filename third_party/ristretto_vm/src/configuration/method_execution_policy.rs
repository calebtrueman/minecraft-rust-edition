/// Optional rules that stop Java methods from running when they belong to selected classes.
///
/// This is a method-execution guard for hosts that provide selected Java objects from native
/// state. It is not a sandbox: it does not restrict field access, reflection, or other ways a
/// Java program may interact with its environment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MethodExecutionPolicy {
    denied_class_prefixes: Vec<String>,
}

impl MethodExecutionPolicy {
    /// Block methods declared by classes whose JVM internal name starts with one of these
    /// prefixes. Instance calls are also blocked when the receiver's runtime class matches a
    /// prefix, which catches inherited methods such as `Object.toString()` on a protected object.
    ///
    /// Prefixes may use JVM internal names such as `net/minecraft/`, or dotted names such as
    /// `net.minecraft.`. Dots are changed to slashes when the policy is created. Include the
    /// trailing slash when you mean a whole package.
    #[must_use]
    pub fn deny_class_prefixes<I, S>(prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let denied_class_prefixes = prefixes
            .into_iter()
            .map(|prefix| prefix.as_ref().replace('.', "/"))
            .filter(|prefix| !prefix.is_empty())
            .collect();
        Self {
            denied_class_prefixes,
        }
    }

    /// Returns the protected class name for a blocked method call, if any.
    pub(crate) fn blocked_class<'a>(
        &self,
        method_class_name: &'a str,
        receiver_class_name: Option<&'a str>,
    ) -> Option<&'a str> {
        if self.class_name_is_denied(method_class_name) {
            return Some(method_class_name);
        }
        receiver_class_name.filter(|class_name| self.class_name_is_denied(class_name))
    }

    fn class_name_is_denied(&self, class_name: &str) -> bool {
        self.denied_class_prefixes
            .iter()
            .any(|prefix| class_name.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::MethodExecutionPolicy;

    #[test]
    fn dotted_prefixes_match_internal_names() {
        let policy = MethodExecutionPolicy::deny_class_prefixes(["example.blocked."]);
        assert_eq!(
            Some("example/blocked/Target"),
            policy.blocked_class("example/blocked/Target", None)
        );
        assert_eq!(None, policy.blocked_class("example/allowed/Target", None));
    }

    #[test]
    fn inherited_method_is_blocked_by_receiver_class() {
        let policy = MethodExecutionPolicy::deny_class_prefixes(["example/blocked/"]);
        assert_eq!(
            Some("example/blocked/Target"),
            policy.blocked_class("java/lang/Object", Some("example/blocked/Target"))
        );
        assert_eq!(None, policy.blocked_class("java/lang/Object", None));
    }
}
