import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import sandbox_profile_paths


class SandboxProfilePathsTests(unittest.TestCase):
    def test_scheme_escape_handles_backslashes_and_quotes(self) -> None:
        self.assertEqual(
            sandbox_profile_paths.scheme_escape('a\\b"c'),
            'a\\\\b\\"c',
        )

    def test_literal_form_resolves_parent_symlinks_and_escapes_name(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            real = root / "real"
            real.mkdir()
            alias = root / "alias"
            alias.symlink_to(real, target_is_directory=True)

            form = sandbox_profile_paths.literal_form(alias / 'file"name')
            expected = real.resolve() / 'file"name'
            self.assertEqual(
                form,
                f'(literal "{sandbox_profile_paths.scheme_escape(str(expected))}")',
            )

    def test_subpath_form_creates_and_canonicalizes_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "new" / "work"
            form = sandbox_profile_paths.subpath_form(path)
            self.assertTrue(path.is_dir())
            self.assertEqual(form, f'(subpath "{path.resolve()}")')


if __name__ == "__main__":
    unittest.main()
