import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools import prepare_ristretto_java_home as prepare


class JavaHomeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "jdk"
        self.source.mkdir()
        (self.source / "release").write_text('JAVA_VERSION="25.0.1"\n')
        (self.source / "jmods").mkdir()
        (self.source / "jmods/java.base.jmod").write_bytes(b"fixture library")
        (self.source / "jmods/java.sql.jmod").write_bytes(b"fixture SQL library")
        (self.source / "lib").mkdir()
        (self.source / "lib/tzdb.dat").write_bytes(b"fixture time-zone database")
        (self.source / "conf/security").mkdir(parents=True)
        (self.source / "conf/security/java.security").write_text("fixture security settings")
        self.scratch = self.root / "scratch"
        self.output = self.scratch / "home"
        self.extended_output = self.scratch / "fabric-home"
        for name, value in [("SCRATCH_ROOT", self.scratch), ("OUTPUT", self.output)]:
            replacement = patch.object(prepare, name, value)
            replacement.start()
            self.addCleanup(replacement.stop)

    def jmod_tool(self, tool, *arguments):
        if arguments[0] == "describe":
            module = Path(arguments[1]).stem
            return f"{module}@25.0.1\nrequires java.base mandated\n"
        if arguments[0] == "list":
            if Path(arguments[1]).stem == "java.base":
                return "classes/java/lang/ThreadGroup.class\n"
            return "classes/java/sql/Driver.class\n"
        raise AssertionError(f"unexpected JDK tool call: {tool} {arguments}")

    def test_copies_library_without_changing_source(self):
        with patch.object(prepare, "run_tool", side_effect=self.jmod_tool):
            prepare.prepare(self.source, False)
        self.assertEqual((self.output / "jmods/java.base.jmod").read_bytes(), b"fixture library")
        self.assertEqual(
            sorted(path.name for path in (self.output / "jmods").iterdir()),
            ["java.base.jmod"],
        )
        self.assertEqual((self.output / "lib/tzdb.dat").read_bytes(), b"fixture time-zone database")
        self.assertEqual((self.source / "jmods/java.base.jmod").read_bytes(), b"fixture library")
        self.assertEqual((self.source / "lib/tzdb.dat").read_bytes(), b"fixture time-zone database")
        self.assertFalse((self.output / "lib/modules").exists())
        self.assertFalse((self.output / "conf/security/java.security").exists())
        with self.assertRaisesRegex(RuntimeError, "already exists"):
            prepare.prepare(self.source, False)

    def test_failed_validation_keeps_old_output(self):
        self.output.mkdir(parents=True)
        (self.output / "keep").write_text("previous library")
        def missing_thread_group(tool, *arguments):
            if arguments[0] == "describe":
                return f"{Path(arguments[1]).stem}@25.0.1\n"
            return ""

        with patch.object(prepare, "run_tool", side_effect=missing_thread_group):
            with self.assertRaisesRegex(RuntimeError, "missing ThreadGroup"):
                prepare.prepare(self.source, True)
        self.assertEqual((self.output / "keep").read_text(), "previous library")
        self.assertEqual(list(self.scratch.iterdir()), [self.output])

    def test_refuses_output_symlink_even_with_force(self):
        self.scratch.mkdir()
        self.output.symlink_to(self.source, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            prepare.prepare(self.source, True)
        self.assertTrue((self.source / "release").is_file())

    def test_refuses_to_delete_a_nested_source(self):
        nested = self.output / "jdk"
        nested.mkdir(parents=True)
        (nested / "release").write_text('JAVA_VERSION="25.0.1"\n')
        with self.assertRaisesRegex(RuntimeError, "contain each other"):
            prepare.prepare(nested, True)
        self.assertTrue((nested / "release").is_file())

    def test_rejects_other_java_versions(self):
        (self.source / "release").write_text('JAVA_VERSION="21.0.1"\n')
        with self.assertRaisesRegex(RuntimeError, "expected a Java 25"):
            prepare.prepare(self.source, False)
        self.assertFalse(self.output.exists())

    def test_normalizes_and_rejects_module_lists(self):
        self.assertEqual(
            prepare.normalize_modules("java.base, java.sql,java.transaction.xa"),
            ("java.base", "java.sql", "java.transaction.xa"),
        )
        for invalid in (
            "",
            "java.base,,java.sql",
            "java.base,java.base",
            "java.sql",
            "java.base,../java.sql",
            "java.base,java\\sql",
        ):
            with self.subTest(invalid=invalid), self.assertRaises(RuntimeError):
                prepare.normalize_modules(invalid)

    def test_extended_home_copies_only_requested_jmods_and_security_settings(self):
        with patch.object(prepare, "run_tool", side_effect=self.jmod_tool):
            prepare.prepare(
                self.source,
                False,
                modules="java.base,java.sql",
                output=self.extended_output,
            )
        self.assertEqual(
            sorted(path.name for path in (self.extended_output / "jmods").iterdir()),
            ["java.base.jmod", "java.sql.jmod"],
        )
        self.assertEqual(
            (self.extended_output / "conf/security/java.security").read_text(),
            "fixture security settings",
        )
        self.assertFalse(self.output.exists())
        self.assertEqual((self.source / "jmods/java.sql.jmod").read_bytes(), b"fixture SQL library")

    def test_extended_modules_need_a_separate_output(self):
        with self.assertRaisesRegex(RuntimeError, "separate --output"):
            prepare.prepare(self.source, True, modules=("java.base", "java.sql"))
        self.assertFalse(self.output.exists())

    def test_missing_runtime_dependency_keeps_previous_home(self):
        self.extended_output.mkdir(parents=True)
        (self.extended_output / "keep").write_text("previous complete home")

        def missing_dependency(tool, *arguments):
            result = self.jmod_tool(tool, *arguments)
            if arguments[0] == "describe" and Path(arguments[1]).stem == "java.sql":
                result += "requires java.prefs transitive\n"
            return result

        with patch.object(prepare, "run_tool", side_effect=missing_dependency):
            with self.assertRaisesRegex(RuntimeError, "java.sql needs missing runtime modules: java.prefs"):
                prepare.prepare(
                    self.source,
                    True,
                    modules=("java.base", "java.sql"),
                    output=self.extended_output,
                )
        self.assertEqual((self.extended_output / "keep").read_text(), "previous complete home")
        self.assertEqual(list(self.scratch.iterdir()), [self.extended_output])

    def test_runtime_dependencies_include_transitive_but_not_static_modules(self):
        self.assertEqual(
            prepare.required_modules(
                "example.module@1\nrequires java.base mandated\n"
                "requires java.xml transitive\nrequires java.compiler static\n"
                "requires optional.api static transitive\nexports example.module\n"
            ),
            {"java.base", "java.xml"},
        )

    def test_rejects_a_jmod_whose_descriptor_name_does_not_match(self):
        self.extended_output.mkdir(parents=True)
        (self.extended_output / "keep").write_text("previous extended home")

        def wrong_descriptor(tool, *arguments):
            if arguments[0] == "describe":
                return "java.other@25.0.1\n"
            return "classes/java/lang/ThreadGroup.class\n"

        with patch.object(prepare, "run_tool", side_effect=wrong_descriptor):
            with self.assertRaisesRegex(RuntimeError, "name does not match"):
                prepare.prepare(
                    self.source,
                    True,
                    modules=("java.base", "java.sql"),
                    output=self.extended_output,
                )
        self.assertEqual((self.extended_output / "keep").read_text(), "previous extended home")
        self.assertEqual(list(self.scratch.iterdir()), [self.extended_output])

    def test_image_only_home_extracts_only_requested_modules(self):
        shutil.rmtree(self.source / "jmods")
        (self.source / "lib/modules").write_bytes(b"fixture module image")
        image_listing = """\
jimage: fixture

Module: java.base
    module-info.class
    java/lang/Object.class
    java/lang/ThreadGroup.class

Module: java.sql
    module-info.class
    java/sql/Driver.class

Module: java.desktop
    module-info.class
    java/awt/Button.class
"""
        calls = []

        def image_tool(tool, *arguments):
            calls.append((tool.name, arguments))
            if tool.name == "jimage" and arguments[0] == "list":
                return image_listing
            if tool.name == "jimage" and arguments[0] == "extract":
                self.assertIn("--include", arguments)
                patterns = arguments[arguments.index("--include") + 1]
                self.assertIn("glob:**/java/lang/**", patterns)
                self.assertIn("glob:**/java/sql/**", patterns)
                self.assertNotIn("glob:**/java/awt/**", patterns)
                dir_argument = next(
                    argument for argument in arguments if argument.startswith("--dir=")
                )
                extracted = Path(dir_argument.partition("=")[2])
                for module, resource in (
                    ("java.base", "java/lang/ThreadGroup.class"),
                    ("java.sql", "java/sql/Driver.class"),
                    ("java.desktop", "java/awt/Button.class"),
                ):
                    module_dir = extracted / module
                    (module_dir / "module-info.class").parent.mkdir(parents=True, exist_ok=True)
                    (module_dir / "module-info.class").write_bytes(b"module descriptor")
                    file = module_dir / resource
                    file.parent.mkdir(parents=True, exist_ok=True)
                    file.write_bytes(b"module class")
                return ""
            if tool.name == "jmod" and arguments[0] == "create":
                Path(arguments[-1]).write_bytes(b"rebuilt JMOD")
                return ""
            if tool.name == "jmod" and arguments[0] == "describe":
                return f"{Path(arguments[1]).stem}@25.0.1\n"
            if tool.name == "jmod" and arguments[0] == "list":
                return "classes/java/lang/ThreadGroup.class\n"
            raise AssertionError(f"unexpected JDK tool call: {tool} {arguments}")

        with patch.object(prepare, "run_tool", side_effect=image_tool):
            prepare.prepare(
                self.source,
                False,
                modules=("java.base", "java.sql"),
                output=self.extended_output,
            )

        self.assertEqual(
            sorted(path.name for path in (self.extended_output / "jmods").iterdir()),
            ["java.base.jmod", "java.sql.jmod"],
        )
        self.assertFalse((self.extended_output / "jmods/java.desktop.jmod").exists())
        self.assertTrue((self.extended_output / "conf/security/java.security").is_file())
        extract_calls = [
            arguments
            for name, arguments in calls
            if name == "jimage" and arguments[0] == "extract"
        ]
        self.assertEqual(len(extract_calls), 1)

    def test_rejects_modules_missing_from_both_jmods_and_image(self):
        self.extended_output.mkdir(parents=True)
        (self.extended_output / "keep").write_text("previous extended library")
        with self.assertRaisesRegex(RuntimeError, "missing requested JMODs: java.unknown"):
            prepare.prepare(
                self.source,
                True,
                modules=("java.base", "java.unknown"),
                output=self.extended_output,
            )
        self.assertEqual((self.extended_output / "keep").read_text(), "previous extended library")
