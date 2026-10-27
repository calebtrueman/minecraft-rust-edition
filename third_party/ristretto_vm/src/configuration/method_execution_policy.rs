//! Optional guards for method calls and direct field instructions on host-owned Java objects.

use crate::Error::{ConfigurationError, PoisonedLock};
use crate::Result;
use ristretto_classloader::{Class, Value};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Optional rules for blocking Java method calls and direct field access.
///
/// This is a narrow guard for hosts that provide selected Java objects from native state. It
/// covers Java method calls and `getfield`, `getstatic`, `putfield`, and `putstatic` bytecode. It
/// does not restrict reflection, `Unsafe`, or other ways a Java program may reach its environment.
#[derive(Clone, Debug, Default)]
pub struct MethodExecutionPolicy {
    denied_class_prefixes: Vec<String>,
    host_objects: Arc<RwLock<Vec<HostObjectRules>>>,
}

#[derive(Clone, Debug)]
struct HostObjectRules {
    object: Value,
    allowed_methods: HashSet<String>,
    readable_fields: HashSet<String>,
}

impl MethodExecutionPolicy {
    /// Block methods and direct field access for classes whose JVM internal name starts with one
    /// of these prefixes. A host may then allow exact methods and field reads for selected object
    /// identities with [`Self::allow_method_on_host_object`] and
    /// [`Self::allow_field_read_on_host_object`]. Java field writes are never allowed by this
    /// policy.
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
            host_objects: Arc::default(),
        }
    }

    /// Allow one exact method signature on one non-null host object.
    ///
    /// The signature uses an internal class name, a dot, the method name, and its JVM descriptor,
    /// such as `example/Host.read()I`. Its owner must be the object's runtime class. Constructors
    /// and class initializers cannot be allowed. Clones of this policy share registrations, and
    /// each registration keeps a strong copy of the object value for the policy's lifetime.
    ///
    /// # Errors
    /// Returns an error if the value is not a non-null object or the signature is invalid.
    pub fn allow_method_on_host_object(
        &self,
        object: &Value,
        signature: impl AsRef<str>,
    ) -> Result<()> {
        let class_name = object_class_name(object)?;
        let signature = normalize_method_signature(signature.as_ref())?;
        let owner = method_owner(&signature).ok_or_else(|| {
            ConfigurationError(format!("invalid host method signature: {signature}"))
        })?;
        if owner != class_name {
            return Err(ConfigurationError(format!(
                "host method owner {owner} does not match object class {class_name}"
            )));
        }
        if signature.contains(".<init>(") || signature.contains(".<clinit>(") {
            return Err(ConfigurationError(
                "host policy cannot allow constructors or class initializers".to_owned(),
            ));
        }
        self.with_host_object_rules(object, |rules| {
            rules.allowed_methods.insert(signature);
        })
    }

    /// Allow one exact method declared by this object's class, a superclass, or an interface.
    ///
    /// This keeps the permission tied to this one object and checks that the declared owner is in
    /// its loaded class hierarchy. Constructors and class initializers remain forbidden.
    ///
    /// # Errors
    /// Returns an error if the value is not an object, the signature is invalid, or its owner is
    /// outside the object's loaded class hierarchy.
    pub fn allow_inherited_method_on_host_object(
        &self,
        object: &Value,
        signature: impl AsRef<str>,
    ) -> Result<()> {
        let class = object_class(object)?;
        let signature = normalize_method_signature(signature.as_ref())?;
        let owner = method_owner(&signature).ok_or_else(|| {
            ConfigurationError(format!("invalid host method signature: {signature}"))
        })?;
        if !class_hierarchy_contains(&class, owner)? {
            return Err(ConfigurationError(format!(
                "host method owner {owner} is not in the class hierarchy of {}",
                class.name()
            )));
        }
        if signature.contains(".<init>(") || signature.contains(".<clinit>(") {
            return Err(ConfigurationError(
                "host policy cannot allow constructors or class initializers".to_owned(),
            ));
        }
        self.with_host_object_rules(object, |rules| {
            rules.allowed_methods.insert(signature);
        })
    }

    /// Allow one exact direct field read on one non-null host object.
    ///
    /// The signature uses an internal class name, a dot, the field name, a colon, and its JVM
    /// descriptor, such as `example/Host.value:I`. Its owner must be the object's runtime class.
    /// This grants no field writes and says nothing about reflection or `Unsafe` access. The
    /// registration keeps a strong copy of the object value for the policy's lifetime.
    ///
    /// # Errors
    /// Returns an error if the value is not a non-null object or the signature is invalid.
    pub fn allow_field_read_on_host_object(
        &self,
        object: &Value,
        signature: impl AsRef<str>,
    ) -> Result<()> {
        let class_name = object_class_name(object)?;
        let signature = normalize_field_signature(signature.as_ref())?;
        let owner = field_owner(&signature).ok_or_else(|| {
            ConfigurationError(format!("invalid host field signature: {signature}"))
        })?;
        if owner != class_name {
            return Err(ConfigurationError(format!(
                "host field owner {owner} does not match object class {class_name}"
            )));
        }
        self.with_host_object_rules(object, |rules| {
            rules.readable_fields.insert(signature);
        })
    }

    /// Allow one exact field read declared by this object's class or a superclass.
    ///
    /// The permission is tied to this object identity. The host VM still resolves the field and
    /// checks its actual declaring class before consulting this rule.
    ///
    /// # Errors
    /// Returns an error if the value is not an object, the signature is invalid, or its owner is
    /// outside the object's loaded class hierarchy.
    pub fn allow_inherited_field_read_on_host_object(
        &self,
        object: &Value,
        signature: impl AsRef<str>,
    ) -> Result<()> {
        let class = object_class(object)?;
        let signature = normalize_field_signature(signature.as_ref())?;
        let owner = field_owner(&signature).ok_or_else(|| {
            ConfigurationError(format!("invalid host field signature: {signature}"))
        })?;
        if !class_hierarchy_contains(&class, owner)? {
            return Err(ConfigurationError(format!(
                "host field owner {owner} is not in the class hierarchy of {}",
                class.name()
            )));
        }
        self.with_host_object_rules(object, |rules| {
            rules.readable_fields.insert(signature);
        })
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

    /// Returns the protected class name for a blocked direct field instruction, if any.
    pub(crate) fn blocked_field_class<'a>(
        &self,
        owner_class_name: &'a str,
        receiver_class_name: Option<&'a str>,
    ) -> Option<&'a str> {
        self.blocked_class(owner_class_name, receiver_class_name)
    }

    /// Whether this exact method is allowed for this exact object identity.
    pub(crate) fn method_allowed_on_host_object(&self, object: &Value, signature: &str) -> bool {
        self.host_objects
            .read()
            .ok()
            .and_then(|host_objects| {
                host_objects
                    .iter()
                    .find(|rules| same_object(&rules.object, object))
                    .map(|rules| rules.allowed_methods.contains(signature))
            })
            .unwrap_or(false)
    }

    /// Whether this exact field may be read from this exact object identity.
    pub(crate) fn field_read_allowed_on_host_object(
        &self,
        object: &Value,
        signature: &str,
    ) -> bool {
        self.host_objects
            .read()
            .ok()
            .and_then(|host_objects| {
                host_objects
                    .iter()
                    .find(|rules| same_object(&rules.object, object))
                    .map(|rules| rules.readable_fields.contains(signature))
            })
            .unwrap_or(false)
    }

    fn with_host_object_rules(
        &self,
        object: &Value,
        update: impl FnOnce(&mut HostObjectRules),
    ) -> Result<()> {
        let mut host_objects = self
            .host_objects
            .write()
            .map_err(|error| PoisonedLock(error.to_string()))?;
        if let Some(rules) = host_objects
            .iter_mut()
            .find(|rules| same_object(&rules.object, object))
        {
            update(rules);
        } else {
            let mut rules = HostObjectRules {
                object: object.clone(),
                allowed_methods: HashSet::new(),
                readable_fields: HashSet::new(),
            };
            update(&mut rules);
            host_objects.push(rules);
        }
        Ok(())
    }

    fn class_name_is_denied(&self, class_name: &str) -> bool {
        self.denied_class_prefixes
            .iter()
            .any(|prefix| class_name.starts_with(prefix))
    }
}

impl PartialEq for MethodExecutionPolicy {
    fn eq(&self, other: &Self) -> bool {
        if self.denied_class_prefixes != other.denied_class_prefixes {
            return false;
        }
        if Arc::ptr_eq(&self.host_objects, &other.host_objects) {
            return true;
        }
        let (Ok(left), Ok(right)) = (self.host_objects.read(), other.host_objects.read()) else {
            return false;
        };
        left.len() == right.len()
            && left.iter().all(|left_rule| {
                right.iter().any(|right_rule| {
                    same_object(&left_rule.object, &right_rule.object)
                        && left_rule.allowed_methods == right_rule.allowed_methods
                        && left_rule.readable_fields == right_rule.readable_fields
                })
            })
    }
}

impl Eq for MethodExecutionPolicy {}

fn object_class_name(object: &Value) -> Result<String> {
    match object {
        Value::Object(Some(reference)) => reference.read().class_name().map_err(crate::Error::from),
        _ => Err(ConfigurationError(
            "host policy requires a non-null Java object".to_owned(),
        )),
    }
}

fn object_class(object: &Value) -> Result<Arc<Class>> {
    match object {
        Value::Object(Some(_)) => object
            .as_object_ref()
            .map(|object| object.class().clone())
            .map_err(crate::Error::from),
        _ => Err(ConfigurationError(
            "host policy requires a non-null Java object".to_owned(),
        )),
    }
}

fn class_hierarchy_contains(class: &Arc<Class>, owner: &str) -> Result<bool> {
    const MAX_HIERARCHY_CLASSES: usize = 65_536;

    let mut pending = vec![class.clone()];
    let mut scheduled = HashSet::new();
    scheduled.insert(Arc::as_ptr(class) as usize);
    while let Some(class) = pending.pop() {
        if class.name() == owner {
            return Ok(true);
        }
        let mut related = class.interfaces()?;
        if let Some(parent) = class.parent()? {
            related.push(parent);
        }
        for related_class in related {
            let identity = Arc::as_ptr(&related_class) as usize;
            if scheduled.contains(&identity) {
                continue;
            }
            if scheduled.len() >= MAX_HIERARCHY_CLASSES {
                return Err(ConfigurationError(format!(
                    "host policy class hierarchy exceeds {MAX_HIERARCHY_CLASSES} classes"
                )));
            }
            scheduled.insert(identity);
            pending.push(related_class);
        }
    }
    Ok(false)
}

fn same_object(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(Some(left)), Value::Object(Some(right))) => left.ptr_eq(right),
        _ => false,
    }
}

fn normalize_method_signature(signature: &str) -> Result<String> {
    let Some((owner, method)) = signature.split_once('.') else {
        return Err(ConfigurationError(format!(
            "invalid host method signature: {signature}"
        )));
    };
    let Some((method_name, descriptor)) = method.split_once('(') else {
        return Err(ConfigurationError(format!(
            "invalid host method signature: {signature}"
        )));
    };
    let Some((_, return_type)) = descriptor.split_once(')') else {
        return Err(ConfigurationError(format!(
            "invalid host method signature: {signature}"
        )));
    };
    if owner.is_empty() || method_name.is_empty() || return_type.is_empty() {
        return Err(ConfigurationError(format!(
            "invalid host method signature: {signature}"
        )));
    }
    Ok(format!("{}.{method}", owner.replace('.', "/")))
}

fn method_owner(signature: &str) -> Option<&str> {
    signature.split_once('.').map(|(owner, _)| owner)
}

fn normalize_field_signature(signature: &str) -> Result<String> {
    let Some((owner, field)) = signature.split_once('.') else {
        return Err(ConfigurationError(format!(
            "invalid host field signature: {signature}"
        )));
    };
    let Some((name, descriptor)) = field.split_once(':') else {
        return Err(ConfigurationError(format!(
            "invalid host field signature: {signature}"
        )));
    };
    if owner.is_empty() || name.is_empty() || descriptor.is_empty() {
        return Err(ConfigurationError(format!(
            "invalid host field signature: {signature}"
        )));
    }
    Ok(format!("{}.{name}:{descriptor}", owner.replace('.', "/")))
}

fn field_owner(signature: &str) -> Option<&str> {
    signature.split_once('.').map(|(owner, _)| owner)
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
