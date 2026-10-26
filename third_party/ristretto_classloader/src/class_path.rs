use crate::Error::ClassNotFound;
use crate::Result;
use crate::class_path_entry::ClassPathEntry;
use ristretto_classfile::ClassFile;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::Path;
use tracing::info;
use url::Url;

/// Represents a class path.
///
/// The class path is a list of directories and JAR files that contain class files.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassPath {
    class_path: Vec<ClassPathEntry>,
}

/// Implementation for `ClassPath`.
impl ClassPath {
    /// Creates a new `ClassPath` with the given class path.
    #[must_use]
    pub fn new(class_path: Vec<ClassPathEntry>) -> Self {
        ClassPath { class_path }
    }

    /// Creates a new `ClassPath` from a string.
    ///
    /// # Errors
    ///
    /// if the class path is not valid.
    pub fn from<S>(class_path: &[S]) -> Self
    where
        S: AsRef<OsStr>,
    {
        let mut class_path_entries = Vec::new();
        for class_path in class_path {
            let class_path = class_path.as_ref();
            #[cfg(feature = "url")]
            let class_path = {
                let class_path = class_path.to_string_lossy().to_string();
                let class_path = class_path
                    .replace("http//", "http://")
                    .replace("https//", "https://");
                std::ffi::OsString::from(class_path)
            };
            class_path_entries.push(ClassPathEntry::new(class_path));
        }
        ClassPath::new(class_path_entries)
    }

    /// Returns an iterator over the class path.
    pub fn iter(&self) -> impl Iterator<Item = &ClassPathEntry> {
        self.class_path.iter()
    }

    /// Read a class from the class path.
    ///
    /// # Errors
    ///
    /// if the class file is not found or cannot be read.
    pub async fn read_class<S: AsRef<str>>(&self, name: S) -> Result<ClassFile<'static>> {
        self.read_class_with_code_source(name, true).await
    }

    /// Read a class, optionally resolving its code source URL.
    ///
    /// Root class loaders omit code sources, so they can skip the filesystem queries used to
    /// construct a URL that would otherwise be discarded.
    pub(crate) async fn read_class_with_code_source<S: AsRef<str>>(
        &self,
        name: S,
        include_code_source: bool,
    ) -> Result<ClassFile<'static>> {
        let name = name.as_ref();

        for class_path_entry in self.iter() {
            if let Ok(mut class_file) = class_path_entry.read_class(name).await {
                let source = class_path_entry.name().to_string_lossy();
                info!("load class {name} source: {source}");
                if !include_code_source {
                    class_file.code_source_url = None;
                    return Ok(class_file);
                }
                let code_source_url = class_path_code_source_url(class_path_entry);
                class_file.code_source_url = code_source_url;
                return Ok(class_file);
            }
        }

        Err(ClassNotFound(name.to_string()))
    }

    /// Read a resource from the class path.
    ///
    /// # Errors
    ///
    /// if a matching resource cannot be read.
    pub async fn read_resource<S: AsRef<str>>(
        &self,
        module: Option<&str>,
        name: S,
    ) -> Result<Option<Vec<u8>>> {
        let name = name.as_ref();
        for class_path_entry in self.iter() {
            if let Some(bytes) = class_path_entry.read_resource(module, name).await? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Get the class names in the class path.
    ///
    /// # Errors
    ///
    /// if the class names cannot be read.
    pub async fn class_names(&self) -> Result<Vec<String>> {
        let mut classes = Vec::new();
        for class_path_entry in self.iter() {
            let class_names = class_path_entry.class_names().await?;
            classes.extend(class_names);
        }
        classes.sort();
        Ok(classes)
    }
}

fn class_path_code_source_url(entry: &ClassPathEntry) -> Option<String> {
    match entry {
        ClassPathEntry::Directory(directory) => file_path_url(directory.name(), true),
        ClassPathEntry::Image(image) => file_path_url(image.name(), false),
        ClassPathEntry::Jar(jar) => {
            let name = jar.name().to_string_lossy();
            if name.starts_with("http://") || name.starts_with("https://") {
                Some(name.into_owned())
            } else {
                file_path_url(jar.name(), false)
            }
        }
        // A synthetic protocol would be rejected by java.net.URL because no
        // corresponding protocol handler exists.
        ClassPathEntry::Memory(_) => None,
    }
}

fn file_path_url(path: &OsStr, is_directory: bool) -> Option<String> {
    let path = Path::new(path);
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(_) if path.is_absolute() => path.to_path_buf(),
        Err(_) => std::env::current_dir().ok()?.join(path),
    };
    let is_directory = is_directory || path.is_dir();
    let mut url = Url::from_file_path(path).ok()?;
    if is_directory && !url.path().ends_with('/') {
        url.path_segments_mut().ok()?.push("");
    }
    Some(url.into())
}

/// Into iterator for `ClassPath`.
impl IntoIterator for ClassPath {
    type Item = ClassPathEntry;
    type IntoIter = std::vec::IntoIter<ClassPathEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.class_path.into_iter()
    }
}

impl Display for ClassPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(target_os = "windows")]
        let path_separator: &str = ";";
        #[cfg(not(target_os = "windows"))]
        let path_separator: &str = ":";

        let class_path = self
            .class_path
            .iter()
            .map(|entry| entry.name().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(path_separator);
        write!(f, "{class_path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;
    use std::path::PathBuf;

    #[cfg(target_os = "windows")]
    const PATH_SEPARATOR: &str = ";";
    #[cfg(not(target_os = "windows"))]
    const PATH_SEPARATOR: &str = ":";

    #[test]
    fn test_new() {
        let class_path = ClassPath::new(vec![ClassPathEntry::new("."), ClassPathEntry::new("..")]);
        assert_eq!(format!(".{PATH_SEPARATOR}.."), class_path.to_string());
    }

    #[test]
    fn test_from() {
        let class_path = ClassPath::from(&[".", ".."]);
        assert_eq!(format!(".{PATH_SEPARATOR}.."), class_path.to_string());
    }

    #[test]
    fn test_iter() {
        let class_path = ClassPath::from(&[".", ".."]);
        let mut iter = class_path.iter();
        assert_eq!(
            ".",
            iter.next()
                .expect("next")
                .name()
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(
            "..",
            iter.next()
                .expect("next")
                .name()
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn test_into_iter() {
        let class_path = ClassPath::from(&[".", ".."]);
        let mut iter = class_path.into_iter();
        assert_eq!(
            ".",
            iter.next()
                .expect("next")
                .name()
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(
            "..",
            iter.next()
                .expect("next")
                .name()
                .to_string_lossy()
                .to_string()
        );
    }

    #[tokio::test]
    async fn test_read_class() -> Result<()> {
        let cargo_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let classes_directory = cargo_manifest.join("..").join("classes");
        let classes_jar = cargo_manifest
            .join("..")
            .join("classes")
            .join("classes.jar");

        let class_path_entries = [
            classes_directory.to_string_lossy().to_string(),
            classes_jar.to_string_lossy().to_string(),
            #[cfg(feature = "url")]
            "https//repo1.maven.org/maven2/org/springframework/boot/spring-boot/3.3.0/spring-boot-3.3.0.jar".to_string(),
        ];

        let class_path_entry = ClassPath::from(&class_path_entries);

        let class_file = class_path_entry.read_class("HelloWorld").await?;
        assert_eq!("HelloWorld", class_file.class_name()?);
        assert!(
            class_file
                .code_source_url
                .as_deref()
                .is_some_and(|url| url.starts_with("file:") && url.ends_with('/'))
        );

        #[cfg(feature = "url")]
        {
            let class_file = class_path_entry
                .read_class("org/springframework/boot/SpringApplication")
                .await?;
            assert_eq!(
                "org/springframework/boot/SpringApplication",
                class_file.class_name()?
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_read_class_from_memory() -> Result<()> {
        let memory = crate::Memory::new("generated");
        memory
            .add_class(include_bytes!("../../classes/Minimum.class"))
            .await?;
        let class_path = ClassPath::new(vec![ClassPathEntry::Memory(memory)]);

        let class_file = class_path.read_class("Minimum").await?;

        assert_eq!("Minimum", class_file.class_name()?);
        assert_eq!(None, class_file.code_source_url);
        Ok(())
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn test_file_urls_escape_special_path_characters() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("path with spaces-雪-#-%");
        std::fs::create_dir_all(&directory).unwrap();
        let directory_url = file_path_url(directory.as_os_str(), true).unwrap();
        let parsed_directory_url = Url::parse(&directory_url).unwrap();

        assert!(directory_url.starts_with("file:"));
        assert!(directory_url.contains("%20"));
        assert!(directory_url.contains("%E9%9B%AA"));
        assert!(directory_url.contains("%23"));
        assert!(directory_url.contains("%25"));
        assert!(directory_url.ends_with('/'));
        assert_eq!(
            parsed_directory_url.to_file_path().unwrap(),
            directory.canonicalize().unwrap()
        );

        let jar = directory.join("mod file-雪-#-%.jar");
        std::fs::write(&jar, b"fixture").unwrap();
        let jar_url = file_path_url(jar.as_os_str(), false).unwrap();
        let parsed_jar_url = Url::parse(&jar_url).unwrap();
        assert!(!jar_url.ends_with('/'));
        assert_eq!(
            parsed_jar_url.to_file_path().unwrap(),
            jar.canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn test_read_resource() -> Result<()> {
        let cargo_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let missing_directory = cargo_manifest.join("missing");
        let classes_directory = cargo_manifest.join("..").join("classes");
        let class_path = ClassPath::new(vec![
            ClassPathEntry::new(missing_directory),
            ClassPathEntry::new(classes_directory),
        ]);
        assert!(
            class_path
                .read_resource(None, "HelloWorld.class")
                .await?
                .is_some()
        );
        assert!(
            class_path
                .read_resource(None, "missing.resource")
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_class_names() -> Result<()> {
        let cargo_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let classes_directory = cargo_manifest.join("..").join("classes");
        let classes_jar = cargo_manifest
            .join("..")
            .join("classes")
            .join("classes.jar");

        let class_path_entries = [
            classes_directory.to_string_lossy().to_string(),
            classes_jar.to_string_lossy().to_string(),
            #[cfg(feature = "url")]
            "https//repo1.maven.org/maven2/org/springframework/boot/spring-boot/3.3.0/spring-boot-3.3.0.jar".to_string(),
        ];

        let class_path_entry = ClassPath::from(&class_path_entries);

        let class_names = class_path_entry.class_names().await?;
        assert!(class_names.contains(&"HelloWorld".to_string()));

        #[cfg(feature = "url")]
        {
            assert!(
                class_names.contains(&"org/springframework/boot/SpringApplication".to_string())
            );
        }
        Ok(())
    }
}
