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
        self.scratch = self.root / "scratch"
        self.output = self.scratch / "home"
        for name, value in [("SCRATCH_ROOT", self.scratch), ("OUTPUT", self.output)]:
            replacement = patch.object(prepare, name, value)
            replacement.start()
            self.addCleanup(replacement.stop)

    def test_copies_library_without_changing_source(self):
        with patch.object(prepare, "run_tool", return_value="classes/java/lang/ThreadGroup.class\n"):
            prepare.prepare(self.source, False)
        self.assertEqual((self.output / "jmods/java.base.jmod").read_bytes(), b"fixture library")
        self.assertEqual((self.source / "jmods/java.base.jmod").read_bytes(), b"fixture library")
        self.assertFalse((self.output / "lib/modules").exists())
        with self.assertRaisesRegex(RuntimeError, "already exists"):
            prepare.prepare(self.source, False)

    def test_failed_validation_keeps_old_output(self):
        self.output.mkdir(parents=True)
        (self.output / "keep").write_text("previous library")
        with patch.object(prepare, "run_tool", return_value=""):
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
