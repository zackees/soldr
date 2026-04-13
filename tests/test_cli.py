"""
Unit test file.
"""

import os
import unittest

COMMAND = "soldr"


class MainTester(unittest.TestCase):
    """Main tester class."""

    @unittest.skip("TODO")
    def test_imports(self) -> None:
        """Test command line interface (CLI)."""
        rtn = os.system(COMMAND)
        self.assertEqual(0, rtn)


if __name__ == "__main__":
    unittest.main()
